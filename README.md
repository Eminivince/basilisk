# Basilisk

An AI-driven smart-contract auditor that reasons about protocols end-to-end —
from deployed bytecode to GitHub source, with cross-contract graph awareness.

**Status:** Phase 2 complete — tool surface shipped. Given a GitHub URL, a
deployed address, or a local path, Basilisk resolves the full system:
bytecode, verified source, proxy structure, upgrade history, and a typed
cross-contract graph. The AI reasoning layer (Phase 3) is in progress.

## What it does today

### On-chain resolution

Given an address on any supported EVM chain:

- Fetches bytecode via RPC (alloy-based client with retries and namespaced caching).
- Resolves verified source through a Sourcify → Etherscan V2 → Blockscout
  fallback chain.
- Detects proxy patterns: EIP-1967 (Transparent / UUPS / Beacon),
  EIP-1167 minimal proxies, EIP-2535 diamonds.
- Expands the contract graph: recursive implementation resolution, upgrade
  history (where the RPC permits), constructor-argument recovery,
  bytecode `PUSH20` / storage-slot / verified-source immutable address
  references.
- Bounded expansion with configurable depth, contract count, and duration
  limits — emits a `TruncationReason` when a budget is hit.

### Source resolution

Given a GitHub URL or a local path:

- Shallow-clones the repo (HTTPS) into a persistent cache keyed by commit SHA.
- Resolves ambiguous refs (branch vs tag) via the GitHub API.
- Detects project type: Foundry, Hardhat, Truffle, Mixed.
- Parses `foundry.toml`, `hardhat.config.{js,ts,cjs,mjs}`,
  `truffle-config.js`, `remappings.txt`, and `package.json`.
- Enumerates Solidity files with kind classification (source, test, script).
- Parses import statements, builds the project-wide import graph, reports
  unresolved imports with the search path tried.

### Supported chains

Ethereum, Sepolia, Arbitrum, Arbitrum Sepolia, Base, Base Sepolia,
Optimism, Optimism Sepolia, Polygon, BNB, Avalanche. Other EVM chains are
reachable via `RPC_URL_<CHAIN>` overrides.

## Roadmap

- **Phase 3: the agent.** Wire an LLM (Claude by default, model-agnostic
  backend) to the existing tool surface via a tool-use loop. The agent reads
  resolved systems, forms vulnerability hypotheses, and produces findings
  with attributed reasoning.
- **Phase 4: knowledge base.** Retrieval-augmented grounding over audit
  corpora (Solodit, Code4rena, Sherlock, SWC registry) and per-engagement
  context (protocol docs, whitepapers). Every finding retrieves its
  precedent.
- **Phase 5: proof-of-concept synthesis.** Findings get proven. The agent
  writes Foundry fork tests that reproduce exploits against mainnet state.
  If the test fails, the finding is demoted.

Everything executes in forked simulation — Basilisk never broadcasts
transactions to real networks.

## Quickstart

```bash
# clone
git clone https://github.com/eminivince/basilisk
cd basilisk

# build
cargo build --release

# configure
cp .env.example .env
# edit .env — at minimum set ALCHEMY_API_KEY and ETHERSCAN_API_KEY for
# on-chain, and GITHUB_TOKEN for GitHub rate-limit headroom

# run
./target/release/audit recon 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 --chain ethereum
./target/release/audit recon https://github.com/foundry-rs/forge-template
./target/release/audit recon ./path/to/foundry-project
```

To install `audit` as a system binary: `cargo install --path crates/cli`.

## Configuration

| Variable | What it enables | Without it |
|---|---|---|
| `ANTHROPIC_API_KEY` | Phase 3+ agent reasoning (not yet wired) | Phase 2 tool surface works without |
| `ALCHEMY_API_KEY` | Primary RPC for supported chains | Falls back to `RPC_URL_<CHAIN>` or public RPC |
| `ETHERSCAN_API_KEY` | Verified source, creation-tx lookup, multi-chain via Etherscan V2 | Falls back to Sourcify and Blockscout |
| `GITHUB_TOKEN` | 5000/hour API rate limit, private-repo access, authenticated clones | 60/hour unauthenticated |
| `RPC_URL_<CHAIN>` | Override RPC for a specific chain (e.g. `RPC_URL_BNB=...`) | Uses Alchemy if supported, else public |
| `LOG_LEVEL` | `error`, `warn`, `info`, `debug`, `trace`; accepts any `tracing` filter | Defaults to `info` |

**Upgrade-history caveat.** Upgrade history requires an RPC provider that
permits large `eth_getLogs` queries. Alchemy's free tier caps at 10-block
ranges, so history degrades to a logged warning on that tier and the output
records `upgrade-history unavailable`. Paid Alchemy, QuickNode, or a
self-hosted node resolves this — point `RPC_URL_ETHEREUM` at it.

## Demo

### On-chain

```console
$ audit recon 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2 --chain ethereum

System resolved from 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2 on ethereum (id 1)
  Contracts: 2 resolved, 0 failed
  Graph edges: 3 (1 ProxiesTo, 0 FacetOf, 0 Historical, 1 StorageRef, 1 BytecodeRef, 0 ImmutableRef)
  Duration: 3.42s

Contract: 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2
  Chain:    ethereum (id 1)
  Verified: yes (via etherscan, full match)
  Name:     InitializableImmutableAdminUpgradeabilityProxy
  Compiler: 0.8.10+commit.fc410830
  Proxy:    EIP-1967 Transparent
    Implementation: 0x5faab9e1adbddad0a08734be8a52185fd6558e14
    Admin:          0xEC568fffba86c094cf06b22134B23074DFE2252c
  Bytecode: 1763 bytes (hash 0xabcd...)
  Sources:
    rpc:       https://eth-mainnet.g.alchemy.com/v2/***
    explorers: sourcify=not-verified, etherscan=found(full)
    note:      upgrade-history unavailable (RPC provider limits log queries
               — upgrade RPC plan or set RPC_URL_ETHEREUM to a provider
               without range limits)

Contract: 0x5faab9e1adbddad0a08734be8a52185fd6558e14
  Verified: yes (via etherscan, full match)
  Name:     PoolInstance
  Proxy:    <none>
  References:
    storage slot 0x07 → ACLManager
    bytecode 0x3fe   → WETH9
```

Basilisk identified the Aave V3 Pool proxy, resolved its implementation
one hop down, and discovered the library contracts the implementation
delegates to via bytecode `PUSH20` and storage-slot scanning. Upgrade
history was unavailable because the demo used Alchemy's free tier, which
caps `eth_getLogs` queries — see [Configuration](#configuration).

<!-- TODO: replace with fresh output from a paid-tier RPC run once available;
     current block preserves the vetted Set 5 CP1 demo with API keys redacted
     to ***. -->

### Source, from GitHub

```console
$ audit recon https://github.com/foundry-rs/forge-template

Project at ~/.basilisk/repos/foundry-rs/forge-template/f5db6aeeff588c8a789b6f7da83313950fd97178
  kind: foundry
  configs: foundry.toml
  sources: 1 file(s)
  tests: 1 file(s)
  missing dirs: 1
    - script
  imports: 1 resolved, 1 unresolved (1 file(s) with unresolved)

Unresolved imports (1):
  test/Contract.t.sol:4  "forge-std/Test.sol"
```

A second invocation against the same ref hits the cache (sub-second). The
`forge-std/Test.sol` import is expected: the template relies on
`forge install` to populate `lib/forge-std/`. Run `forge install` inside
the cached working tree (printed in the header line) before re-running
Basilisk if you want full resolution.

Inspect or prune the repo cache:

```console
$ audit cache repos stats
repo cache: ~/.basilisk/repos
repos: 1
total: 1.2 MB
oldest: 3m ago
newest: 3m ago

$ audit cache repos list
owner/repo                               sha        depth    cloned
---------------------------------------- ---------- -------- ----------------
foundry-rs/forge-template                f5db6aee   shallow  3m ago

$ audit cache repos clear --owner foundry-rs
cleared foundry-rs/*: 1.2 MB freed
```

### Source, from local path

```console
$ audit recon crates/project/tests/fixtures/foundry-minimal

Project at crates/project/tests/fixtures/foundry-minimal
  kind: foundry
  configs: foundry.toml
  solc: 0.8.20
  remappings: 2
  sources: 2 file(s)
  tests: 1 file(s)
  missing dirs: 1
    - script
  imports: 4 resolved, 0 unresolved (0 file(s) with unresolved)
  externals: 2 file(s) reached via imports (deps)
```

The local-path pipeline is identical to the GitHub pipeline after the
clone — same layout detector, same config parsers, same import graph.
Points at any directory containing a `foundry.toml`, `hardhat.config.*`, or
`truffle-config.js`.

## Architecture

```
crates/
├── cli/         command-line interface, clap-derived subcommands
├── core/        target detection, chain abstraction, error types
├── cache/       on-disk KV cache (RPC results, explorer responses, etc.)
├── rpc/         EVM RPC client (alloy, multi-chain, retry-aware)
├── explorers/   verified-source resolution (Sourcify, Etherscan V2, Blockscout)
├── onchain/     on-chain ingestion orchestrator — proxy detection, graph expansion
├── graph/       typed contract graph, cycle detection, DOT export
├── github/      thin GitHub REST client (reqwest, rustls)
├── git/         shallow clone with persistent cache, ref resolution (git2)
├── project/     source project analysis — config parsing, enumeration, imports
└── logging/     tracing setup
```

Each crate is independently testable and depends only on crates below it in
the layering. The CLI composes the full pipeline; downstream agent phases
will reuse these crates without modification.

## Development

**Build and test.**

```bash
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

**Live tests.** `cargo test --workspace -- --ignored` runs opt-in
live-network tests (real GitHub clones, real mainnet RPC). Requires the
corresponding API keys in `.env`.

**Graph visualization.** Runs using `--dot <path>` emit a graphviz DOT
file; render with:

```bash
audit recon 0x87870... --chain ethereum --dot /tmp/aave.dot
dot -Tpng /tmp/aave.dot -o /tmp/aave.png
```

Install graphviz via `brew install graphviz` (macOS) or
`apt install graphviz` (Debian/Ubuntu).

**Caches.** `audit cache stats` lists the RPC / explorer / GitHub-API
namespaces with entry counts and byte totals. `audit cache repos stats`
does the same for cloned repos. `audit cache clear` and
`audit cache repos clear` reclaim space. Cache roots:

- KV caches: `dirs::cache_dir()/basilisk/` —
  `~/.cache/basilisk/` on Linux, `~/Library/Caches/basilisk/` on macOS.
- Git clones: `~/.basilisk/repos/`.

## Security and scope

- **No broadcasting.** Basilisk does not and will not send transactions to
  live networks. All execution is forked simulation. This is
  architecturally enforced: no private-key handling, no wallet integration,
  no broadcast endpoints in any crate.
- **No training on your data.** Findings produced by Basilisk are stored
  locally in your knowledge base (Phase 4+); they are not uploaded to any
  training pipeline. If using API-based LLMs, model providers' data
  retention policies apply — Anthropic's and OpenAI's API terms explicitly
  exclude API inputs from training.
- **Private repos.** Supported via `GITHUB_TOKEN` with fine-grained scopes.
  The token never appears in logs, JSON output, error messages, or cached
  metadata. Clone credentials are not persisted.

## License and contributing

MIT (see [LICENSE](LICENSE)). Contributions welcome — open a pull request
with a focused change and a test that demonstrates the before/after.

## Acknowledgements

Built on top of [alloy](https://github.com/alloy-rs/alloy) for Ethereum
primitives, [Foundry](https://github.com/foundry-rs/foundry) for test
execution (Phase 5), [git2](https://github.com/rust-lang/git2-rs) /
libgit2, the [Sourcify](https://sourcify.dev/),
[Etherscan](https://etherscan.io/), and [Blockscout](https://www.blockscout.com/)
teams for verified-source infrastructure, and
[Anthropic](https://anthropic.com) for Claude.
