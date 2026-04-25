//! SQLite-backed persistence for [`Scratchpad`].
//!
//! The store lives in the same `~/.basilisk/sessions.db` file as the
//! agent's session tables so one open call covers both. Callers
//! interact through [`ScratchpadStore`]; the agent's `SessionStore`
//! invokes [`apply_schema`] as part of its own migration.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};

use crate::{
    error::ScratchpadError,
    types::{Scratchpad, Section, SectionKey},
};

/// Cap on revisions retained per session. Older entries get pruned
/// on every save so the table doesn't grow without bound.
pub const REVISION_CAP_PER_SESSION: usize = 100;

/// Embedded schema text. Exposed so the agent's session-store
/// migration can run it alongside its own tables.
pub const SCRATCHPAD_SCHEMA_SQL: &str = include_str!("schema.sql");

/// Apply the scratchpad schema to an open `SQLite` connection.
/// Idempotent — running twice is a no-op. Called by the agent's
/// `SessionStore::apply_schema` as part of the v3 migration.
///
/// # Errors
///
/// Returns [`ScratchpadError::Sqlite`] if the `CREATE TABLE` batch
/// fails.
pub fn apply_schema(conn: &Connection) -> Result<(), ScratchpadError> {
    conn.execute_batch(SCRATCHPAD_SCHEMA_SQL)?;
    Ok(())
}

/// Short description of a stored scratchpad — returned by
/// [`ScratchpadStore::list_sessions`] for the CLI listing path.
#[derive(Debug, Clone)]
pub struct ScratchpadSummary {
    pub session_id: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub item_counts: BTreeMap<SectionKey, usize>,
    pub total_size_bytes: usize,
}

/// Persistence adapter. Cheap to clone — the underlying connection
/// is `Arc<Mutex<..>>`-shared.
#[derive(Clone)]
pub struct ScratchpadStore {
    inner: Arc<Inner>,
}

struct Inner {
    conn: Mutex<Connection>,
}

impl ScratchpadStore {
    /// Open (or create) the store at `db_path`. Runs
    /// [`apply_schema`] so fresh DB files come up ready to use.
    ///
    /// # Errors
    ///
    /// Returns [`ScratchpadError::Sqlite`] on any `SQLite` failure.
    pub fn open(db_path: &Path) -> Result<Self, ScratchpadError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ScratchpadError::Storage(format!("mkdir: {e}")))?;
        }
        let conn = Connection::open(db_path)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        apply_schema(&conn)?;
        Ok(Self {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
            }),
        })
    }

    /// Open an in-memory store — used by tests so we don't need a
    /// tempfile for every round-trip check.
    ///
    /// # Errors
    ///
    /// Same as [`Self::open`].
    pub fn open_in_memory() -> Result<Self, ScratchpadError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        // Without the `sessions` table, our FK on scratchpads would
        // reject inserts. Create a minimal stub so in-memory tests
        // can exercise the real INSERT path.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY);",
        )?;
        apply_schema(&conn)?;
        Ok(Self {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
            }),
        })
    }

    fn conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, ScratchpadError> {
        self.inner
            .conn
            .lock()
            .map_err(|_| ScratchpadError::Storage("connection lock poisoned".into()))
    }

    /// Create + persist a fresh scratchpad for the given session.
    /// Errors if a scratchpad already exists for this session —
    /// callers that want "create-or-load" use [`Self::load`] first.
    ///
    /// # Errors
    ///
    /// Returns [`ScratchpadError::Sqlite`] when the session row is
    /// missing (FK violation) or the scratchpad already exists.
    pub fn create(&self, session_id: &str) -> Result<Scratchpad, ScratchpadError> {
        let sp = Scratchpad::new(session_id);
        self.save(&sp)?;
        Ok(sp)
    }

    /// Load the current state for a session. Returns `None` when
    /// the session has no scratchpad yet (pre-initialisation).
    ///
    /// # Errors
    ///
    /// Returns [`ScratchpadError::Sqlite`] / [`ScratchpadError::Serde`]
    /// on storage or deserialisation failure.
    pub fn load(&self, session_id: &str) -> Result<Option<Scratchpad>, ScratchpadError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT schema_version, created_at_ms, updated_at_ms, sections_json, next_item_id
                 FROM scratchpads WHERE session_id = ?1",
                params![session_id],
                |r| {
                    Ok((
                        r.get::<_, u32>(0)?,
                        r.get::<_, u64>(1)?,
                        r.get::<_, u64>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, u64>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((schema_version, created_at_ms, updated_at_ms, sections_json, next_item_id)) = row
        else {
            return Ok(None);
        };
        let sections: BTreeMap<SectionKey, Section> = serde_json::from_str(&sections_json)?;
        Ok(Some(Scratchpad {
            session_id: session_id.into(),
            schema_version,
            created_at_ms,
            updated_at_ms,
            sections,
            next_item_id,
        }))
    }

    /// Upsert the scratchpad's current state and append a row to
    /// `scratchpad_revisions`. Retains the
    /// [`REVISION_CAP_PER_SESSION`] most-recent revisions.
    ///
    /// # Errors
    ///
    /// Returns [`ScratchpadError::Sqlite`] / [`ScratchpadError::Serde`]
    /// on storage or serialisation failure. All writes run inside a
    /// single transaction so a failure leaves the DB untouched.
    pub fn save(&self, scratchpad: &Scratchpad) -> Result<(), ScratchpadError> {
        let sections_json = serde_json::to_string(&scratchpad.sections)?;
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO scratchpads
                 (session_id, schema_version, created_at_ms, updated_at_ms,
                  sections_json, next_item_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id) DO UPDATE SET
                 schema_version = excluded.schema_version,
                 updated_at_ms = excluded.updated_at_ms,
                 sections_json = excluded.sections_json,
                 next_item_id = excluded.next_item_id",
            params![
                scratchpad.session_id,
                scratchpad.schema_version,
                scratchpad.created_at_ms,
                scratchpad.updated_at_ms,
                sections_json,
                scratchpad.next_item_id,
            ],
        )?;

        // Append to revisions with a monotonic index.
        let next_rev: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(revision_index), 0) + 1
                 FROM scratchpad_revisions WHERE session_id = ?1",
                params![scratchpad.session_id],
                |r| r.get(0),
            )
            .unwrap_or(1);
        tx.execute(
            "INSERT INTO scratchpad_revisions
                 (session_id, revision_index, at_ms, sections_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                scratchpad.session_id,
                next_rev,
                scratchpad.updated_at_ms,
                sections_json,
            ],
        )?;

        // Prune anything older than the cap.
        let cap = i64::try_from(REVISION_CAP_PER_SESSION).unwrap_or(100);
        tx.execute(
            "DELETE FROM scratchpad_revisions
             WHERE session_id = ?1
               AND revision_index <= ?2 - ?3",
            params![scratchpad.session_id, next_rev, cap],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Remove the scratchpad (and its revisions) for a session.
    /// No-op if the session had no scratchpad.
    ///
    /// # Errors
    ///
    /// Returns [`ScratchpadError::Sqlite`] on storage failure.
    pub fn delete(&self, session_id: &str) -> Result<(), ScratchpadError> {
        let conn = self.conn()?;
        conn.execute(
            "DELETE FROM scratchpads WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// List all scratchpads in the store, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`ScratchpadError::Sqlite`] / [`ScratchpadError::Serde`]
    /// on query or deserialisation failure.
    pub fn list_sessions(&self, limit: usize) -> Result<Vec<ScratchpadSummary>, ScratchpadError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, created_at_ms, updated_at_ms, sections_json
             FROM scratchpads
             ORDER BY updated_at_ms DESC
             LIMIT ?1",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut rows = stmt.query(params![limit])?;

        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let session_id: String = row.get(0)?;
            let created_at_ms: u64 = row.get(1)?;
            let updated_at_ms: u64 = row.get(2)?;
            let sections_json: String = row.get(3)?;
            let sections: BTreeMap<SectionKey, Section> = serde_json::from_str(&sections_json)?;
            let mut item_counts = BTreeMap::new();
            for (k, s) in &sections {
                let n = match s {
                    Section::Items(i) => i.items.len(),
                    Section::Prose(_) => 0,
                };
                item_counts.insert(k.clone(), n);
            }
            out.push(ScratchpadSummary {
                session_id,
                created_at_ms,
                updated_at_ms,
                item_counts,
                total_size_bytes: sections_json.len(),
            });
        }
        Ok(out)
    }

    /// Load the scratchpad's sections as-of a specific revision
    /// index. Returns `None` if the revision doesn't exist (pruned
    /// or never recorded).
    ///
    /// # Errors
    ///
    /// Returns [`ScratchpadError::Sqlite`] / [`ScratchpadError::Serde`]
    /// on query or deserialisation failure.
    pub fn load_at_revision(
        &self,
        session_id: &str,
        revision_index: i64,
    ) -> Result<Option<BTreeMap<SectionKey, Section>>, ScratchpadError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT sections_json FROM scratchpad_revisions
                 WHERE session_id = ?1 AND revision_index = ?2",
                params![session_id, revision_index],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
        }
    }

    /// List (`revision_index`, `at_ms`) pairs for a session, newest
    /// first. Used by `audit session scratchpad history` and the
    /// `scratchpad_history` agent tool.
    ///
    /// # Errors
    ///
    /// Returns [`ScratchpadError::Sqlite`] on query failure.
    pub fn list_revisions(
        &self,
        session_id: &str,
    ) -> Result<Vec<(i64, u64)>, ScratchpadError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT revision_index, at_ms FROM scratchpad_revisions
             WHERE session_id = ?1 ORDER BY revision_index DESC",
        )?;
        let mut rows = stmt.query(params![session_id])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push((row.get::<_, i64>(0)?, row.get::<_, u64>(1)?));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Item, ItemStatus, ItemsSection, Section};

    fn seed_session(store: &ScratchpadStore, id: &str) {
        let conn = store.conn().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO sessions (id) VALUES (?1)",
            params![id],
        )
        .unwrap();
    }

    #[test]
    fn create_and_load_round_trips() {
        let store = ScratchpadStore::open_in_memory().unwrap();
        seed_session(&store, "s1");
        let sp = store.create("s1").unwrap();
        let back = store.load("s1").unwrap().unwrap();
        assert_eq!(sp, back);
        assert_eq!(back.session_id, "s1");
        assert_eq!(back.sections.len(), 8);
    }

    #[test]
    fn save_persists_mutations() {
        let store = ScratchpadStore::open_in_memory().unwrap();
        seed_session(&store, "s2");
        let mut sp = store.create("s2").unwrap();

        // Directly mutate an items section — full ops API lands in
        // CP8.3; this is a raw storage-layer check.
        let items = sp.sections.get_mut(&SectionKey::Hypotheses).unwrap();
        if let Section::Items(ItemsSection { items, last_updated_ms }) = items {
            items.push(Item {
                id: crate::types::ItemId(1),
                content: "possible reentrancy on withdraw".into(),
                status: ItemStatus::Open,
                tags: vec!["draft".into()],
                created_at_ms: 1,
                updated_at_ms: 1,
                history: vec![],
            });
            *last_updated_ms = 1;
        }
        sp.updated_at_ms = 1;
        sp.next_item_id = 2;
        store.save(&sp).unwrap();

        let loaded = store.load("s2").unwrap().unwrap();
        assert_eq!(loaded.next_item_id, 2);
        assert_eq!(loaded.item_count(), 1);
    }

    #[test]
    fn save_appends_revision_row() {
        let store = ScratchpadStore::open_in_memory().unwrap();
        seed_session(&store, "s3");
        let sp = store.create("s3").unwrap();
        // create() calls save() internally → 1 revision.
        store.save(&sp).unwrap();
        store.save(&sp).unwrap();
        let revs = store.list_revisions("s3").unwrap();
        assert_eq!(revs.len(), 3);
        // Newest-first ordering, indices ascending under the hood.
        assert!(revs[0].0 > revs[1].0);
    }

    #[test]
    fn revision_cap_prunes_oldest() {
        let store = ScratchpadStore::open_in_memory().unwrap();
        seed_session(&store, "s4");
        let sp = store.create("s4").unwrap();
        for _ in 0..(REVISION_CAP_PER_SESSION + 20) {
            store.save(&sp).unwrap();
        }
        let revs = store.list_revisions("s4").unwrap();
        assert_eq!(revs.len(), REVISION_CAP_PER_SESSION);
        // Newest revision index should have advanced far past the cap.
        assert!(revs[0].0 > i64::try_from(REVISION_CAP_PER_SESSION).unwrap());
    }

    #[test]
    fn load_at_revision_returns_historical_state() {
        let store = ScratchpadStore::open_in_memory().unwrap();
        seed_session(&store, "s5");
        let mut sp = store.create("s5").unwrap();
        // first revision: empty
        let rev1 = store.list_revisions("s5").unwrap()[0].0;
        // mutate + save
        sp.updated_at_ms = 42;
        sp.next_item_id = 99;
        store.save(&sp).unwrap();
        let rev2 = store.list_revisions("s5").unwrap()[0].0;
        assert!(rev2 > rev1);
        let original = store.load_at_revision("s5", rev1).unwrap().unwrap();
        let updated = store.load_at_revision("s5", rev2).unwrap().unwrap();
        assert_eq!(original.len(), 8);
        assert_eq!(updated.len(), 8);
    }

    #[test]
    fn delete_cascades_revisions() {
        let store = ScratchpadStore::open_in_memory().unwrap();
        seed_session(&store, "s6");
        let _sp = store.create("s6").unwrap();
        store.save(&store.load("s6").unwrap().unwrap()).unwrap();
        assert_eq!(store.list_revisions("s6").unwrap().len(), 2);
        store.delete("s6").unwrap();
        assert!(store.load("s6").unwrap().is_none());
        assert!(store.list_revisions("s6").unwrap().is_empty());
    }

    #[test]
    fn session_cascade_drops_scratchpad() {
        // Deleting a row in `sessions` should cascade to scratchpads
        // via the FK. This is why required FK = ON.
        let store = ScratchpadStore::open_in_memory().unwrap();
        seed_session(&store, "s7");
        let _sp = store.create("s7").unwrap();
        {
            let conn = store.conn().unwrap();
            conn.execute("DELETE FROM sessions WHERE id = ?1", params!["s7"])
                .unwrap();
        }
        assert!(store.load("s7").unwrap().is_none());
    }

    #[test]
    fn list_sessions_returns_summaries_newest_first() {
        let store = ScratchpadStore::open_in_memory().unwrap();
        seed_session(&store, "a");
        seed_session(&store, "b");
        let mut a = store.create("a").unwrap();
        let mut b = store.create("b").unwrap();
        // Touch b later → it should appear first.
        b.updated_at_ms = 1000;
        store.save(&b).unwrap();
        a.updated_at_ms = 500;
        store.save(&a).unwrap();
        let out = store.list_sessions(10).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].session_id, "b");
        assert_eq!(out[1].session_id, "a");
    }
}
