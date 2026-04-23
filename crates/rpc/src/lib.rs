//! RPC client abstraction for Basilisk.
//!
//! [`RpcProvider`] is the narrow trait every consumer (on-chain ingester,
//! proxy detector) codes against. [`AlloyProvider`] is the default
//! implementation — a thin wrapper around `alloy-provider`'s HTTP transport
//! with URL resolution, bytecode caching, and a small retry loop.

pub mod alloy_impl;
pub mod chunked;
pub mod error;
pub mod memory;
pub mod provider;
pub mod resolver;
pub mod retry;
pub mod types;

pub use alloy_impl::AlloyProvider;
pub use chunked::fetch_logs_chunked;
pub use error::{is_rpc_range_limited, RpcError};
pub use memory::MemoryProvider;
pub use provider::{LogFilter, RpcProvider};
pub use resolver::{resolve_rpc_url, ResolvedEndpoint, RpcSource};
pub use types::{RpcLog, RpcTransaction};
