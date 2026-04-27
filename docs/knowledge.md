# Knowledge Base

The knowledge base (KB) is a semantic search index over historical audit findings, vulnerability patterns, and protocol documentation. The agent uses it during `--vuln` analysis to recall similar vulnerabilities and relevant precedents, and you can query it directly with `basilisk knowledge search`.

## How it works

```
Corpus (markdown, JSONL)
  → chunk + normalize
  → embed (Voyage voyage-code-3 / OpenAI / Ollama)
  → store in FileVectorStore (~/.basilisk/knowledge/)
  → search: embed query → cosine similarity → top-k hits
```

The KB lives at `~/.basilisk/knowledge/`. The vector store is a JSON snapshot (LanceDB migration planned for large corpora).

---

## Quick start (first-time setup)

```bash
# 1. Set an embedding provider in .env (Voyage recommended)
echo "VOYAGE_API_KEY=pa-..." >> .env

# 2. Seed the public corpus (runs in ~10–30 min depending on provider speed)
basilisk knowledge ingest swc            # fast — ~50 entries, no API required for source
basilisk knowledge ingest openzeppelin   # fast — OZ security advisories
basilisk knowledge ingest code4rena      # slow — clones GitHub repos; needs GITHUB_TOKEN
basilisk knowledge ingest sherlock       # moderate — Sherlock audit reports
basilisk knowledge ingest --all          # everything above + solodit/rekt/trailofbits if available

# 3. Check what was indexed
basilisk knowledge stats

# 4. Search to confirm it's working
basilisk knowledge search "reentrancy via ERC777 callback"
```

---

## Ingest sources

### SWC (Smart Contract Weakness Classification)

Standard vulnerability taxonomy covering integer overflow, reentrancy, access control, and ~40 other categories. No extra files required.

```bash
basilisk knowledge ingest swc
```

### OpenZeppelin Security Advisories

Official OZ library-level security reports and upgrade notices. No extra files required.

```bash
basilisk knowledge ingest openzeppelin
```

### Code4rena

Clones `code-423n4/<contest>-findings` repositories from GitHub and indexes per-finding markdown files. Covers hundreds of contests.

**Requires:** `GITHUB_TOKEN` (for API rate limits — the free 60 req/hr unauthenticated limit is too low for bulk cloning).

```bash
basilisk knowledge ingest code4rena
# Tip: use --max-records N to cap how many findings are indexed
basilisk knowledge ingest code4rena --max-records 5000
```

### Sherlock

Clones `sherlock-protocol/sherlock-reports` and indexes each audit's `README.md` findings.

**Requires:** `GITHUB_TOKEN`.

```bash
basilisk knowledge ingest sherlock
```

### Solodit

Public smart-contract vulnerability database. Solodit gates content behind Cloudflare, so the ingester reads a local JSONL dump instead of scraping live.

**Requires:** Place `solodit_dump.jsonl` at `~/.basilisk/knowledge/solodit_dump.jsonl` (one finding per line). See `crates/ingest/tests/fixtures/solodit/` for the expected field shape.

```bash
basilisk knowledge ingest solodit
```

### rekt.news

Post-mortems with loss amounts, attack vectors, and chain metadata. Loss is bucketed (`<1m / 1m_10m / 10m_100m / >100m`) for retrieval filtering.

**Requires:** Place an operator-curated `rekt_dump.jsonl` at `~/.basilisk/knowledge/rekt_dump.jsonl`.

```bash
basilisk knowledge ingest rekt
```

### Trail of Bits

Security writeups and advisories from the Trail of Bits blog. You curate which posts are smart-contract-relevant.

**Requires:** Place an operator-curated `tob_dump.jsonl` at `~/.basilisk/knowledge/tob_dump.jsonl`.

```bash
basilisk knowledge ingest trailofbits
```

---

## Adding protocol documentation

Index protocol-specific docs before auditing a target so the agent can search them with `search_protocol_docs`. Use any combination of sources.

### From a GitHub repository

Clones the repo and indexes all `.md` and `.sol` files under the specified path. Useful for protocol spec docs, design docs, and developer guides.

```bash
# Index the entire repo
basilisk knowledge add-protocol uniswap-v3 \
  --github https://github.com/Uniswap/v3-core

# Index only a subdirectory (e.g. docs/ or contracts/)
basilisk knowledge add-protocol aave-v3 \
  --github aave/aave-v3-core:docs
```

### From a PDF

Extracts and chunks text from a whitepaper or technical specification.

```bash
basilisk knowledge add-protocol aave-v3 \
  --pdf ./aave-v3-technical-paper.pdf
```

### From a URL

Fetches and extracts the page content. Works for documentation sites and blog posts.

```bash
basilisk knowledge add-protocol curve \
  --url https://docs.curve.fi

basilisk knowledge add-protocol compound \
  --url https://docs.compound.finance
```

Protocol docs are stored in a separate collection (`protocol_docs`) and are not mixed with public findings during retrieval.

---

## Search

Query the KB with natural language. Works against all collections by default; narrow with `--collection`.

```bash
basilisk knowledge search "reentrancy via ERC777 callback"
basilisk knowledge search "flash loan oracle manipulation" --limit 10
basilisk knowledge search "storage collision proxy upgrade"
basilisk knowledge search "rounding error lending protocol" --collection public_findings
basilisk knowledge search "aave liquidation" --collection protocol_docs
```

Each result shows: similarity score, source, title, and a snippet.

---

## Findings management

The KB accumulates findings the agent surfaces during `--vuln` sessions. You can curate these to improve future runs.

```bash
# Browse
basilisk knowledge list-findings
basilisk knowledge show-finding <id>

# Correct a wrong finding (preserves the original with an amendment pointer)
basilisk knowledge correct <id> --reason "unreachable on mainnet — guard catches it"

# Dismiss false positives
basilisk knowledge dismiss <id> --reason "invariant maintained by the caller"

# Validate a real finding
basilisk knowledge confirm <id>
```

Corrections are stored as sibling rows with `is_correction = true` and a pointer back to the original. On retrieval, both the original and the correction surface together — so the next run sees the human verdict without any extra training loop.

---

## Corpus stats

```bash
basilisk knowledge stats
# Prints: collection name, entry count, embedding dim, provider, last-ingested timestamp
```

---

## Ingest state and idempotency

The KB tracks progress in `~/.basilisk/knowledge/ingest_state.json`. Re-running any ingest command is safe — previously indexed entries are skipped unless the corpus version has changed.

---

## Embedding providers

| Variable | Provider | Model | Dims | Notes |
|---|---|---|---|---|
| `VOYAGE_API_KEY` | Voyage | `voyage-code-3` | 1024 | Primary; best for code + security text |
| `OPENAI_API_KEY` | OpenAI | `text-embedding-3-small` | 1536 | Fallback |
| `OLLAMA_HOST` | Ollama | configurable | varies | Fully local / offline |

Set `EMBEDDINGS_PROVIDER=voyage|openai|ollama` to pin a provider. Changing providers after ingest invalidates existing vectors — all collections must be re-embedded (different vector spaces are incompatible). A `basilisk knowledge reembed <collection>` command is planned.

---

## Storage layout

| Path | Contents |
|---|---|
| `~/.basilisk/knowledge/store.json` | Vector store (all collections) |
| `~/.basilisk/knowledge/ingest_state.json` | Ingest progress per source |
| `~/.basilisk/knowledge/solodit_dump.jsonl` | Solodit input corpus (operator-provided) |
| `~/.basilisk/knowledge/rekt_dump.jsonl` | Rekt post-mortems (operator-provided) |
| `~/.basilisk/knowledge/tob_dump.jsonl` | Trail of Bits writeups (operator-provided) |
