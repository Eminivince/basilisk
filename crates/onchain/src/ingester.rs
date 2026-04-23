//! The `OnchainIngester` orchestrator.
//!
//! Composes an `RpcProvider`, an `ExplorerChain`, and the proxy detector
//! into a single `resolve(address)` entry point that returns a
//! [`ResolvedContract`]. Behavior:
//!
//! - Bytecode is fetched via RPC with a deadline; timing out here fails
//!   the whole call with [`IngestError::BytecodeTimeout`].
//! - Once bytecode is in hand, source-lookup and proxy detection run
//!   in parallel with individual timeouts against the remaining budget.
//!   Partial timeouts degrade gracefully — `source: None` or
//!   `proxy: None` with a note in `resolution.proxy_detection_notes`.
//! - If proxy detection yields an implementation address, the
//!   implementation is resolved once (depth capped at 1).

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use alloy_primitives::{keccak256, Address};
use basilisk_core::{Chain, Config};
use basilisk_explorers::ExplorerChain;
use basilisk_rpc::{AlloyProvider, RpcProvider};

use crate::{
    error::IngestError,
    proxy::{self, ProxyKind},
    resolved::{ResolutionSources, ResolvedContract},
};

/// Default overall timeout if `Config::onchain_timeout_secs` isn't set.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Maximum recursion depth for implementation resolution. "One hop only" —
/// the resolved implementation's own proxy chain is not followed.
const MAX_DEPTH: u32 = 1;

/// Orchestrator bringing RPC + explorers + proxy detection together.
pub struct OnchainIngester {
    chain: Chain,
    rpc: Arc<dyn RpcProvider>,
    explorers: Arc<ExplorerChain>,
    timeout: Duration,
}

impl std::fmt::Debug for OnchainIngester {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnchainIngester")
            .field("chain", &self.chain.canonical_name())
            .field("rpc_endpoint", &self.rpc.endpoint())
            .field("timeout_secs", &self.timeout.as_secs())
            .finish_non_exhaustive()
    }
}

impl OnchainIngester {
    /// Standard constructor: builds [`AlloyProvider`] and the full
    /// [`ExplorerChain`] from the user's config.
    pub fn new(chain: &Chain, config: &Config) -> Result<Self, IngestError> {
        let rpc = AlloyProvider::for_chain(chain, config)?;
        let explorers = ExplorerChain::standard(chain, config);
        let timeout = Duration::from_secs(if config.onchain_timeout_secs == 0 {
            DEFAULT_TIMEOUT_SECS
        } else {
            config.onchain_timeout_secs
        });
        Ok(Self {
            chain: chain.clone(),
            rpc: Arc::new(rpc),
            explorers: Arc::new(explorers),
            timeout,
        })
    }

    /// Test-oriented constructor: inject components directly.
    #[must_use]
    pub fn with_components(
        chain: Chain,
        rpc: Arc<dyn RpcProvider>,
        explorers: Arc<ExplorerChain>,
        timeout: Duration,
    ) -> Self {
        Self {
            chain,
            rpc,
            explorers,
            timeout,
        }
    }

    /// Fetch, classify, and resolve a contract. One-hop implementation
    /// resolution for proxies.
    pub async fn resolve(&self, address: Address) -> Result<ResolvedContract, IngestError> {
        let deadline = Instant::now() + self.timeout;
        self.resolve_at_depth(address, deadline, 0).await
    }

    async fn resolve_at_depth(
        &self,
        address: Address,
        deadline: Instant,
        depth: u32,
    ) -> Result<ResolvedContract, IngestError> {
        // Bytecode is required — no point continuing without it.
        let bytecode_budget = remaining(deadline);
        let bytecode = match tokio::time::timeout(bytecode_budget, self.rpc.get_code(address)).await
        {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => return Err(IngestError::Rpc(e)),
            Err(_) => return Err(IngestError::BytecodeTimeout(self.timeout.as_secs())),
        };
        let bytecode_hash = keccak256(bytecode.as_ref());
        let is_contract = !bytecode.is_empty();

        let mut resolution = ResolutionSources::new(self.rpc.endpoint());

        if !is_contract {
            resolution
                .proxy_detection_notes
                .push("no bytecode at address; skipping source + proxy detection".into());
            return Ok(ResolvedContract {
                address,
                chain: self.chain.clone(),
                bytecode,
                bytecode_hash,
                is_contract,
                source: None,
                proxy: None,
                implementation: None,
                fetched_at: SystemTime::now(),
                resolution,
            });
        }

        // Parallel source + proxy detection, each capped by remaining budget.
        let remaining_budget = remaining(deadline);
        let source_fut = self.explorers.resolve(&self.chain, address);
        let proxy_fut = proxy::detect_proxy(&bytecode, address, self.rpc.as_ref());

        let (source_res, proxy_res) = futures::join!(
            tokio::time::timeout(remaining_budget, source_fut),
            tokio::time::timeout(remaining_budget, proxy_fut),
        );

        let source = if let Ok(attempt) = source_res {
            resolution.source_attempts = attempt.attempts;
            attempt.result.map(|(name, src)| {
                resolution.source_winner = Some(name);
                src
            })
        } else {
            resolution
                .proxy_detection_notes
                .push("source resolution timed out".into());
            None
        };

        let proxy = if let Ok(p) = proxy_res {
            p
        } else {
            resolution
                .proxy_detection_notes
                .push("proxy detection timed out".into());
            None
        };

        if let Some(p) = &proxy {
            resolution
                .proxy_detection_notes
                .push(format!("proxy pattern: {}", kind_to_label(p.kind),));
        }

        // One-hop recursion: resolve the implementation too when we have
        // an address and we're not already at max depth.
        let implementation = if depth < MAX_DEPTH {
            match proxy.as_ref().and_then(|p| p.implementation_address) {
                Some(impl_addr) if impl_addr != Address::ZERO => {
                    match Box::pin(self.resolve_at_depth(impl_addr, deadline, depth + 1)).await {
                        Ok(r) => Some(Box::new(r)),
                        Err(e) => {
                            resolution.proxy_detection_notes.push(format!(
                                "implementation resolution failed for {impl_addr}: {e}",
                            ));
                            None
                        }
                    }
                }
                _ => None,
            }
        } else {
            None
        };

        Ok(ResolvedContract {
            address,
            chain: self.chain.clone(),
            bytecode,
            bytecode_hash,
            is_contract,
            source,
            proxy,
            implementation,
            fetched_at: SystemTime::now(),
            resolution,
        })
    }
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn kind_to_label(kind: ProxyKind) -> &'static str {
    match kind {
        ProxyKind::Eip1967Transparent => "EIP-1967 Transparent",
        ProxyKind::Eip1967Uups => "EIP-1967 UUPS",
        ProxyKind::Eip1967Beacon => "EIP-1967 Beacon",
        ProxyKind::Eip1167Minimal => "EIP-1167 Minimal",
        ProxyKind::Eip2535Diamond => "EIP-2535 Diamond",
        ProxyKind::UnknownProxyPattern => "unclassified proxy-like",
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy_primitives::{Bytes, B256};
    use basilisk_core::Chain;
    use basilisk_explorers::{ExplorerChain, Sourcify};
    use basilisk_rpc::MemoryProvider;
    use wiremock::{
        matchers::{method, path_regex},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;
    use crate::proxy::slots;

    fn contract() -> Address {
        Address::from_str("0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359").unwrap()
    }

    fn impl_addr() -> Address {
        let mut a = [0u8; 20];
        a[19] = 0x11;
        Address::from(a)
    }

    fn slot_for(addr: Address) -> B256 {
        let mut buf = [0u8; 32];
        buf[12..].copy_from_slice(addr.as_slice());
        B256::from(buf)
    }

    fn runtime_bytecode() -> Bytes {
        Bytes::from(vec![0x60u8, 0x80, 0x60, 0x40, 0x52])
    }

    fn build_ingester(rpc: Arc<dyn RpcProvider>, explorers: ExplorerChain) -> OnchainIngester {
        OnchainIngester::with_components(
            Chain::EthereumMainnet,
            rpc,
            Arc::new(explorers),
            Duration::from_secs(5),
        )
    }

    #[tokio::test]
    async fn eoa_returns_is_contract_false() {
        let rpc: Arc<dyn RpcProvider> = Arc::new(MemoryProvider::new(Chain::EthereumMainnet));
        let explorers = ExplorerChain::new_uncached(vec![]);
        let ingester = build_ingester(rpc, explorers);
        let resolved = ingester.resolve(contract()).await.unwrap();
        assert!(!resolved.is_contract);
        assert!(resolved.source.is_none());
        assert!(resolved.proxy.is_none());
    }

    #[tokio::test]
    async fn plain_contract_with_no_verified_source() {
        let rpc: Arc<dyn RpcProvider> = Arc::new(
            MemoryProvider::new(Chain::EthereumMainnet).with_code(contract(), runtime_bytecode()),
        );
        // Empty explorer chain: no source lookup attempts.
        let explorers = ExplorerChain::new_uncached(vec![]);
        let ingester = build_ingester(rpc, explorers);
        let resolved = ingester.resolve(contract()).await.unwrap();
        assert!(resolved.is_contract);
        assert_eq!(resolved.bytecode, runtime_bytecode());
        assert!(resolved.source.is_none());
        assert!(resolved.proxy.is_none());
        assert!(resolved.implementation.is_none());
    }

    #[tokio::test]
    async fn bytecode_hash_is_keccak_of_bytecode() {
        let rpc: Arc<dyn RpcProvider> = Arc::new(
            MemoryProvider::new(Chain::EthereumMainnet).with_code(contract(), runtime_bytecode()),
        );
        let explorers = ExplorerChain::new_uncached(vec![]);
        let ingester = build_ingester(rpc, explorers);
        let resolved = ingester.resolve(contract()).await.unwrap();
        let expected = keccak256(runtime_bytecode().as_ref());
        assert_eq!(resolved.bytecode_hash, expected);
    }

    #[tokio::test]
    async fn uups_proxy_resolves_implementation_one_hop() {
        // Proxy exposes impl slot pointing at impl_addr(); both addresses
        // have runtime bytecode.
        let rpc: Arc<dyn RpcProvider> = Arc::new(
            MemoryProvider::new(Chain::EthereumMainnet)
                .with_code(contract(), runtime_bytecode())
                .with_code(impl_addr(), Bytes::from(vec![0xab, 0xcd]))
                .with_slot(
                    contract(),
                    slots::IMPLEMENTATION_SLOT,
                    slot_for(impl_addr()),
                ),
        );
        let explorers = ExplorerChain::new_uncached(vec![]);
        let ingester = build_ingester(rpc, explorers);
        let resolved = ingester.resolve(contract()).await.unwrap();
        assert!(resolved.proxy.is_some());
        assert_eq!(
            resolved.proxy.as_ref().unwrap().kind,
            ProxyKind::Eip1967Uups
        );
        let imp = resolved.implementation.expect("implementation resolved");
        assert_eq!(imp.address, impl_addr());
        assert_eq!(imp.bytecode.as_ref(), &[0xab, 0xcd]);
    }

    #[tokio::test]
    async fn recursion_depth_capped_at_one() {
        // Proxy -> Impl where Impl is ALSO a proxy. The second-level
        // proxy's implementation should NOT be resolved.
        let second_impl = Address::from([0x22u8; 20]);
        let rpc: Arc<dyn RpcProvider> = Arc::new(
            MemoryProvider::new(Chain::EthereumMainnet)
                .with_code(contract(), runtime_bytecode())
                .with_code(impl_addr(), runtime_bytecode())
                .with_code(second_impl, Bytes::from(vec![0xff]))
                .with_slot(
                    contract(),
                    slots::IMPLEMENTATION_SLOT,
                    slot_for(impl_addr()),
                )
                .with_slot(
                    impl_addr(),
                    slots::IMPLEMENTATION_SLOT,
                    slot_for(second_impl),
                ),
        );
        let explorers = ExplorerChain::new_uncached(vec![]);
        let ingester = build_ingester(rpc, explorers);
        let resolved = ingester.resolve(contract()).await.unwrap();
        let imp = resolved.implementation.expect("impl resolved");
        // The impl is itself a proxy pointing at second_impl...
        assert!(imp.proxy.is_some());
        assert_eq!(
            imp.proxy.as_ref().unwrap().implementation_address,
            Some(second_impl)
        );
        // ...but depth capping means we don't resolve second_impl.
        assert!(imp.implementation.is_none());
    }

    #[tokio::test]
    async fn sourcify_found_via_wiremock_end_to_end() {
        // Stand up a fake Sourcify returning verified source for `contract()`.
        let server = MockServer::start().await;
        let metadata = serde_json::json!({
            "compiler": { "version": "0.8.20" },
            "settings": { "compilationTarget": { "X.sol": "X" } },
            "output": { "abi": [] }
        })
        .to_string();
        Mock::given(method("GET"))
            .and(path_regex(r"^/files/any/\d+/0x.*$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "full",
                "files": [
                    { "name": "metadata.json", "path": "metadata.json", "content": metadata },
                    { "name": "X.sol", "path": "X.sol", "content": "// X" }
                ]
            })))
            .mount(&server)
            .await;

        let rpc: Arc<dyn RpcProvider> = Arc::new(
            MemoryProvider::new(Chain::EthereumMainnet).with_code(contract(), runtime_bytecode()),
        );
        // Chain with a single mocked Sourcify.
        let sourcify: Arc<dyn basilisk_explorers::SourceExplorer> =
            Arc::new(Sourcify::new(server.uri()));
        let explorers = ExplorerChain::new_uncached(vec![sourcify]);
        let ingester = build_ingester(rpc, explorers);

        let resolved = ingester.resolve(contract()).await.unwrap();
        let src = resolved.source.expect("source found");
        assert_eq!(src.contract_name, "X");
        assert_eq!(
            resolved.resolution.source_winner.as_deref(),
            Some("sourcify")
        );
        assert_eq!(resolved.resolution.source_attempts.len(), 1);
    }

    #[tokio::test]
    async fn pretty_display_contains_sections() {
        let rpc: Arc<dyn RpcProvider> = Arc::new(
            MemoryProvider::new(Chain::EthereumMainnet)
                .with_code(contract(), runtime_bytecode())
                .with_code(impl_addr(), Bytes::from(vec![0x01]))
                .with_slot(
                    contract(),
                    slots::IMPLEMENTATION_SLOT,
                    slot_for(impl_addr()),
                ),
        );
        let explorers = ExplorerChain::new_uncached(vec![]);
        let ingester = build_ingester(rpc, explorers);
        let resolved = ingester.resolve(contract()).await.unwrap();
        let pretty = resolved.to_string();
        assert!(pretty.contains("Contract:"));
        assert!(pretty.contains("Chain:"));
        assert!(pretty.contains("Proxy:"));
        assert!(pretty.contains("Implementation:"));
        assert!(pretty.contains("Bytecode:"));
    }

    #[tokio::test]
    async fn json_round_trip_preserves_shape() {
        let rpc: Arc<dyn RpcProvider> = Arc::new(
            MemoryProvider::new(Chain::EthereumMainnet).with_code(contract(), runtime_bytecode()),
        );
        let explorers = ExplorerChain::new_uncached(vec![]);
        let ingester = build_ingester(rpc, explorers);
        let resolved = ingester.resolve(contract()).await.unwrap();
        let json = serde_json::to_string(&resolved).unwrap();
        let round: ResolvedContract = serde_json::from_str(&json).unwrap();
        assert_eq!(round.address, resolved.address);
        assert_eq!(round.is_contract, resolved.is_contract);
        assert_eq!(round.bytecode_hash, resolved.bytecode_hash);
    }
}
