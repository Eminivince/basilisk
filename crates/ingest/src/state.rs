//! Incremental-ingest state tracking.
//!
//! Persisted at `~/.basilisk/knowledge/ingest_state.json` so a
//! re-run of `audit knowledge ingest <source>` skips records
//! already upserted. Each ingester owns its own entry; the file
//! merges updates atomically (read-modify-write with a tempfile
//! rename).
//!
//! State shape per source is deliberately loose — `cursor` is an
//! opaque string the ingester interprets. For Solodit it's the
//! highest-seen finding id; for GitHub-backed sources it's the
//! latest commit SHA; for scraped feeds it's an ISO-8601
//! timestamp. Ingesters set it, read it, advance it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::IngestError;

/// Per-source incremental state. Serialised in the state file
/// keyed by source name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceState {
    /// Opaque cursor: whatever the ingester uses to resume (id,
    /// timestamp, sha, page number). `None` means "no successful
    /// prior run."
    #[serde(default)]
    pub cursor: Option<String>,
    /// Total records the ingester has successfully persisted for
    /// this source across all runs.
    #[serde(default)]
    pub records_ingested: u64,
    /// Wall-clock timestamp (seconds since Unix epoch) of the last
    /// successful `ingest()` call. `None` pre-first-run.
    #[serde(default)]
    pub last_run_unix: Option<u64>,
}

/// The full state document. One row per source.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IngestState {
    #[serde(default)]
    pub sources: std::collections::BTreeMap<String, SourceState>,
}

impl IngestState {
    /// Load from disk. Returns an empty state when the file is
    /// missing — the natural case on a fresh machine.
    pub fn load(path: &Path) -> Result<Self, IngestError> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(serde_json::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomically write the state back to disk. The parent directory
    /// is created if missing. Writes via `<path>.tmp` + rename so a
    /// mid-write crash doesn't leave a half-written file.
    pub fn save(&self, path: &Path) -> Result<(), IngestError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Fetch a source's state. Returns a default (empty) state for
    /// sources that haven't been seen yet.
    #[must_use]
    pub fn get(&self, source: &str) -> SourceState {
        self.sources.get(source).cloned().unwrap_or_default()
    }

    /// Upsert one source's state. Returns `self` for chaining when
    /// updating several sources in one transaction.
    pub fn set(&mut self, source: impl Into<String>, state: SourceState) -> &mut Self {
        self.sources.insert(source.into(), state);
        self
    }
}

/// Default state-file path: `<dirs::home_dir>/.basilisk/knowledge/
/// ingest_state.json`. Falls back to `./.basilisk/knowledge/...`
/// when no home directory is discoverable.
#[must_use]
pub fn default_state_path() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        return home
            .join(".basilisk")
            .join("knowledge")
            .join("ingest_state.json");
    }
    PathBuf::from(".basilisk")
        .join("knowledge")
        .join("ingest_state.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_missing_file_is_empty_state() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ingest_state.json");
        let state = IngestState::load(&path).unwrap();
        assert!(state.sources.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ingest_state.json");
        let mut state = IngestState::default();
        state.set(
            "solodit",
            SourceState {
                cursor: Some("sol-1234".into()),
                records_ingested: 50_000,
                last_run_unix: Some(1_700_000_000),
            },
        );
        state.save(&path).unwrap();

        let back = IngestState::load(&path).unwrap();
        assert_eq!(back.sources.len(), 1);
        let s = back.get("solodit");
        assert_eq!(s.cursor.as_deref(), Some("sol-1234"));
        assert_eq!(s.records_ingested, 50_000);
    }

    #[test]
    fn save_creates_parent_directory_if_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("deep").join("state.json");
        let state = IngestState::default();
        state.save(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn set_on_existing_source_replaces() {
        let mut state = IngestState::default();
        state.set(
            "solodit",
            SourceState {
                cursor: Some("old".into()),
                ..Default::default()
            },
        );
        state.set(
            "solodit",
            SourceState {
                cursor: Some("new".into()),
                ..Default::default()
            },
        );
        assert_eq!(state.get("solodit").cursor.as_deref(), Some("new"));
    }

    #[test]
    fn get_unknown_source_returns_default() {
        let state = IngestState::default();
        let s = state.get("never-seen");
        assert!(s.cursor.is_none());
        assert_eq!(s.records_ingested, 0);
    }

    #[test]
    fn sources_iterate_alphabetically_via_btreemap() {
        let mut state = IngestState::default();
        state.set("zulu", SourceState::default());
        state.set("alpha", SourceState::default());
        state.set("mike", SourceState::default());
        let names: Vec<_> = state.sources.keys().collect();
        assert_eq!(names, vec!["alpha", "mike", "zulu"]);
    }
}
