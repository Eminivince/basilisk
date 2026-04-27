# Basilisk

An AI-driven smart-contract auditor that reasons about protocols end-to-end —
from deployed bytecode to GitHub source, with cross-contract graph awareness.

**Status:** Phase 4 open — vulnerability reasoning + PoC synthesis +
evaluation harness shipped (Set 9 / 9.5). Given a GitHub URL, a deployed
address, or a local path, Basilisk runs an LLM-driven tool-use loop that
resolves the system, hypothesizes vulnerabilities, simulates against a
forked mainnet, optionally writes and runs a Foundry test as proof, and
records suspicions and limitations to a queryable knowledge base. See
`reports/SET-9.md` and `reports/wave-launcher-trial.md` for honest
calibration data.

## What it does

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
./target/release/basilisk recon 0xdeaDbeefdeadbeefbeefbeefbeefdeadbeef --chain ethereum
./target/release/basilisk recon https://github.com/foundry-rs/forge-template
./target/release/basilisk recon ./path/to/foundry-project
```

To install `basilisk` as a system binary: `cargo install --path crates/cli`.

Browse the built-in documentation at any time:

```bash
basilisk doc --open        # serves http://localhost:3000 and opens your browser
```

## Configuration

| Variable              | What it enables                                                                                                        | Without it                                                    |
| --------------------- | ---------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------- |
| `ANTHROPIC_API_KEY`   | Default LLM provider for `basilisk recon <target>`                                                                | Pick another `--provider` or fall back to deterministic recon |
| `OPENROUTER_API_KEY`  | Agent routing via `--provider openrouter` (any Claude/GPT/Gemini/Llama model)                                          | Use a different provider                                      |
| `OPENAI_API_KEY`      | Agent routing via `--provider openai`; fallback key for `--provider openai-compat`; also used as an embedding provider | Use a different provider or supply `--llm-api-key-env`        |
| `VOYAGE_API_KEY`      | Primary embedding provider for `basilisk knowledge` (voyage-code-3)                                                       | Falls back to OpenAI or local Ollama                          |
| `OLLAMA_HOST`         | Local Ollama endpoint for embeddings (`nomic-embed-text`, fully offline)                                               | Defaults to `http://localhost:11434`                          |
| `EMBEDDINGS_PROVIDER` | Explicit `voyage`\|`openai`\|`ollama` override                                                                         | Picks the first configured provider                           |
| `ALCHEMY_API_KEY`     | Primary RPC for supported chains                                                                                       | Falls back to `RPC_URL_<CHAIN>` or public RPC                 |
| `ETHERSCAN_API_KEY`   | Verified source, creation-tx lookup, multi-chain via Etherscan V2                                                      | Falls back to Sourcify and Blockscout                         |
| `GITHUB_TOKEN`        | 5000/hour API rate limit, private-repo access, authenticated clones                                                    | 60/hour unauthenticated                                       |
| `RPC_URL_<CHAIN>`     | Override RPC for a specific chain (e.g. `RPC_URL_BNB=...`)                                                             | Uses Alchemy if supported, else public                        |
| `LOG_LEVEL`           | `error`, `warn`, `info`, `debug`, `trace`; accepts any `tracing` filter                                                | Defaults to `info`                                            |

**Upgrade-history caveat.** Upgrade history requires an RPC provider that
permits large `eth_getLogs` queries. Alchemy's free tier caps at 10-block
ranges, so history degrades to a logged warning on that tier and the output
records `upgrade-history unavailable`. Paid Alchemy, QuickNode, or a
self-hosted node resolves this — point `RPC_URL_ETHEREUM` at it.

## Demo

### On-chain

```console
$ basilisk recon 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2 --chain ethereum

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
$ basilisk recon https://github.com/foundry-rs/forge-template

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
$ basilisk cache repos stats
repo cache: ~/.basilisk/repos
repos: 1
total: 1.2 MB
oldest: 3m ago
newest: 3m ago

$ basilisk cache repos list
owner/repo                               sha        depth    cloned
---------------------------------------- ---------- -------- ----------------
foundry-rs/forge-template                f5db6aee   shallow  3m ago

$ basilisk cache repos clear --owner foundry-rs
cleared foundry-rs/*: 1.2 MB freed
```

### Source, from local path

```console
$ basilisk recon crates/project/tests/fixtures/foundry-minimal

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

### `basilisk recon` — LLM-driven auditor

`basilisk recon <target>` runs an LLM-driven tool-use loop against the
target. Recon-mode by default; pass `--vuln` to switch into
vulnerability-hunting mode (Set 9.5).


The agent calls a registry of tools — 14 in recon mode, **25 in
`--vuln` mode** (recon tools + knowledge-base retrieval + analytical
wrappers like `find_callers_of` / `simulate_call_chain` + the three
self-critique tools that drive structured-recording discipline).
Output is a markdown brief in the agent's voice.

```console
$ basilisk recon 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 \
    --max-turns 20 \
    --max-cost 100

→ agent running  target="0xA0b86991...48"  model=anthropic/claude-opus-4-7  budget=Budget { ... }
  session db: /Users/you/.basilisk/sessions.db
── session 7a1c2f90-…-0b3e ──
━━ turn 1 ━━
I'll start by classifying this target.
  ↳ calling classify_target
  ↳ classify_target  ok  (4ms)
━━ turn 2 ━━
This is an on-chain address on Ethereum mainnet. Let me pull the system.
  ↳ calling resolve_onchain_system
  ↳ resolve_onchain_system  ok  (2340ms)
━━ turn 3 ━━
USDC is a proxy. I'll finalize the brief.
  ↳ calling finalize_report
  ↳ finalize_report  ok  (1ms)

── agent session: COMPLETED ──
stop_reason: report_finalized
stats: 3 turns, 3 tool calls, 24500 tokens, ~37¢, 42000ms

── final report (High) ──
# USDC Recon Brief
…
```

Every session is persisted to `~/.basilisk/sessions.db`. Inspect, resume,
or delete via:

```bash
basilisk session list
basilisk session show <id>
basilisk session show <id> --report-only      # just the markdown
basilisk session show <id> --format json      # machine-readable full transcript
basilisk session resume <id>                  # continue an interrupted run
basilisk session delete <id> --yes            # remove from the DB
```

**Budgets.** The agent will stop cleanly the moment any of the four
caps trip (`--max-turns`, `--max-tokens`, `--max-cost`, `--agent-max-duration`).
Defaults: 40 turns / 500k tokens / $5 / 20 min. The session row is
marked `interrupted`; pick it up with `session resume`.

**Prompt iteration.** The shipped system prompt lives at
`crates/agent/src/prompts/recon_v1.md` and is embedded at build
time. Point `--system-prompt <path>` at a working copy to iterate
without a rebuild.

**Choosing a provider.** `--provider` selects the LLM backend:

| `--provider`            | Endpoint                    | Key env var                          | Notes                                                                |
| ----------------------- | --------------------------- | ------------------------------------ | -------------------------------------------------------------------- |
| `anthropic` _(default)_ | `api.anthropic.com`         | `ANTHROPIC_API_KEY`                  | Native Claude.                                                       |
| `openrouter`            | `openrouter.ai/api/v1`      | `OPENROUTER_API_KEY`                 | Any Claude / GPT / Gemini / Llama model OpenRouter proxies.          |
| `openai`                | `api.openai.com/v1`         | `OPENAI_API_KEY`                     | Native OpenAI.                                                       |
| `ollama`                | `http://localhost:11434/v1` | none                                 | Local models (Llama, Qwen, DeepSeek, …).                             |
| `openai-compat`         | `--llm-base-url <url>`      | `--llm-api-key-env <VAR>` (optional) | Any OpenAI-compatible server: `llama.cpp`, LM Studio, LocalAI, vLLM. |

Examples:

```bash
# OpenRouter, routed to Claude Opus under the hood:
basilisk recon <target> \
  --provider openrouter \
  --model anthropic/claude-opus-4-7

# Local Ollama running Llama 3.1 70B:
basilisk recon <target> \
  --provider ollama \
  --model llama3.1:70b

# llama.cpp server on a custom port:
basilisk recon <target> \
  --provider openai-compat \
  --llm-base-url http://localhost:8080/v1 \
  --model qwen2.5-coder-32b

# OpenAI GPT-4o:
basilisk recon <target> \
  --provider openai --model gpt-4o
```

The transcript, cost accounting, and session persistence are provider-
neutral — `basilisk session list` / `show <id>` work identically across
providers. The session row records `model = "<provider>/<model>"` so
a mixed DB remains attributable.

**Setting defaults in `.env`.** Every agent flag has an env-var twin,
so you can put provider + model + budgets in `.env` once and drop the
flags from every invocation:

```bash
# .env
BASILISK_LLM_PROVIDER=openrouter
BASILISK_LLM_MODEL=anthropic/claude-opus-4-7
BASILISK_MAX_COST_CENTS=300
```

```bash
# now this just works:
basilisk recon <target>
```

CLI flags override env vars, so one-off tweaks stay cheap: `basilisk recon
<target> --model openai/gpt-4o`. The full list of `BASILISK_*`
variables is documented in `.env.example`.

#### Vulnerability-hunt mode (`--vuln`)


```bash
basilisk recon 0xB9873b482d51b8b0f989DCD6CCf1D91520092b95 \
  --chain ethereum \
  --vuln \
  --provider openrouter \
  --model anthropic/claude-sonnet-4-6 \
  --session-note "wave-launcher vuln hunt"
```

What `--vuln` flips:

- **Registry** → 25 tools (recon's 14 + 4 knowledge-base retrieval +
  4 analytical wrappers like `find_callers_of`,
  `trace_state_dependencies`, `simulate_call_chain`,
  `build_and_run_foundry_test` + 3 self-critique tools).
- **System prompt** → `vuln_v2.md` (Set 9.5; ~2,400 words; three
  phases — Discovery → Investigation → Synthesis — plus a
  non-negotiable "structured recording" section that makes
  `record_suspicion` and `record_limitation` calls mandatory for
  hunches and walls).
- **Default model** → Claude Sonnet 4.6. Faster + cheaper than Opus
  (~$3-5 vs ~$25 on a typical novel target). Opt into Opus for
  high-stakes targets via `--model claude-opus-4-7`.
- **Default budget** → 100 turns / 2M tokens / $50 / 1h.
- **Exec backend** → anvil-spawned forks for `simulate_call_chain`
  and `build_and_run_foundry_test`. Requires Foundry on `$PATH` and
  `MAINNET_RPC_URL` or `ALCHEMY_API_KEY`.
- **Ordering rail** → `finalize_self_critique` mandatory before
  `finalize_report`. Runner blocks the first finalize attempt with a
  retryable nudge; second attempt force-injects a stub critique.

The `~/.basilisk/feedback/` directory accumulates structured records
across sessions:

```bash
cat ~/.basilisk/feedback/limitations.jsonl   # walls hit, by session
cat ~/.basilisk/feedback/suspicions.jsonl    # hunches the agent surfaced
cat ~/.basilisk/feedback/self_critiques.jsonl # per-session reflections
```

`scripts/calibrate-vuln.sh` re-runs WaveLauncher with Sonnet and
prints a summary against the recorded Opus baseline (Set 9 Run C,
$25.12 / 9 min) for cost-vs-quality calibration.

#### Benchmark — `basilisk bench`

```bash
basilisk bench list                            # 5 calibration targets
basilisk bench show euler-2023                 # full target dossier
basilisk bench run visor-2021                  # spawn a vuln session, score it
basilisk bench run                             # all 5 sequentially
basilisk bench history                         # newest-first run log
basilisk bench score <run-id>                  # re-score against current expectations
basilisk bench compare <run-a> <run-b>         # side-by-side diff
basilisk bench review <run-id>                 # interactively label misses + false positives
```

`basilisk bench review` walks every miss and false positive from a recorded
run and asks the operator to label each — `actual_miss`,
`scoring_failure`, `false_positive`, `wrongly_flagged`, or
`in_scope_extra`. Verdicts persist in `bench_review_verdicts` so re-runs
resume where you left off. Use `scripts/feedback-summary.sh` to see
recurring miss-classes and the agent's self-reported limitations across
sessions.

Five targets shipped: Euler (donation + self-liquidation), Visor
(reentrancy via owner callback), Cream (oracle manipulation),
Beanstalk (governance via flash-loaned voting weight), Nomad
(zero-root replay). Each pinned at the block immediately before the
exploit. Scoring is heuristic keyword matching against
`expected_findings`; full rationale per target in
`crates/bench/src/targets.rs`.

Local-model caveat: tool-use quality varies significantly by model.
A 7B-class model rarely completes a recon brief without supervision.
The 70B-class Llamas / Qwens may work for small trivial contracts with little dependency graph. But the Opus 4.7 is near perfect for now.

**Live tests.** Three `#[ignore]`-d tests (`crates/agent/tests/agent_live.rs`)
exercise the full path against real targets (`forge-template`, USDC,
Aave V3 Pool). They cost real money — run explicitly:

```bash
cargo test -p basilisk-agent --test agent_live -- --ignored --nocapture
```

### Knowledge base

Set 7 adds a persistent, user-owned knowledge substrate under
`~/.basilisk/knowledge/`. The agent gains four retrieval tools
(`search_knowledge_base`, `search_similar_code`, `search_protocol_docs`,
`record_finding`); the operator gets `basilisk knowledge` to curate the
corpus by hand.

Three layers are supported:

1. **External corpus** — Solodit dump, the SWC registry, and OpenZeppelin
   security advisories.
2. **Protocol context** — per-engagement docs ingested from a URL, a PDF,
   a local file, or a GitHub directory.
3. **Findings memory** — the agent's own accumulated findings plus
   human-authored corrections, dismissals, and confirmations.

```console
# what's stored, where, and which provider embedded it:
$ basilisk knowledge stats

# seed the external corpus:
$ basilisk knowledge ingest solodit --max-records 1000
$ basilisk knowledge ingest swc
$ basilisk knowledge ingest openzeppelin
$ basilisk knowledge ingest code4rena       # set 9.6: github-based contest archive
$ basilisk knowledge ingest sherlock        # set 9.6: github-based audit reports
$ basilisk knowledge ingest rekt            # set 9.6: operator-curated post-mortems
$ basilisk knowledge ingest trailofbits     # set 9.6: operator-curated security writeups
$ basilisk knowledge ingest --all

# attach engagement-specific context:
$ basilisk knowledge add-protocol aave-v3 --github aave/aave-v3-core:docs
$ basilisk knowledge add-protocol aave-v3 --pdf ./aave-v3-whitepaper.pdf
$ basilisk knowledge add-protocol aave-v3 --url https://docs.aave.com/developers/

# natural-language retrieval:
$ basilisk knowledge search "reentrancy via erc777 callback"
$ basilisk knowledge search "rounding direction" --collection public_findings

# findings memory — the agent writes, the operator curates:
$ basilisk knowledge list-findings
$ basilisk knowledge show-finding <id>
$ basilisk knowledge correct <id>  --reason "actually unreachable on mainnet — liquidity guard catches it"
$ basilisk knowledge dismiss <id>  --reason "false positive — invariant is maintained by the caller"
$ basilisk knowledge confirm <id>
```

Corrections are stored as sibling rows in the `user_findings` collection
(columns: `is_correction`, `corrects_id`, `correction_reason`,
`user_verdict`). Retrieval surfaces them alongside the original finding,
so the next run benefits from the human verdict without any separate
"teach the agent" loop.

**Solodit ingester.** Solodit puts content behind Cloudflare, so the
ingester reads a user-supplied JSONL dump at
`~/.basilisk/knowledge/solodit_dump.jsonl` (one finding per line) rather
than scraping live. See the fixture shape in
`crates/ingest/tests/fixtures/solodit/`.

**Set 9.6 ingesters.** Four additional sources joined the corpus:

- `code4rena` — clones `code-423n4/<contest>-findings` repositories and
  parses both per-finding `data/<auditor>-<sev>-<num>.md` files and
  consolidated `report.md` shapes. Walks the `DEFAULT_CONTESTS` curated
  list out of the box (override via `with_contests`).
- `sherlock` — clones `sherlock-protocol/sherlock-reports` and walks
  each audit subdirectory's `README.md` for `## Issue H-1: title`-style
  headings.
- `rekt` — operator-curated JSONL at
  `~/.basilisk/knowledge/rekt_dump.jsonl` (post-mortems with loss
  amounts, attack vectors, chain). Loss is bucketed
  (`<1m / 1m_10m / 10m_100m / >100m`) for retrieval filtering.
- `trailofbits` — operator-curated JSONL at
  `~/.basilisk/knowledge/tob_dump.jsonl` covering the security-relevant
  subset of the Trail of Bits blog (the full blog is wider; the
  operator curates which posts are smart-contract-relevant).

**Embedding providers** (configure via env or `.env`):

| Variable              | What it enables                                                                 | Without it                           |
| --------------------- | ------------------------------------------------------------------------------- | ------------------------------------ |
| `VOYAGE_API_KEY`      | Primary embedding provider (`voyage-code-3`, 1024 dims)                         | Falls back to OpenAI or Ollama       |
| `OPENAI_API_KEY`      | `nvidia/llama-nemotron-embed-vl-1b-v2:free` (3072 dims) — also used for the LLM | Falls back to Ollama if present      |
| `OLLAMA_HOST`         | Local Ollama endpoint (`nomic-embed-text`, 768 dims) — fully offline            | Defaults to `http://localhost:11434` |
| `EMBEDDINGS_PROVIDER` | Explicit `voyage`\|`openai`\|`ollama` override                                  | Picks the first configured provider  |

Changing providers changes the vector dimension. Each collection carries
a `schema_version` + `embedding_dim` in its metadata; mismatched writes
are refused with a pointer to `basilisk knowledge reembed <collection>`
(landing in a follow-up set).

**Interim persistence.** The shipping store is a JSON file at
`~/.basilisk/knowledge/store.json`, plus an ingest-state file alongside.
The `VectorStore` trait is backend-neutral — the LanceDB-backed
implementation lands in a follow-up set (tracked in `ROADMAP.md`). The
JSON store is good for hundreds-to-thousands of records; swap is
transparent to callers.

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
├── llm/         model-agnostic LlmBackend trait + Anthropic impl + SSE streaming
├── embeddings/  EmbeddingProvider trait + Voyage / OpenAI / Ollama backends
├── vector/      VectorStore trait + collection specs + JSON-backed interim store
├── ingest/      Solodit / SWC / OpenZeppelin / protocol-docs ingesters
├── knowledge/   KnowledgeBase public API — retrieval, findings, corrections
├── agent/       tool definitions, tool-use loop, sessions (SQLite), prompts
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
basilisk recon 0x87870... --chain ethereum --dot /tmp/aave.dot
dot -Tpng /tmp/aave.dot -o /tmp/aave.png
```

Install graphviz via `brew install graphviz` (macOS) or
`apt install graphviz` (Debian/Ubuntu).

**Caches.** `basilisk cache stats` lists the RPC / explorer / GitHub-API
namespaces with entry counts and byte totals. `basilisk cache repos stats`
does the same for cloned repos. `basilisk cache clear` and
`basilisk cache repos clear` reclaim space. Cache roots:

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
