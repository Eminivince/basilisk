//! On-disk, TTL-keyed key/value cache used by Basilisk's RPC and explorer
//! clients.
//!
//! Entries live at `<root>/<namespace>/<sha256(key)>.json` and carry a
//! `stored_at` / `expires_at` pair. Writes are atomic (tempfile + rename).
//! Expired entries are returned as `None` from [`Cache::get`] but are not
//! eagerly deleted — they get overwritten lazily on the next [`Cache::put`]
//! for the same key.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{fs, io::AsyncWriteExt};

/// Errors produced by the cache layer.
#[derive(Debug, Error)]
pub enum CacheError {
    /// Filesystem I/O failure while reading or writing a cache entry.
    #[error("cache I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON (de)serialization failure for a cache entry.
    #[error("cache serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// No usable base directory: the system cache dir was missing and the
    /// fallback could not be created.
    #[error("cache base directory is unavailable: {0}")]
    NoBaseDir(String),
    /// A namespace contained characters unsafe for a filesystem path.
    #[error("invalid cache namespace {0:?}: must be non-empty and path-safe")]
    InvalidNamespace(String),
}

/// A namespaced cache rooted at a single directory on disk.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
    namespace: String,
}

/// A successfully retrieved cache entry plus its timing metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cached<T> {
    pub value: T,
    #[serde(with = "epoch_ms")]
    pub stored_at: SystemTime,
    #[serde(with = "epoch_ms")]
    pub expires_at: SystemTime,
}

#[derive(Debug, Serialize, Deserialize)]
struct Envelope<T> {
    #[serde(with = "epoch_ms")]
    stored_at: SystemTime,
    #[serde(with = "epoch_ms")]
    expires_at: SystemTime,
    value: T,
}

impl Cache {
    /// Open (or create) a cache namespace under the system cache directory,
    /// or `.basilisk-cache/` in the working directory if none is available.
    pub fn open(namespace: &str) -> Result<Self, CacheError> {
        validate_namespace(namespace)?;
        let root = default_base_dir()?;
        Self::open_at(&root, namespace)
    }

    /// Open (or create) a cache namespace under an explicit root directory.
    /// Primarily for tests and the `audit cache` CLI helpers.
    pub fn open_at(root: &Path, namespace: &str) -> Result<Self, CacheError> {
        validate_namespace(namespace)?;
        let ns_dir = root.join(namespace);
        std::fs::create_dir_all(&ns_dir)?;
        Ok(Self {
            root: root.to_path_buf(),
            namespace: namespace.to_string(),
        })
    }

    /// Root (base) directory for this cache, shared across namespaces.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Namespace this handle is scoped to.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Fetch a value. Returns `None` for cache miss OR expired entry.
    pub async fn get<T: DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<Cached<T>>, CacheError> {
        let path = self.path_for(key);
        let bytes = match fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let envelope: Envelope<T> = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                // Corrupt entry — log and treat as a miss rather than failing the caller.
                tracing::warn!(
                    namespace = %self.namespace,
                    key,
                    error = %e,
                    "corrupt cache entry; treating as miss",
                );
                return Ok(None);
            }
        };
        let now = SystemTime::now();
        if envelope.expires_at <= now {
            return Ok(None);
        }
        Ok(Some(Cached {
            value: envelope.value,
            stored_at: envelope.stored_at,
            expires_at: envelope.expires_at,
        }))
    }

    /// Write `value` with a time-to-live. Atomic (tempfile + rename).
    pub async fn put<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        ttl: Duration,
    ) -> Result<(), CacheError> {
        let now = SystemTime::now();
        let envelope = Envelope {
            stored_at: now,
            expires_at: now + ttl,
            value,
        };
        let bytes = serde_json::to_vec(&envelope)?;

        let final_path = self.path_for(key);
        let dir = final_path
            .parent()
            .expect("namespace dir is always present");
        fs::create_dir_all(dir).await?;

        // Manual tempfile + rename: tokio::fs + tempfile's sync API don't mix
        // well, so we construct a sibling-named temp path ourselves.
        let tmp_path = dir.join(format!(
            ".{}.tmp",
            final_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("entry"),
        ));
        {
            let mut f = fs::File::create(&tmp_path).await?;
            f.write_all(&bytes).await?;
            f.sync_all().await?;
        }
        fs::rename(&tmp_path, &final_path).await?;
        Ok(())
    }

    /// Remove a single entry if present.
    pub async fn invalidate(&self, key: &str) -> Result<(), CacheError> {
        let path = self.path_for(key);
        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Remove every entry in this namespace.
    pub async fn clear(&self) -> Result<(), CacheError> {
        let ns_dir = self.namespace_dir();
        match fs::remove_dir_all(&ns_dir).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        fs::create_dir_all(&ns_dir).await?;
        Ok(())
    }

    /// Summary of current contents: (entry count, total size in bytes).
    pub async fn stats(&self) -> Result<NamespaceStats, CacheError> {
        let ns_dir = self.namespace_dir();
        let mut entries = 0u64;
        let mut bytes = 0u64;
        let mut read = match fs::read_dir(&ns_dir).await {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(NamespaceStats {
                    namespace: self.namespace.clone(),
                    entries: 0,
                    bytes: 0,
                });
            }
            Err(e) => return Err(e.into()),
        };
        while let Some(entry) = read.next_entry().await? {
            let meta = entry.metadata().await?;
            if meta.is_file() {
                entries += 1;
                bytes += meta.len();
            }
        }
        Ok(NamespaceStats {
            namespace: self.namespace.clone(),
            entries,
            bytes,
        })
    }

    fn namespace_dir(&self) -> PathBuf {
        self.root.join(&self.namespace)
    }

    fn path_for(&self, key: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(key.as_bytes());
        let digest = hex_lower(&hasher.finalize());
        self.namespace_dir().join(format!("{digest}.json"))
    }
}

/// Discover the default filesystem root for caches.
///
/// Prefers `dirs::cache_dir()/basilisk/`; falls back to `.basilisk-cache/`
/// in the current working directory if no system cache dir is available.
pub fn default_base_dir() -> Result<PathBuf, CacheError> {
    if let Some(system) = dirs::cache_dir() {
        let root = system.join("basilisk");
        std::fs::create_dir_all(&root)?;
        return Ok(root);
    }
    let cwd = std::env::current_dir().map_err(|e| CacheError::NoBaseDir(e.to_string()))?;
    let root = cwd.join(".basilisk-cache");
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

/// Per-namespace size/count summary returned by [`Cache::stats`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceStats {
    pub namespace: String,
    pub entries: u64,
    pub bytes: u64,
}

fn validate_namespace(ns: &str) -> Result<(), CacheError> {
    let ok = !ns.is_empty()
        && !ns.contains(['/', '\\', '\0', '.'])
        && ns
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(CacheError::InvalidNamespace(ns.to_string()))
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

mod epoch_ms {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let ms = t
            .duration_since(UNIX_EPOCH)
            .map_err(serde::ser::Error::custom)?
            .as_millis();
        s.serialize_u128(ms)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let ms = u128::deserialize(d)?;
        let secs = u64::try_from(ms / 1000).map_err(serde::de::Error::custom)?;
        // (ms % 1000) * 1_000_000 fits comfortably in u32: max 999_999_000.
        let nanos = u32::try_from((ms % 1000) * 1_000_000).map_err(serde::de::Error::custom)?;
        Ok(UNIX_EPOCH + Duration::new(secs, nanos))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct Sample {
        name: String,
        count: u32,
    }

    fn sample() -> Sample {
        Sample {
            name: "basilisk".into(),
            count: 42,
        }
    }

    #[tokio::test]
    async fn put_get_round_trip() {
        let dir = TempDir::new().unwrap();
        let c = Cache::open_at(dir.path(), "bytecode").unwrap();
        c.put("k1", &sample(), Duration::from_secs(60))
            .await
            .unwrap();
        let got: Cached<Sample> = c.get("k1").await.unwrap().expect("hit");
        assert_eq!(got.value, sample());
        assert!(got.expires_at > got.stored_at);
    }

    #[tokio::test]
    async fn miss_returns_none() {
        let dir = TempDir::new().unwrap();
        let c = Cache::open_at(dir.path(), "bytecode").unwrap();
        let got: Option<Cached<Sample>> = c.get("missing").await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn expired_entry_returns_none() {
        let dir = TempDir::new().unwrap();
        let c = Cache::open_at(dir.path(), "bytecode").unwrap();
        // TTL of zero: expires_at == stored_at, so `expires_at <= now` trips immediately.
        c.put("k", &sample(), Duration::from_secs(0)).await.unwrap();
        let got: Option<Cached<Sample>> = c.get("k").await.unwrap();
        assert!(got.is_none(), "expected expiry, got {got:?}");
    }

    #[tokio::test]
    async fn invalidate_removes_entry() {
        let dir = TempDir::new().unwrap();
        let c = Cache::open_at(dir.path(), "bytecode").unwrap();
        c.put("k", &sample(), Duration::from_secs(60))
            .await
            .unwrap();
        c.invalidate("k").await.unwrap();
        let got: Option<Cached<Sample>> = c.get("k").await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn invalidate_missing_key_is_ok() {
        let dir = TempDir::new().unwrap();
        let c = Cache::open_at(dir.path(), "bytecode").unwrap();
        c.invalidate("never-written").await.unwrap();
    }

    #[tokio::test]
    async fn clear_removes_everything() {
        let dir = TempDir::new().unwrap();
        let c = Cache::open_at(dir.path(), "bytecode").unwrap();
        c.put("a", &sample(), Duration::from_secs(60))
            .await
            .unwrap();
        c.put("b", &sample(), Duration::from_secs(60))
            .await
            .unwrap();
        c.clear().await.unwrap();
        let stats = c.stats().await.unwrap();
        assert_eq!(stats.entries, 0);
    }

    #[tokio::test]
    async fn namespaces_are_isolated() {
        let dir = TempDir::new().unwrap();
        let a = Cache::open_at(dir.path(), "bytecode").unwrap();
        let b = Cache::open_at(dir.path(), "verified_source").unwrap();
        a.put("k", &sample(), Duration::from_secs(60))
            .await
            .unwrap();
        let from_b: Option<Cached<Sample>> = b.get("k").await.unwrap();
        assert!(from_b.is_none());
    }

    #[tokio::test]
    async fn stats_counts_entries_and_bytes() {
        let dir = TempDir::new().unwrap();
        let c = Cache::open_at(dir.path(), "bytecode").unwrap();
        c.put("a", &sample(), Duration::from_secs(60))
            .await
            .unwrap();
        c.put("b", &sample(), Duration::from_secs(60))
            .await
            .unwrap();
        let stats = c.stats().await.unwrap();
        assert_eq!(stats.entries, 2);
        assert!(stats.bytes > 0);
        assert_eq!(stats.namespace, "bytecode");
    }

    #[tokio::test]
    async fn corrupt_entry_treated_as_miss() {
        let dir = TempDir::new().unwrap();
        let c = Cache::open_at(dir.path(), "bytecode").unwrap();
        let path = c.path_for("weird");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not valid json").unwrap();
        let got: Option<Cached<Sample>> = c.get("weird").await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn simulated_partial_write_leaves_no_visible_entry() {
        // Mimic a crashed write: drop a tempfile in the namespace but never rename.
        let dir = TempDir::new().unwrap();
        let c = Cache::open_at(dir.path(), "bytecode").unwrap();
        let ns = c.namespace_dir();
        std::fs::create_dir_all(&ns).unwrap();
        std::fs::write(ns.join(".orphan.tmp"), b"half-written").unwrap();
        let got: Option<Cached<Sample>> = c.get("anything").await.unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn invalid_namespace_rejected() {
        let dir = TempDir::new().unwrap();
        for bad in [
            "",
            "has/slash",
            "has.dot",
            "has\\back",
            "has\0nul",
            "weird!",
        ] {
            let err = Cache::open_at(dir.path(), bad).unwrap_err();
            assert!(
                matches!(err, CacheError::InvalidNamespace(_)),
                "{bad:?} → {err:?}"
            );
        }
    }

    #[test]
    fn valid_namespaces_accepted() {
        let dir = TempDir::new().unwrap();
        for ok in ["bytecode", "verified_source", "abi-cache", "ns1"] {
            Cache::open_at(dir.path(), ok).expect(ok);
        }
    }
}
