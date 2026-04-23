//! Re-exports of the alloy RPC types we surface through the
//! [`crate::RpcProvider`] trait.
//!
//! Kept in their own module so downstream consumers can import them
//! directly (`basilisk_rpc::types::RpcLog`) without needing an
//! `alloy-rpc-types-eth` dep of their own.

pub use alloy_rpc_types_eth::{Log as RpcLog, Transaction as RpcTransaction};
