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
    /// [`ExplorerChain`] from the user's config, both with caching enabled.
    pub fn new(chain: &Chain, config: &Config) -> Result<Self, IngestError> {
        let rpc = AlloyProvider::for_chain(chain, config)?;
        let explorers = ExplorerChain::standard(chain, config);
        Ok(Self {
            chain: chain.clone(),
            rpc: Arc::new(rpc),
            explorers: Arc::new(explorers),
            timeout: Self::timeout_from_config(config),
        })
    }

    /// Same as [`Self::new`] but neither the bytecode cache nor the
    /// verified-source cache will be read or written. Use from `--no-cache`.
    pub fn new_uncached(chain: &Chain, config: &Config) -> Result<Self, IngestError> {
        let rpc = AlloyProvider::for_chain(chain, config)?.without_cache();
        let explorers = ExplorerChain::standard_uncached(chain, config);
        Ok(Self {
            chain: chain.clone(),
            rpc: Arc::new(rpc),
            explorers: Arc::new(explorers),
            timeout: Self::timeout_from_config(config),
        })
    }

    /// Override the overall resolution timeout. Builder method.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn timeout_from_config(config: &Config) -> Duration {
        Duration::from_secs(if config.onchain_timeout_secs == 0 {
            DEFAULT_TIMEOUT_SECS
        } else {
            config.onchain_timeout_secs
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
                constructor_args: None,
                storage_layout: None,
                referenced_addresses: Vec::new(),
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
            constructor_args: None,
            storage_layout: None,
            referenced_addresses: Vec::new(),
        })
    }

    /// Resolve a single contract with every CP5-7 enrichment turned on
    /// according to `limits`. Does **not** recurse into implementations
    /// or referenced addresses — that's the system orchestrator's job.
    #[allow(clippy::too_many_lines)] // Enrichment phases are tightly coupled; splitting each into
                                     // its own helper would just route-through control without clarifying anything.
    pub async fn resolve_full(
        &self,
        address: Address,
        limits: &crate::ExpansionLimits,
    ) -> Result<ResolvedContract, IngestError> {
        // Start from the Set-3 single-contract resolve with depth >= MAX_DEPTH
        // so the one-hop impl recursion is suppressed.
        let deadline = Instant::now() + self.timeout;
        let mut resolved = self.resolve_at_depth(address, deadline, MAX_DEPTH).await?;
        if !resolved.is_contract {
            return Ok(resolved);
        }

        // History — only for proxies.
        if limits.fetch_history {
            if let Some(proxy) = resolved.proxy.as_mut() {
                let head = self.rpc.get_block_number().await.unwrap_or(0);
                match crate::history::fetch_upgrade_history(
                    self.rpc.as_ref(),
                    address,
                    limits.history_from_block,
                    head,
                )
                .await
                {
                    Ok(events) => proxy.upgrade_history = events,
                    Err(e) => {
                        // Many free-tier RPC providers cap eth_getLogs range.
                        // Degrade gracefully: empty history + informational note.
                        if let IngestError::Rpc(rpc_err) = &e {
                            if basilisk_rpc::is_rpc_range_limited(rpc_err) {
                                tracing::warn!(
                                    chain = self.chain.canonical_name(),
                                    address = %address,
                                    "upgrade-history unavailable: RPC provider limits log queries",
                                );
                                resolved.resolution.proxy_detection_notes.push(
                                    "upgrade-history unavailable (RPC provider limits log \
                                     queries — upgrade RPC plan or set RPC_URL_<CHAIN> to a \
                                     provider without range limits)"
                                        .into(),
                                );
                                // Leave proxy.upgrade_history as its empty default.
                            } else {
                                resolved
                                    .resolution
                                    .proxy_detection_notes
                                    .push(format!("upgrade-history fetch failed: {e}"));
                            }
                        } else {
                            resolved
                                .resolution
                                .proxy_detection_notes
                                .push(format!("upgrade-history fetch failed: {e}"));
                        }
                    }
                }
            }
        }

        // Constructor args.
        if limits.fetch_constructor_args {
            match crate::constructor::recover_constructor_args(
                self.rpc.as_ref(),
                self.explorers.as_ref(),
                &self.chain,
                address,
                &resolved.bytecode,
            )
            .await
            {
                Ok(args) => resolved.constructor_args = args,
                Err(e) => {
                    resolved
                        .resolution
                        .proxy_detection_notes
                        .push(format!("constructor-args recovery failed: {e}"));
                }
            }
        }

        // Storage layout (currently stubbed — returns None).
        if limits.fetch_storage_layout {
            if let Some(src) = &resolved.source {
                match crate::storage_layout::recover_storage_layout(src).await {
                    Ok(layout) => resolved.storage_layout = layout,
                    Err(e) => {
                        resolved
                            .resolution
                            .proxy_detection_notes
                            .push(format!("storage-layout recovery failed: {e}"));
                    }
                }
            }
        }

        // Reference extraction: storage / bytecode / source.
        let mut refs = Vec::new();
        if limits.expand_storage {
            match crate::references::scan_storage_for_addresses(
                self.rpc.as_ref(),
                address,
                limits.storage_scan_depth,
            )
            .await
            {
                Ok(r) => refs.extend(r),
                Err(e) => {
                    resolved
                        .resolution
                        .proxy_detection_notes
                        .push(format!("storage scan failed: {e}"));
                }
            }
        }
        if limits.expand_bytecode {
            let candidates =
                crate::references::scan_bytecode_for_addresses(resolved.bytecode.as_ref());
            match crate::references::verify_bytecode_address_references(
                self.rpc.as_ref(),
                candidates,
            )
            .await
            {
                Ok(r) => refs.extend(r),
                Err(e) => {
                    resolved
                        .resolution
                        .proxy_detection_notes
                        .push(format!("bytecode scan failed: {e}"));
                }
            }
        }
        if limits.expand_immutables {
            if let Some(src) = &resolved.source {
                refs.extend(crate::references::extract_immutable_addresses(src));
            }
        }
        resolved.referenced_addresses = refs;
        Ok(resolved)
    }

    /// BFS from `root`, enriching every reachable contract and building
    /// a typed graph of how they relate. Every contract reachable within
    /// `limits.max_depth` / `max_contracts` / `max_duration` lands in
    /// the returned `ResolvedSystem.contracts` map.
    #[allow(clippy::too_many_lines)]
    pub async fn resolve_system(
        &self,
        root: alloy_primitives::Address,
        limits: crate::ExpansionLimits,
    ) -> Result<crate::ResolvedSystem, IngestError> {
        let start = Instant::now();
        let mut contracts = std::collections::BTreeMap::new();
        let mut graph = basilisk_graph::ContractGraph::new();
        let mut visited = std::collections::BTreeSet::new();
        let mut queue: std::collections::VecDeque<(alloy_primitives::Address, usize)> =
            std::collections::VecDeque::new();
        queue.push_back((root, 0));
        let mut stats = crate::SystemResolutionStats {
            duration: Duration::default(),
            ..Default::default()
        };

        while let Some((addr, depth)) = queue.pop_front() {
            if contracts.len() >= limits.max_contracts {
                stats
                    .expansion_truncated
                    .push(crate::TruncationReason::MaxContractsReached {
                        last_attempted: addr,
                    });
                break;
            }
            if start.elapsed() >= limits.max_duration {
                stats
                    .expansion_truncated
                    .push(crate::TruncationReason::MaxTimeReached);
                break;
            }
            if !visited.insert(addr) {
                continue;
            }

            let resolved = match self.resolve_full(addr, &limits).await {
                Ok(r) => r,
                Err(e) => {
                    stats.contracts_failed.push(crate::FailedResolution {
                        address: addr,
                        reached_via: Vec::new(),
                        error: e.to_string(),
                    });
                    continue;
                }
            };
            graph.add_node(addr);

            // If we've already hit max depth, don't enqueue anything further.
            let at_max_depth = depth >= limits.max_depth;
            if at_max_depth {
                stats
                    .expansion_truncated
                    .push(crate::TruncationReason::MaxDepthReached {
                        at_address: addr,
                        depth,
                    });
            }

            // Proxy edges.
            if let Some(proxy) = &resolved.proxy {
                if let Some(impl_addr) = proxy.implementation_address {
                    graph.add_edge(basilisk_graph::GraphEdge {
                        from: addr,
                        to: impl_addr,
                        kind: basilisk_graph::EdgeKind::ProxiesTo,
                    });
                    if !at_max_depth && !visited.contains(&impl_addr) {
                        queue.push_back((impl_addr, depth + 1));
                    }
                }
                if let Some(admin) = proxy.admin_address {
                    graph.add_edge(basilisk_graph::GraphEdge {
                        from: addr,
                        to: admin,
                        kind: basilisk_graph::EdgeKind::AdminOf,
                    });
                }
                if let Some(beacon) = proxy.beacon_address {
                    graph.add_edge(basilisk_graph::GraphEdge {
                        from: addr,
                        to: beacon,
                        kind: basilisk_graph::EdgeKind::BeaconOf,
                    });
                    if !at_max_depth && !visited.contains(&beacon) {
                        queue.push_back((beacon, depth + 1));
                    }
                }
                for facet in &proxy.facets {
                    graph.add_edge(basilisk_graph::GraphEdge {
                        from: addr,
                        to: facet.facet_address,
                        kind: basilisk_graph::EdgeKind::FacetOf,
                    });
                    if !at_max_depth && !visited.contains(&facet.facet_address) {
                        queue.push_back((facet.facet_address, depth + 1));
                    }
                }
                // History events — edges only, don't chase historical impls deeper.
                for event in &proxy.upgrade_history {
                    graph.add_edge(basilisk_graph::GraphEdge {
                        from: addr,
                        to: event.new_implementation,
                        kind: basilisk_graph::EdgeKind::HistoricalImplementation {
                            block: event.block_number,
                            tx_hash: event.tx_hash,
                        },
                    });
                }
            }

            // Address references (storage / bytecode / source).
            for r in &resolved.referenced_addresses {
                if r.address == alloy_primitives::Address::ZERO {
                    continue; // immutables we couldn't materialize
                }
                let kind = match &r.source {
                    crate::ReferenceSource::Storage { slot } => {
                        basilisk_graph::EdgeKind::ReferencesViaStorage { slot: *slot }
                    }
                    crate::ReferenceSource::Bytecode { offset } => {
                        basilisk_graph::EdgeKind::ReferencesViaBytecode { offset: *offset }
                    }
                    crate::ReferenceSource::Immutable { name }
                    | crate::ReferenceSource::VerifiedConstant { name } => {
                        basilisk_graph::EdgeKind::ReferencesViaImmutable { name: name.clone() }
                    }
                };
                graph.add_edge(basilisk_graph::GraphEdge {
                    from: addr,
                    to: r.address,
                    kind,
                });
                if !at_max_depth && !visited.contains(&r.address) {
                    queue.push_back((r.address, depth + 1));
                }
            }

            contracts.insert(addr, resolved);
        }

        stats.contracts_resolved = contracts.len();
        stats.duration = start.elapsed();
        Ok(crate::ResolvedSystem {
            root,
            chain: self.chain.clone(),
            contracts,
            graph,
            stats,
            resolved_at: SystemTime::now(),
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

    fn build_limits() -> crate::ExpansionLimits {
        crate::ExpansionLimits {
            max_depth: 2,
            max_contracts: 10,
            max_duration: Duration::from_secs(30),
            expand_storage: true,
            expand_bytecode: false, // noise in tests
            expand_immutables: false,
            fetch_history: false,
            fetch_constructor_args: false,
            fetch_storage_layout: false,
            storage_scan_depth: 4,
            history_from_block: 0,
            parallelism: 1,
        }
    }

    #[tokio::test]
    async fn resolve_system_single_contract() {
        let rpc: Arc<dyn RpcProvider> = Arc::new(
            MemoryProvider::new(Chain::EthereumMainnet).with_code(contract(), runtime_bytecode()),
        );
        let explorers = ExplorerChain::new_uncached(vec![]);
        let ingester = build_ingester(rpc, explorers);
        let system = ingester
            .resolve_system(contract(), build_limits())
            .await
            .unwrap();
        assert_eq!(system.root, contract());
        assert_eq!(system.contracts.len(), 1);
        assert_eq!(system.stats.contracts_resolved, 1);
        assert!(system.stats.contracts_failed.is_empty());
        assert_eq!(system.graph.edge_count(), 0);
    }

    #[tokio::test]
    async fn resolve_system_expands_proxy_to_impl() {
        let rpc: Arc<dyn RpcProvider> = Arc::new(
            MemoryProvider::new(Chain::EthereumMainnet)
                .with_code(contract(), runtime_bytecode())
                .with_code(impl_addr(), alloy_primitives::Bytes::from_static(&[0xab]))
                .with_slot(
                    contract(),
                    slots::IMPLEMENTATION_SLOT,
                    slot_for(impl_addr()),
                ),
        );
        let explorers = ExplorerChain::new_uncached(vec![]);
        let ingester = build_ingester(rpc, explorers);
        let system = ingester
            .resolve_system(contract(), build_limits())
            .await
            .unwrap();
        assert_eq!(system.contracts.len(), 2);
        let counts = system.graph.edge_counts();
        assert_eq!(counts.proxies_to, 1);
    }

    #[tokio::test]
    async fn resolve_system_truncates_at_max_contracts() {
        // 4 contracts linked via storage slot 0; limits cap at 2.
        let a = Address::from([1u8; 20]);
        let b = Address::from([2u8; 20]);
        let c = Address::from([3u8; 20]);
        let d = Address::from([4u8; 20]);
        let rpc: Arc<dyn RpcProvider> = Arc::new(
            MemoryProvider::new(Chain::EthereumMainnet)
                .with_code(a, runtime_bytecode())
                .with_code(b, runtime_bytecode())
                .with_code(c, runtime_bytecode())
                .with_code(d, runtime_bytecode())
                .with_slot(a, B256::ZERO, slot_for(b))
                .with_slot(b, B256::ZERO, slot_for(c))
                .with_slot(c, B256::ZERO, slot_for(d)),
        );
        let explorers = ExplorerChain::new_uncached(vec![]);
        let ingester = build_ingester(rpc, explorers);
        let mut limits = build_limits();
        limits.max_contracts = 2;
        let system = ingester.resolve_system(a, limits).await.unwrap();
        assert_eq!(system.contracts.len(), 2);
        assert!(system
            .stats
            .expansion_truncated
            .iter()
            .any(|t| matches!(t, crate::TruncationReason::MaxContractsReached { .. })));
    }

    #[tokio::test]
    async fn resolve_system_records_max_depth_truncation() {
        // Root references another contract via storage, max_depth = 0.
        let rpc: Arc<dyn RpcProvider> = Arc::new(
            MemoryProvider::new(Chain::EthereumMainnet)
                .with_code(contract(), runtime_bytecode())
                .with_code(impl_addr(), runtime_bytecode())
                .with_slot(contract(), B256::ZERO, slot_for(impl_addr())),
        );
        let explorers = ExplorerChain::new_uncached(vec![]);
        let ingester = build_ingester(rpc, explorers);
        let mut limits = build_limits();
        limits.max_depth = 0;
        let system = ingester.resolve_system(contract(), limits).await.unwrap();
        // Root only — the referenced contract isn't enqueued past depth 0.
        assert_eq!(system.contracts.len(), 1);
        assert!(system
            .stats
            .expansion_truncated
            .iter()
            .any(|t| matches!(t, crate::TruncationReason::MaxDepthReached { .. })));
    }

    #[tokio::test]
    async fn resolve_system_display_shows_counts_and_blocks() {
        let rpc: Arc<dyn RpcProvider> = Arc::new(
            MemoryProvider::new(Chain::EthereumMainnet)
                .with_code(contract(), runtime_bytecode())
                .with_code(impl_addr(), runtime_bytecode())
                .with_slot(
                    contract(),
                    slots::IMPLEMENTATION_SLOT,
                    slot_for(impl_addr()),
                ),
        );
        let explorers = ExplorerChain::new_uncached(vec![]);
        let ingester = build_ingester(rpc, explorers);
        let system = ingester
            .resolve_system(contract(), build_limits())
            .await
            .unwrap();
        let pretty = system.to_string();
        assert!(pretty.contains("System resolved from"));
        assert!(pretty.contains("Graph edges:"));
        assert!(pretty.contains("Contract "));
        assert!(pretty.contains("(root)"));
    }

    #[tokio::test]
    async fn resolve_system_dot_export_contains_nodes() {
        let rpc: Arc<dyn RpcProvider> = Arc::new(
            MemoryProvider::new(Chain::EthereumMainnet)
                .with_code(contract(), runtime_bytecode())
                .with_code(impl_addr(), runtime_bytecode())
                .with_slot(
                    contract(),
                    slots::IMPLEMENTATION_SLOT,
                    slot_for(impl_addr()),
                ),
        );
        let explorers = ExplorerChain::new_uncached(vec![]);
        let ingester = build_ingester(rpc, explorers);
        let system = ingester
            .resolve_system(contract(), build_limits())
            .await
            .unwrap();
        let dot = system.graph.to_dot();
        assert!(dot.starts_with("digraph G {"));
        assert!(dot.contains(&contract().to_string()));
        assert!(dot.contains("ProxiesTo"));
    }
}
