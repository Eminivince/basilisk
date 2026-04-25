//! SQLite-backed history of benchmark runs.
//!
//! Lives alongside `sessions.db` — uses the same file by default so
//! `audit bench history` can correlate bench runs with the agent
//! sessions they spawned. The `bench_runs` table is additive and
//! idempotent (`IF NOT EXISTS`).

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::{error::BenchError, score::BenchmarkScore};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS bench_runs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    target_id   TEXT NOT NULL,
    session_id  TEXT,
    run_json    TEXT NOT NULL,
    score_json  TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS bench_runs_target_idx
    ON bench_runs (target_id, created_at DESC);

CREATE TABLE IF NOT EXISTS bench_review_verdicts (
    run_id      INTEGER NOT NULL,
    kind        TEXT NOT NULL,        -- 'miss' | 'false_positive'
    label       TEXT NOT NULL,        -- expected.class for miss, agent.title for fp
    verdict     TEXT NOT NULL,        -- 'actual_miss' | 'scoring_failure' | 'false_positive' | 'wrongly_flagged' | 'in_scope_extra'
    note        TEXT,
    reviewed_at INTEGER NOT NULL,
    PRIMARY KEY (run_id, kind, label)
);

CREATE INDEX IF NOT EXISTS bench_review_verdicts_run_idx
    ON bench_review_verdicts (run_id);
";

pub struct BenchStore {
    conn: Arc<Mutex<Connection>>,
}

impl BenchStore {
    /// Open or create the bench-runs DB at `path`. The file is
    /// usually the operator's existing `sessions.db` so the bench
    /// tables sit alongside the session tables.
    pub fn open(path: &Path) -> Result<Self, BenchError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Self, BenchError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Record one bench run + its score. Returns the auto-generated
    /// row id.
    pub fn record(
        &self,
        target_id: &str,
        session_id: Option<&str>,
        run_json: &str,
        score: &BenchmarkScore,
        created_at_ms: i64,
    ) -> Result<i64, BenchError> {
        let score_json = serde_json::to_string(score)?;
        let conn = self
            .conn
            .lock()
            .map_err(|_| BenchError::Other("lock poisoned".into()))?;
        conn.execute(
            "INSERT INTO bench_runs (target_id, session_id, run_json, score_json, created_at)
             VALUES (?, ?, ?, ?, ?)",
            params![target_id, session_id, run_json, score_json, created_at_ms],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// List every run, newest first. Each row carries the short
    /// summary — `run_json` + `score_json` need explicit `load_run`
    /// if you want the full payload.
    pub fn history(&self) -> Result<Vec<BenchHistoryRow>, BenchError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| BenchError::Other("lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT id, target_id, session_id, created_at, score_json
             FROM bench_runs
             ORDER BY created_at DESC",
        )?;
        let rows: Vec<BenchHistoryRow> = stmt
            .query_map([], |r| {
                let score_json: String = r.get(4)?;
                let score: BenchmarkScore = serde_json::from_str(&score_json).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                Ok(BenchHistoryRow {
                    id: r.get(0)?,
                    target_id: r.get(1)?,
                    session_id: r.get(2)?,
                    created_at_ms: r.get(3)?,
                    coverage_percent: score.coverage_percent,
                    matches: u32::try_from(score.matches.len()).unwrap_or(u32::MAX),
                    misses: u32::try_from(score.misses.len()).unwrap_or(u32::MAX),
                    false_positives: u32::try_from(score.false_positives.len()).unwrap_or(u32::MAX),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Upsert a review verdict for one (run, kind, label) tuple.
    /// Re-reviewing the same item overwrites the previous verdict —
    /// the table's primary key is `(run_id, kind, label)` so a second
    /// `record_verdict` call updates rather than inserts.
    pub fn record_verdict(
        &self,
        run_id: i64,
        kind: &str,
        label: &str,
        verdict: &str,
        note: Option<&str>,
        reviewed_at_ms: i64,
    ) -> Result<(), BenchError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| BenchError::Other("lock poisoned".into()))?;
        conn.execute(
            "INSERT INTO bench_review_verdicts (run_id, kind, label, verdict, note, reviewed_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(run_id, kind, label) DO UPDATE SET
                 verdict = excluded.verdict,
                 note    = excluded.note,
                 reviewed_at = excluded.reviewed_at",
            params![run_id, kind, label, verdict, note, reviewed_at_ms],
        )?;
        Ok(())
    }

    /// All verdicts recorded for one run, in insertion order.
    pub fn load_verdicts(&self, run_id: i64) -> Result<Vec<ReviewVerdict>, BenchError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| BenchError::Other("lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT kind, label, verdict, note, reviewed_at
             FROM bench_review_verdicts
             WHERE run_id = ?
             ORDER BY reviewed_at ASC",
        )?;
        let rows: Vec<ReviewVerdict> = stmt
            .query_map([run_id], |r| {
                Ok(ReviewVerdict {
                    run_id,
                    kind: r.get(0)?,
                    label: r.get(1)?,
                    verdict: r.get(2)?,
                    note: r.get(3)?,
                    reviewed_at_ms: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Load one run's full `run_json` + `score_json` by id.
    pub fn load_run(&self, id: i64) -> Result<Option<BenchFullRow>, BenchError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| BenchError::Other("lock poisoned".into()))?;
        let mut stmt = conn.prepare(
            "SELECT target_id, session_id, created_at, run_json, score_json
             FROM bench_runs WHERE id = ?",
        )?;
        let mut iter = stmt.query_map([id], |r| {
            Ok(BenchFullRow {
                id,
                target_id: r.get(0)?,
                session_id: r.get(1)?,
                created_at_ms: r.get(2)?,
                run_json: r.get(3)?,
                score_json: r.get(4)?,
            })
        })?;
        match iter.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchHistoryRow {
    pub id: i64,
    pub target_id: String,
    pub session_id: Option<String>,
    pub created_at_ms: i64,
    pub coverage_percent: f32,
    pub matches: u32,
    pub misses: u32,
    pub false_positives: u32,
}

/// One human review verdict on a (`miss` | `false_positive`) item
/// from a recorded run. Persisted in `bench_review_verdicts`.
/// Verdict values are interpreted by the CLI; the store treats them
/// as opaque strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewVerdict {
    pub run_id: i64,
    /// `"miss"` or `"false_positive"`.
    pub kind: String,
    /// For misses: the expected class. For false positives: the
    /// agent finding's title.
    pub label: String,
    /// Free-form. Conventional values: `actual_miss`,
    /// `scoring_failure`, `false_positive`, `wrongly_flagged`,
    /// `in_scope_extra`.
    pub verdict: String,
    pub note: Option<String>,
    pub reviewed_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchFullRow {
    pub id: i64,
    pub target_id: String,
    pub session_id: Option<String>,
    pub created_at_ms: i64,
    pub run_json: String,
    pub score_json: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::score::{BenchmarkScore, FindingMatch};

    fn stub_score() -> BenchmarkScore {
        BenchmarkScore {
            target_id: "t1".into(),
            target_name: "Test".into(),
            matches: vec![FindingMatch {
                expected_class: "reentrancy".into(),
                agent_finding_title: "A".into(),
                agent_finding_severity: "high".into(),
            }],
            misses: vec![],
            false_positives: vec![],
            coverage_percent: 100.0,
        }
    }

    #[test]
    fn record_and_history_round_trip() {
        let store = BenchStore::open_in_memory().unwrap();
        let id = store
            .record("t1", Some("s1"), "{}", &stub_score(), 100)
            .unwrap();
        assert!(id > 0);
        let h = store.history().unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].target_id, "t1");
        assert_eq!(h[0].matches, 1);
    }

    #[test]
    fn history_is_newest_first() {
        let store = BenchStore::open_in_memory().unwrap();
        store.record("t1", None, "{}", &stub_score(), 100).unwrap();
        store.record("t2", None, "{}", &stub_score(), 200).unwrap();
        let h = store.history().unwrap();
        assert_eq!(h[0].target_id, "t2");
        assert_eq!(h[1].target_id, "t1");
    }

    #[test]
    fn load_run_returns_none_for_missing() {
        let store = BenchStore::open_in_memory().unwrap();
        assert!(store.load_run(999).unwrap().is_none());
    }

    #[test]
    fn load_run_returns_full_row() {
        let store = BenchStore::open_in_memory().unwrap();
        let id = store
            .record("t1", Some("s1"), "{\"x\":1}", &stub_score(), 100)
            .unwrap();
        let row = store.load_run(id).unwrap().unwrap();
        assert_eq!(row.target_id, "t1");
        assert_eq!(row.run_json, "{\"x\":1}");
    }

    #[test]
    fn record_verdict_inserts_and_loads_back() {
        let store = BenchStore::open_in_memory().unwrap();
        let run_id = store
            .record("t1", None, "{}", &stub_score(), 100)
            .unwrap();
        store
            .record_verdict(run_id, "miss", "reentrancy", "actual_miss", Some("agent missed it"), 200)
            .unwrap();
        store
            .record_verdict(run_id, "false_positive", "Bad finding", "wrongly_flagged", None, 201)
            .unwrap();
        let v = store.load_verdicts(run_id).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].label, "reentrancy");
        assert_eq!(v[0].verdict, "actual_miss");
        assert_eq!(v[0].note.as_deref(), Some("agent missed it"));
        assert_eq!(v[1].kind, "false_positive");
    }

    #[test]
    fn record_verdict_upserts_on_repeat() {
        let store = BenchStore::open_in_memory().unwrap();
        let run_id = store
            .record("t1", None, "{}", &stub_score(), 100)
            .unwrap();
        store
            .record_verdict(run_id, "miss", "reentrancy", "actual_miss", None, 200)
            .unwrap();
        // Re-review with a new verdict.
        store
            .record_verdict(run_id, "miss", "reentrancy", "scoring_failure", Some("rework needed"), 300)
            .unwrap();
        let v = store.load_verdicts(run_id).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].verdict, "scoring_failure");
        assert_eq!(v[0].note.as_deref(), Some("rework needed"));
        assert_eq!(v[0].reviewed_at_ms, 300);
    }

    #[test]
    fn load_verdicts_for_unknown_run_is_empty() {
        let store = BenchStore::open_in_memory().unwrap();
        assert!(store.load_verdicts(9999).unwrap().is_empty());
    }

    #[test]
    fn verdicts_for_two_runs_are_isolated() {
        let store = BenchStore::open_in_memory().unwrap();
        let a = store.record("t1", None, "{}", &stub_score(), 100).unwrap();
        let b = store.record("t1", None, "{}", &stub_score(), 200).unwrap();
        store
            .record_verdict(a, "miss", "x", "actual_miss", None, 300)
            .unwrap();
        store
            .record_verdict(b, "miss", "y", "scoring_failure", None, 400)
            .unwrap();
        let va = store.load_verdicts(a).unwrap();
        let vb = store.load_verdicts(b).unwrap();
        assert_eq!(va.len(), 1);
        assert_eq!(vb.len(), 1);
        assert_eq!(va[0].label, "x");
        assert_eq!(vb[0].label, "y");
    }
}
