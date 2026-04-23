//! Per-contract enrichment types attached to [`crate::ResolvedContract`]:
//! constructor args, storage layout, and outbound address references.

use alloy_primitives::{Address, Bytes, B256};
use serde::{Deserialize, Serialize};

/// Constructor-argument recovery, produced by matching the creation
/// transaction's init-code tail against the runtime bytecode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstructorArgs {
    /// Raw bytes appended to the init code. Empty if extraction fell back.
    pub raw: Bytes,
    /// ABI-decoded arguments when the contract has a verified constructor ABI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decoded: Option<Vec<DecodedArg>>,
    pub creation_tx: B256,
    pub creator: Address,
    pub creation_block: u64,
}

/// One decoded constructor parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodedArg {
    pub name: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub value: serde_json::Value,
}

/// Storage-layout recovery produced by compiling verified source with solc's
/// `--storage-layout` output enabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageLayout {
    pub slots: Vec<StorageSlot>,
    pub solc_version: String,
    pub source: StorageLayoutSource,
}

/// Where a [`StorageLayout`] came from. Future phases may add decompilation
/// or heuristic inference as additional variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageLayoutSource {
    SolcCompilation,
}

/// One entry in a [`StorageLayout`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSlot {
    pub slot: B256,
    pub offset: u32,
    #[serde(rename = "type")]
    pub type_: String,
    pub label: String,
    /// Which contract in the inheritance chain declared this slot.
    pub contract: String,
}

/// An outbound address reference observed during resolution.
///
/// Each reference is one link in the [`basilisk_graph::ContractGraph`] the
/// expansion orchestrator builds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressReference {
    pub address: Address,
    pub source: ReferenceSource,
    /// Human-readable context, e.g. "storage slot 0x0",
    /// "immutable ORACLE", "bytecode offset 0x3fe".
    pub context: String,
}

/// Where an [`AddressReference`] was discovered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReferenceSource {
    Storage { slot: B256 },
    Bytecode { offset: usize },
    Immutable { name: String },
    VerifiedConstant { name: String },
}
