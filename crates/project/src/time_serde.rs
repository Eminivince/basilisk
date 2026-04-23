//! `serde` helpers for `SystemTime` as UNIX epoch millis. Duplicated from
//! `basilisk_git::time_serde` and `basilisk_onchain::time_serde` so the
//! project crate doesn't pick up those dependencies just for this.

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
