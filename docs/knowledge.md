# Knowledge Base

The knowledge base (KB) is a semantic search index over historical audit findings, vulnerability patterns, and protocol documentation. The agent uses it during analysis to recall similar vulnerabilities and relevant patterns.

## How it works

```
Corpus (markdown, JSONL)
  → chunk + normalize
  → embed (Voyage voyage-code-3)
  → store in FileVectorStore
  → search: embed query → cosine similarity → top-k hits
```

The KB lives at `~/.basilisk/knowledge/`. The vector store is a JSON snapshot (LanceDB migration planned for large corpora).

## Ingest sources

Run `audit knowledge ingest --all` to seed the KB on first install.

### Solodit

Public smart-contract vulnerability database. Requires `solodit_dump.jsonl` placed at `~/.basilisk/knowledge/solodit_dump.jsonl`.

```bash
audit knowledge ingest solodit
```

### SWC (Smart Contract Weakness Classification)

Standard vulnerability taxonomy: integer overflow, reentrancy, access control, etc.

```bash
audit knowledge ingest swc
```

### OpenZeppelin Security Advisories

Official OZ security reports and library-level vulnerability notices.

```bash
audit knowledge ingest openzeppelin
```

### Code4rena

Clones finding repositories from `code-423n4/<contest>-findings` on GitHub. Requires `GITHUB_TOKEN` for rate limits.

```bash
audit knowledge ingest code4rena
```

### Sherlock

Clones Sherlock audit report repositories.

```bash
audit knowledge ingest sherlock
```

### rekt.news

Post-mortems from rekt.news. Requires an operator-curated `rekt.jsonl` file.

```bash
audit knowledge ingest rekt
```

### Trail of Bits

Security writeups and advisories. Requires an operator-curated `trailofbits.jsonl` file.

```bash
audit knowledge ingest trailofbits
```

## Search

```bash
audit knowledge search "reentrancy via ERC777 callback"
audit knowledge search "flash loan oracle manipulation" --limit 10
audit knowledge search "storage collision proxy upgrade"
```

Each result shows: similarity score, source, title, and a snippet.

## Protocol documentation

Index protocol-specific docs so the agent can search them during analysis:

```bash
# From a GitHub repo (reads markdown, Solidity, and docs)
audit knowledge add-protocol uniswap-v3 \
  --github https://github.com/Uniswap/v3-core

# From a PDF (extracted and chunked)
audit knowledge add-protocol aave-v3 \
  --pdf ./aave-v3-technical-paper.pdf

# From a URL (page content extracted)
audit knowledge add-protocol curve \
  --url https://docs.curve.fi
```

Protocol docs are stored in a separate collection and searched by `search_protocol_docs` during analysis.

## Findings management

The KB records findings that have been surfaced by the agent. Operators can curate these:

```bash
# List all findings
audit knowledge list-findings

# Show one in detail
audit knowledge show-finding <id>

# Curate
audit knowledge correct <id> --reason "false positive — checked manually"
audit knowledge dismiss <id>
audit knowledge confirm <id>
```

Corrections are stored as amendments — the original entry is preserved with an `is_correction` flag pointing to the corrected version.

## Ingest state

The KB tracks what has been ingested in `~/.basilisk/knowledge/ingest_state.json`. Re-running ingest is idempotent — previously ingested entries are skipped unless the corpus has changed.

## Embedding providers

| Provider | Model | Notes |
|---|---|---|
| Voyage | `voyage-code-3` | Primary; best for code + security text |
| OpenAI | `text-embedding-3-small` | Fallback |
| Ollama | Configurable | Local / offline |

Set `EMBEDDINGS_PROVIDER` to override auto-selection. Changing providers after ingest requires re-embedding (different vector spaces).

## Storage

| Path | Contents |
|---|---|
| `~/.basilisk/knowledge/store.json` | Vector store (all collections) |
| `~/.basilisk/knowledge/ingest_state.json` | Ingest progress per source |
| `~/.basilisk/knowledge/solodit_dump.jsonl` | Solodit input corpus (operator-provided) |
