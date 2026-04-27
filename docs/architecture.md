# Architecture

Basilisk is a Rust workspace with 21 crates. Dependencies flow strictly downward — no circular imports.

## Crate map

```
audit (binary)
└── basilisk-cli
    ├── basilisk-agent          LLM agent loop + tool registry + sessions
    │   ├── basilisk-analyze    Static/dynamic analysis tools
    │   ├── basilisk-exec       EVM simulation (Anvil forks)
    │   ├── basilisk-onchain    On-chain resolution + proxy detection
    │   ├── basilisk-project    Source project detection (Foundry/Hardhat)
    │   ├── basilisk-scratchpad Agent working memory (SQLite)
    │   └── basilisk-knowledge  Knowledge base (search, findings)
    │       ├── basilisk-embeddings  Embedding providers
    │       └── basilisk-vector      Vector store
    ├── basilisk-ingest         KB corpus ingestion pipelines
    ├── basilisk-bench          Calibration harness
    ├── basilisk-cache          On-disk TTL cache
    ├── basilisk-git            Git shallow clone + cache
    ├── basilisk-github         GitHub REST v3 client
    ├── basilisk-rpc            RPC provider + alloy integration
    ├── basilisk-explorers      Verified-source resolvers
    ├── basilisk-graph          ContractGraph data structure
    ├── basilisk-llm            LLM backend trait + implementations
    ├── basilisk-logging        tracing setup
    └── basilisk-core           Config, Target, Chain, Error types
```

## Dependency design

Every public abstraction is trait-based, which keeps each layer testable in isolation with fast in-memory mocks:

| Trait | Location | Implementations |
|---|---|---|
| `LlmBackend` | `basilisk-llm` | `AnthropicBackend`, `OpenAICompatibleBackend` |
| `EmbeddingProvider` | `basilisk-embeddings` | `VoyageBackend`, `OpenAI`, `Ollama`, `Batching` |
| `VectorStore` | `basilisk-vector` | `FileVectorStore`, `MemoryVectorStore` |
| `RpcProvider` | `basilisk-rpc` | `AlloyProvider`, `MemoryProvider` |
| `SourceExplorer` | `basilisk-explorers` | `Sourcify`, `EtherscanV2`, `Blockscout` |
| `ExecutionBackend` | `basilisk-exec` | `AnvilForkBackend`, `MockExecutionBackend` |
| `Ingester` | `basilisk-ingest` | Seven corpus adapters |
| `Tool` | `basilisk-agent` | 25 individual tool structs |

## Persistence

| Store | Path | Format |
|---|---|---|
| Sessions, turns, tool calls | `~/.basilisk/sessions.db` | SQLite (bundled) |
| Scratchpad revisions | (same DB) | SQLite |
| Bench run history | (same DB) | SQLite |
| Git repos | `~/.basilisk/repos/` | Bare git + metadata JSON |
| Knowledge vectors | `~/.basilisk/knowledge/store.json` | JSON (LanceDB planned) |
| RPC/explorer cache | `~/.cache/basilisk/` | Filesystem TTL (atomic writes) |

## Key data flows

### On-chain audit

```
audit recon 0x...
  → core::detect() → Target::OnChain
  → onchain::resolve_system()
      → rpc: fetch bytecode
      → explorers: fetch verified source (Sourcify → Etherscan → Blockscout)
      → onchain: detect proxies (EIP-1967 / 1167 / 2535)
      → graph: ContractGraph with typed edges
  → agent: run tool loop
      → tool: ReadFile, GrepProject, SearchKnowledgeBase, ...
      → llm: stream completion
  → session: persist turns + tool calls
  → report: Markdown brief
```

### Knowledge base search

```
audit knowledge search "..."
  → embeddings: embed query (Voyage/OpenAI/Ollama)
  → vector: cosine similarity over FileVectorStore
  → knowledge: rank + deduplicate hits
  → CLI: print table
```

## Async runtime

Everything runs on Tokio (`"full"` feature). Long I/O operations — RPC calls, LLM streaming, Git clones — are concurrent. The agent loop itself is sequential (one LLM turn at a time) but individual tool calls can be parallel.

## Error handling

`anyhow::Result` at the CLI boundary; typed `thiserror` enums (`LlmError`, `RpcError`, `ExplorerError`, etc.) inside crates. Errors are propagated with `?` and enriched with context at each level.
