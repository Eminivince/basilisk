# Basilisk

> Project name: **Basilisk** — the mythical creature whose gaze was said to be
> fatal. A fitting handle for a tool whose job is to stare at smart contracts
> until their flaws surface. Short, memorable, not already claimed by a major
> security tool.

Basilisk is an AI-driven smart-contract auditor for the EVM. Point it at a
GitHub repository or an on-chain address and it will pull the sources, walk
across multi-repo layouts, resolve proxies to their implementations, run
static analysis, reason over the findings with an LLM agent loop, and — when
a vulnerability is plausible — reproduce it as a runnable proof-of-concept.

**Project status — Phase 1: scaffolding. No audit logic yet.** This commit
establishes the Cargo workspace that later phases will build on.

## Prerequisites

- Rust stable (pinned via `rust-toolchain.toml`). Install with
  [rustup](https://rustup.rs/); the pinned toolchain will be fetched
  automatically on first build.

That is the full prerequisite list today. Later phases will depend on
Foundry, Heimdall, Aderyn, and friends — but as Rust crates compiled into
the single `audit` binary, not as external tools you have to install.

## Quickstart

```sh
cargo build                      # debug build
cargo build --release            # single statically-linked `audit` binary
cargo run -- --help              # top-level help
cargo run -- recon 0x1234...     # phase-1 stub: logs the target, exits 0
cargo test                       # runs the smoke test
```

The release binary lands at `target/release/audit`.

## Architecture

Single-binary tool delivered as a Cargo workspace. Today the workspace
contains three crates; later phases slot in beside them without reshaping
the tree.

```
crates/
├── cli/        — the `audit` binary: argument parsing, subcommand dispatch.
├── core/       — shared types (Target, Config, Error) used by every other crate.
└── logging/    — tracing-subscriber setup, shared across the binary and tests.
```

Planned crates (arriving in later instruction sets, one at a time):

- `recon` — turn arbitrary inputs (URLs, addresses, paths) into a typed `Target`.
- `static-analysis` — Aderyn/Slither-style checks over Solidity sources.
- `decompiler` — Heimdall-based decompilation for unverified on-chain targets.
- `agent` — the LLM-driven reasoning loop that plans, tools, and reports.
- `rag` — indexed retrieval over historical incidents and docs.
- `exploit-synth` — synthesize and run Foundry-style PoCs.

## Configuration

Copy `.env.example` to `.env` and fill in the keys your workflow needs.
Every key is optional at startup; features that require a specific key fail
loudly at the point of use, not on launch.

## Development

```sh
cargo fmt-check                  # formatting gate
cargo lint                       # clippy with -D warnings
cargo test-all                   # whole-workspace test run
```

Cargo aliases can't chain multiple cargo subcommands, so CI is spelled as
three separate aliases rather than a single `cargo ci`.

## License

MIT — see [LICENSE](./LICENSE).
