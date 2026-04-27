# Overview

**Basilisk** is an AI-driven smart-contract security auditor. It combines an LLM reasoning loop with deterministic on-chain and source-code analysis to audit EVM protocols end-to-end — from deployed bytecode to GitHub source, with full cross-contract graph awareness.

## What it does

Given a target — an on-chain address, a GitHub repository URL, or a local path — Basilisk:

1. **Resolves the system.** It follows proxy chains, detects diamond facets, traces upgrade histories, and builds a full `ContractGraph` of every contract involved in the protocol.
2. **Fetches verified source.** It pulls Solidity source from Sourcify, Etherscan V2, or Blockscout, then clones the underlying Git repo to recover build configs and remappings.
3. **Reasons about vulnerabilities.** The LLM agent uses 25+ tools to find callers, trace state dependencies, simulate call chains, and synthesize proof-of-concept Foundry tests.
4. **Writes a report.** The final output is a structured Markdown audit brief with findings, severity estimates, and reproduction steps.

## Key capabilities

| Capability | Description |
|---|---|
| Proxy detection | EIP-1967 (Transparent/UUPS/Beacon), EIP-1167 (minimal proxy), EIP-2535 (diamond) |
| Multi-chain | ~15 EVM chains; add any chain via `RPC_URL_<CHAIN>` |
| Source resolution | Sourcify → Etherscan V2 → Blockscout (waterfall fallback) |
| Knowledge base | Semantic search over Solodit, SWC, Code4rena, Sherlock, rekt.news, OZ advisories |
| Session persistence | SQLite-backed; interrupted runs can be resumed |
| Cost control | Hard caps on turns, tokens, USD cost, and wall-clock time |
| PoC synthesis | Foundry test generation and simulation on a forked network |

## Quick start

```bash
# Install
cargo install --path crates/cli

# Recon mode — fast protocol overview
audit recon 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 --chain ethereum

# Vulnerability mode — deep security analysis
audit recon https://github.com/compound-finance/compound-protocol --vuln

# Search the knowledge base
audit knowledge search "reentrancy via callback"

# List past sessions
audit session list
```

## Operating modes

**Recon mode** (default): 14 tools, ≤ 40 turns, $5 cap. Produces a protocol overview: architecture, trust model, external dependencies, and surface-level observations.

**Vuln mode** (`--vuln`): 25 tools, ≤ 100 turns, $50 cap. Hunts for concrete vulnerabilities, simulates exploits on forked state, and writes Foundry PoC tests.

## Project status

Basilisk is at Phase 4 (Checkpoint 9.5). The core audit loop — resolution, source fetching, LLM reasoning, PoC synthesis, and knowledge base — is fully operational. See `ROADMAP.md` for planned improvements including context compaction, parallelised resolution, and LanceDB migration.
