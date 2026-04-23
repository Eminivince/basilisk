//! Pretty multi-section `Display` for [`ResolvedContract`].

use std::fmt;

use basilisk_explorers::ExplorerOutcome;

use crate::{
    proxy::{ProxyInfo, ProxyKind},
    resolved::ResolvedContract,
    system::ResolvedSystem,
};

impl fmt::Display for ResolvedContract {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        render(self, f, 0)
    }
}

fn render(c: &ResolvedContract, f: &mut fmt::Formatter<'_>, indent: usize) -> fmt::Result {
    let pad = "  ".repeat(indent);

    writeln!(f, "{pad}Contract: {}", c.address)?;
    writeln!(
        f,
        "{pad}  Chain:    {} (id {})",
        c.chain,
        c.chain.chain_id()
    )?;
    if !c.is_contract {
        writeln!(f, "{pad}  Bytecode: <empty> (EOA or self-destructed)")?;
        writeln!(f, "{pad}  Verified: n/a")?;
        return Ok(());
    }

    match (&c.source, &c.resolution.source_winner) {
        (Some(src), Some(name)) => {
            let quality = source_match_label(src);
            writeln!(f, "{pad}  Verified: yes (via {name}{quality})")?;
            if !src.contract_name.is_empty() {
                writeln!(f, "{pad}  Name:     {}", src.contract_name)?;
            }
            if !src.compiler_version.is_empty() {
                writeln!(f, "{pad}  Compiler: {}", src.compiler_version)?;
            }
        }
        _ => {
            writeln!(f, "{pad}  Verified: no")?;
        }
    }

    if let Some(p) = &c.proxy {
        render_proxy(p, f, indent)?;
    }

    writeln!(
        f,
        "{pad}  Bytecode: {} bytes (hash {})",
        c.bytecode.len(),
        c.bytecode_hash,
    )?;

    writeln!(f, "{pad}  Sources:")?;
    writeln!(f, "{pad}    rpc:       {}", c.resolution.bytecode_rpc)?;
    if c.resolution.source_attempts.is_empty() {
        writeln!(f, "{pad}    explorers: <none>")?;
    } else {
        let explorers: Vec<String> = c
            .resolution
            .source_attempts
            .iter()
            .map(|a| format!("{}={}", a.explorer, outcome_label(&a.outcome)))
            .collect();
        writeln!(f, "{pad}    explorers: {}", explorers.join(", "))?;
    }
    for note in &c.resolution.proxy_detection_notes {
        writeln!(f, "{pad}    note:      {note}")?;
    }

    if let Some(imp) = &c.implementation {
        writeln!(f)?;
        writeln!(f, "{pad}Implementation: {}", imp.address)?;
        render(imp, f, indent + 1)?;
    }

    Ok(())
}

fn render_proxy(p: &ProxyInfo, f: &mut fmt::Formatter<'_>, indent: usize) -> fmt::Result {
    let pad = "  ".repeat(indent);
    writeln!(f, "{pad}  Proxy:    {}", kind_label(p.kind))?;
    if let Some(addr) = p.implementation_address {
        writeln!(f, "{pad}    Implementation: {addr}")?;
    }
    if let Some(addr) = p.admin_address {
        writeln!(f, "{pad}    Admin:          {addr}")?;
    }
    if let Some(addr) = p.beacon_address {
        writeln!(f, "{pad}    Beacon:         {addr}")?;
    }
    if !p.facets.is_empty() {
        writeln!(f, "{pad}    Facets: {}", p.facets.len())?;
        for facet in &p.facets {
            writeln!(
                f,
                "{pad}      - {} ({} selectors)",
                facet.facet_address,
                facet.selectors.len()
            )?;
        }
    }
    Ok(())
}

fn kind_label(k: ProxyKind) -> &'static str {
    match k {
        ProxyKind::Eip1967Transparent => "EIP-1967 Transparent",
        ProxyKind::Eip1967Uups => "EIP-1967 UUPS",
        ProxyKind::Eip1967Beacon => "EIP-1967 Beacon",
        ProxyKind::Eip1167Minimal => "EIP-1167 Minimal",
        ProxyKind::Eip2535Diamond => "EIP-2535 Diamond",
        ProxyKind::UnknownProxyPattern => "unclassified proxy-like",
    }
}

fn source_match_label(src: &basilisk_explorers::VerifiedSource) -> &'static str {
    match src.metadata.pointer("/sourcify_match") {
        Some(serde_json::Value::String(s)) if s == "partial" => ", partial match",
        Some(serde_json::Value::String(s)) if s == "full" => ", full match",
        _ => "",
    }
}

fn outcome_label(o: &ExplorerOutcome) -> String {
    match o {
        ExplorerOutcome::Found { match_quality } => match match_quality {
            basilisk_explorers::MatchQuality::Full => "found(full)".into(),
            basilisk_explorers::MatchQuality::Partial => "found(partial)".into(),
        },
        ExplorerOutcome::NotVerified => "not-verified".into(),
        ExplorerOutcome::ChainUnsupported => "chain-unsupported".into(),
        ExplorerOutcome::NoApiKey => "no-api-key".into(),
        ExplorerOutcome::NetworkError(_) => "network-error".into(),
        ExplorerOutcome::RateLimited => "rate-limited".into(),
        ExplorerOutcome::Other(_) => "other".into(),
    }
}

impl fmt::Display for ResolvedSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let counts = self.graph.edge_counts();
        writeln!(
            f,
            "System resolved from {} on {} (id {})",
            self.root,
            self.chain,
            self.chain.chain_id()
        )?;
        writeln!(
            f,
            "  Contracts: {} resolved, {} failed",
            self.stats.contracts_resolved,
            self.stats.contracts_failed.len(),
        )?;
        writeln!(
            f,
            "  Graph edges: {} ({} ProxiesTo, {} FacetOf, {} Historical, {} StorageRef, {} BytecodeRef, {} ImmutableRef)",
            counts.total(),
            counts.proxies_to,
            counts.facet_of,
            counts.historical_implementation,
            counts.references_via_storage,
            counts.references_via_bytecode,
            counts.references_via_immutable,
        )?;
        writeln!(f, "  Duration: {:.2}s", self.stats.duration.as_secs_f64())?;
        for reason in &self.stats.expansion_truncated {
            match reason {
                crate::TruncationReason::MaxDepthReached { at_address, depth } => {
                    writeln!(
                        f,
                        "  Truncation: max-depth reached at {at_address} (depth {depth})"
                    )?;
                }
                crate::TruncationReason::MaxContractsReached { last_attempted } => {
                    writeln!(
                        f,
                        "  Truncation: max-contracts reached (last attempted {last_attempted})"
                    )?;
                }
                crate::TruncationReason::MaxTimeReached => {
                    writeln!(f, "  Truncation: max-duration reached")?;
                }
            }
        }
        if !self.stats.contracts_failed.is_empty() {
            writeln!(f, "  Failures:")?;
            for failure in &self.stats.contracts_failed {
                writeln!(f, "    {}: {}", failure.address, failure.error)?;
            }
        }
        writeln!(f)?;

        // Root first, then others alphabetically. Write each as a block.
        let mut first = true;
        if let Some(root) = self.contracts.get(&self.root) {
            writeln!(f, "Contract {} (root)", root.address)?;
            render(root, f, 0)?;
            first = false;
        }
        for (addr, contract) in &self.contracts {
            if *addr == self.root {
                continue;
            }
            if !first {
                writeln!(f)?;
            }
            writeln!(f, "Contract {addr}")?;
            render(contract, f, 0)?;
            first = false;
        }
        Ok(())
    }
}
