//! Storage-layout recovery by compiling verified source.
//!
//! **Status: stubbed.** The public API and types are wired, but the actual
//! `foundry-compilers` / `solc` invocation is deferred. Every call currently
//! returns `Ok(None)` with a tracing note. This keeps the orchestrator's
//! call shape stable so a later commit can drop in a real implementation
//! without touching [`crate::OnchainIngester::resolve_system`].
//!
//! The reasons for stubbing rather than integrating `foundry-compilers`
//! live-in this checkpoint:
//! - `foundry-compilers` pulls in a large transitive dependency tree.
//! - Its builder API changes between major versions; pinning it blindly
//!   risks a compile break without iterative testing.
//! - First use downloads a solc binary from GitHub, which doesn't work in
//!   the sandbox where these checkpoints were built.
//!
//! The intended shape (once implemented):
//!   1. Spin up a [`tempfile::TempDir`].
//!   2. Write every file in `source.source_files` into the tempdir, using
//!      the sanitized paths as-is.
//!   3. Construct a `foundry_compilers::Project` with the compiler
//!      version parsed out of `source.compiler_version` and the optimizer
//!      settings pulled from `source.optimizer`.
//!   4. Compile with `--storage-layout` enabled; walk the compilation
//!      output for `source.contract_name`.
//!   5. Map solc's storage-layout JSON into our [`StorageLayout`] shape.

use basilisk_explorers::VerifiedSource;

use crate::{
    enrichment::{StorageLayout, StorageLayoutSource, StorageSlot},
    error::IngestError,
};

/// Compile the verified source and extract the storage layout for its
/// primary contract. See module docs — currently returns `Ok(None)`.
pub async fn recover_storage_layout(
    source: &VerifiedSource,
) -> Result<Option<StorageLayout>, IngestError> {
    if source.source_files.is_empty() {
        tracing::debug!(
            contract = %source.contract_name,
            "storage-layout recovery skipped: no source files",
        );
        return Ok(None);
    }
    tracing::info!(
        contract = %source.contract_name,
        compiler = %source.compiler_version,
        "storage-layout recovery pending solc integration",
    );
    Ok(None)
}

/// Translate a solc storage-layout JSON entry into our [`StorageSlot`].
///
/// Exposed for testability of the layout-parsing path ahead of the full
/// compiler integration.
pub fn slot_from_solc_json(entry: &serde_json::Value) -> Option<StorageSlot> {
    let slot_dec = entry.get("slot")?.as_str()?;
    let slot_u = slot_dec.parse::<u128>().ok()?;
    let mut slot = [0u8; 32];
    slot[16..].copy_from_slice(&slot_u.to_be_bytes());
    let offset = u32::try_from(entry.get("offset")?.as_u64()?).ok()?;
    let label = entry.get("label")?.as_str()?.to_string();
    let contract = entry.get("contract")?.as_str()?.to_string();
    let type_ = entry.get("type")?.as_str()?.to_string();
    Some(StorageSlot {
        slot: alloy_primitives::B256::from(slot),
        offset,
        type_,
        label,
        contract,
    })
}

/// Assemble a [`StorageLayout`] from a full solc `storageLayout` JSON
/// object (with `"storage"` array). Returns `None` if the shape doesn't
/// match.
pub fn layout_from_solc_json(
    solc_output: &serde_json::Value,
    solc_version: impl Into<String>,
) -> Option<StorageLayout> {
    let entries = solc_output.get("storage")?.as_array()?;
    let mut slots = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Some(slot) = slot_from_solc_json(entry) {
            slots.push(slot);
        }
    }
    Some(StorageLayout {
        slots,
        solc_version: solc_version.into(),
        source: StorageLayoutSource::SolcCompilation,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;

    fn empty_verified_source() -> VerifiedSource {
        VerifiedSource {
            source_files: BTreeMap::new(),
            contract_name: "Empty".into(),
            compiler_version: "0.8.20".into(),
            optimizer: None,
            evm_version: None,
            abi: serde_json::Value::Array(vec![]),
            constructor_args: None,
            license: None,
            proxy_hint: None,
            implementation_hint: None,
            metadata: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn empty_source_returns_none() {
        let src = empty_verified_source();
        let out = recover_storage_layout(&src).await.unwrap();
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn non_empty_source_currently_returns_none() {
        let mut src = empty_verified_source();
        src.source_files.insert(
            std::path::PathBuf::from("Token.sol"),
            "contract Token { uint256 x; }".into(),
        );
        let out = recover_storage_layout(&src).await.unwrap();
        // Will change to Some(...) once solc integration lands.
        assert!(out.is_none());
    }

    #[test]
    fn slot_from_solc_json_parses_basic_entry() {
        let entry = json!({
            "slot": "0",
            "offset": 0,
            "label": "_owner",
            "contract": "Ownable",
            "type": "t_address"
        });
        let slot = slot_from_solc_json(&entry).expect("parse");
        assert_eq!(slot.label, "_owner");
        assert_eq!(slot.contract, "Ownable");
        assert_eq!(slot.offset, 0);
        assert_eq!(slot.type_, "t_address");
        // slot 0 → 32 zero bytes
        assert_eq!(slot.slot, alloy_primitives::B256::ZERO);
    }

    #[test]
    fn slot_from_solc_json_parses_high_slot_number() {
        let entry = json!({
            "slot": "42",
            "offset": 12,
            "label": "_balance",
            "contract": "ERC20",
            "type": "t_mapping"
        });
        let slot = slot_from_solc_json(&entry).expect("parse");
        assert_eq!(slot.offset, 12);
        // Low-byte should be 42.
        assert_eq!(slot.slot.as_slice()[31], 42);
    }

    #[test]
    fn slot_from_solc_json_rejects_missing_fields() {
        let entry = json!({ "slot": "0", "offset": 0, "label": "x" });
        assert!(slot_from_solc_json(&entry).is_none());
    }

    #[test]
    fn layout_from_solc_json_collects_storage_array() {
        let output = json!({
            "storage": [
                {
                    "slot": "0",
                    "offset": 0,
                    "label": "a",
                    "contract": "C",
                    "type": "t_uint256"
                },
                {
                    "slot": "1",
                    "offset": 0,
                    "label": "b",
                    "contract": "C",
                    "type": "t_address"
                }
            ]
        });
        let layout = layout_from_solc_json(&output, "0.8.20").expect("layout");
        assert_eq!(layout.slots.len(), 2);
        assert_eq!(layout.solc_version, "0.8.20");
        assert_eq!(layout.source, StorageLayoutSource::SolcCompilation);
    }

    #[test]
    fn layout_from_solc_json_handles_missing_storage_key() {
        let output = json!({ "types": {} });
        assert!(layout_from_solc_json(&output, "0.8.20").is_none());
    }
}
