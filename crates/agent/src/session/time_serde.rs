//! `serde` helpers for `SystemTime` as UNIX epoch millis. Duplicated
//! from `basilisk_git::time_serde` / `basilisk_onchain::time_serde` /
//! `basilisk_project::time_serde` — small enough to keep self-contained
//! rather than picking up a dependency edge for one helper.

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
    let nanos = u32::try_from((ms % 1000) * 1_000_000).map_err(serde::de::Error::custom)?;
    Ok(UNIX_EPOCH + Duration::new(secs, nanos))
}

/// Convert a [`SystemTime`] into integer millis for `SQLite` storage.
#[allow(dead_code)] // `CP4b` will call this; keeping here so the helper's paired with the serde impls.
pub fn to_millis(t: SystemTime) -> i64 {
    // Clamp: negative-UNIX times don't happen in our flow; u128 → i64
    // truncates only if someone's clock is ~292M years into the future.
    i64::try_from(
        t.duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX)
}

/// Inverse of [`to_millis`].
#[allow(dead_code)] // `CP4c` will call this when reading rows back out.
pub fn from_millis(ms: i64) -> SystemTime {
    let ms = u64::try_from(ms).unwrap_or(0);
    UNIX_EPOCH + Duration::from_millis(ms)
}
