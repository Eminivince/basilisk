# Set 7 — Persistent Knowledge Base and Retrieval

Phase 3 memory substrate: RAG infrastructure that turns Basilisk from a
smart agent calling tools into an auditor that sharpens with every
engagement. Knowledge lives on the operator's machine, inspectable and
correctable; provider and model swaps are first-class.

## What shipped

### Four new crates

| Crate | Purpose |
|---|---|
| `basilisk-embeddings` | `EmbeddingProvider` trait + Voyage / OpenAI / Ollama / OpenRouter backends + batching/retry wrapper |
| `basilisk-vector` | `VectorStore` trait + 5 collection specs + in-memory + JSON-backed persistent implementation |
| `basilisk-ingest` | `Ingester` trait + normalization + file-position-cursor state + Solodit/SWC/OpenZeppelin/Protocol ingesters |
| `basilisk-knowledge` | `KnowledgeBase` public API — retrieval, findings memory, corrections-as-columns |

### Three external corpus ingesters

- **Solodit** — user-supplied JSONL dump. Accepts both the native
  `{id,title,body,...}` shape and OpenAI fine-tuning chat format
  (`{"messages":[{"role":"user",...},{"role":"assistant",...}]}`)
  auto-detected per line. Content-hash ids make re-ingests idempotent.
- **SWC registry** — shallow-clones `SmartContractSecurity/SWC-registry`,
  pins `master`, walks `entries/docs/`, emits one record per SWC entry.
- **OpenZeppelin advisories** — GitHub security advisories API,
  optional `GITHUB_TOKEN` for rate-limit headroom.

### Protocol-context ingestion

Four source types, one ingester, scoped by `engagement_id`:

- **URL** — `readability` extraction + overlapping token windows.
- **PDF** — `pdf-extract` with partial-extraction flags for image-heavy pages.
- **File** — markdown split by H1/H2/H3, plain text split by token window.
- **GitHub dir** — shallow-clone + subdir walk, each `.md` as a record.

### Agent tools (4 new)

- `search_knowledge_base` — natural-language query across collections.
- `search_similar_code` — embed a snippet, find similar.
- `search_protocol_docs` — query scoped to engagement.
- `record_finding` — agent writes to `user_findings`.

Exposed via `knowledge_enhanced_registry(kb)` (15 tools) alongside the
existing `standard_registry()` (11 tools). Set 9 wires retrieval into
the reasoning prompt.

### `audit knowledge` CLI surface

```
stats                          Collection sizes, dims, providers, ingest state
ingest <source> [--all]        Solodit / SWC / OpenZeppelin
add-protocol <eng-id> ...      Per-engagement docs (--url/--pdf/--file/--github)
list-findings                  Browse user_findings
show-finding <id>              Full record
correct <id> --reason ...      Human correction as sibling row
dismiss <id> --reason ...      Mark false positive
confirm <id>                   Mark human-verified
search <query>                 Natural-language retrieval
clear <coll> [--yes]           Destructive; confirms
```

### Key design decisions

- **Corrections as columns, not a separate collection.**
  `user_findings` carries `is_correction`, `corrects_id`,
  `correction_reason`, `user_verdict`. One retrieval path, one reembed
  path, same expressiveness.
- **Content-addressed dedup.** Record ids derive from
  `sha256(source | source_id | chunk_index)`. Re-ingesting unchanged
  content produces the same ids and upserts as a no-op.
- **Interim persistence via JSON.** `FileVectorStore` writes the whole
  store on every mutation (tempfile-rename). Good for
  hundreds-to-thousands of records; LanceDB swap deferred after a
  600MB-target-dir / 11-min-compile validation spike.
- **Provider-agnostic dimensions.** Schema-versioned collection specs
  refuse writes when the configured embedding dim doesn't match; error
  points at `reembed`.

## Validation — end-to-end dogfooding

Live run against an operator machine with Voyage `voyage-code-3` (1024 dims).

### Current corpus

```
store: ~/.basilisk/knowledge/store.json

collection             records     dim  provider
advisories                  57    1024  voyage/voyage-code-3
public_findings           2460    1024  voyage/voyage-code-3

ingest state:
  openzeppelin         records=  20  cursor=GHSA-xrc4-737v-9q75
  solodit              records=2563  cursor=2460 (file-position)
  swc                  records=  37  cursor=<commit-sha>
```

**2517 records embedded, ~1.82M tokens, ~$0.04 at Voyage paid tier.**

### Ingest wall-clock

| Source | Records | Tokens | Duration |
|---|---:|---:|---:|
| SWC | 37 | 55,193 | 6m 29s (free-tier 10k tok/min gate-paced) |
| Solodit | 2460 | 1,759,004 | 19m 15s (paid-tier burst) |
| OpenZeppelin | 20 | ~6,200 | ~15s |

### Test count

**897 passing, 0 failing, 11 ignored** — up from 695 at the start of
Set 7. 202 new tests across the four crates + CLI integration tests.

### Final gate

- `cargo fmt --all` clean
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo test --workspace` green
- `cargo build --release` clean

## Bugs surfaced during dogfooding (and fixed)

Nothing teaches a system like an actual user hitting it. Seven
real-world bugs appeared after the feature was "done" and had to be
fixed on the path to a working ingest.

1. **SWC default-branch lookup failed without a GithubClient.**
   `RepoCache::fetch(ref=None, github=None)` refused to guess. Fixed
   by pinning `refs/heads/master` (SWC is frozen) with `main` fallback.

2. **SWC then failed with auth errors on unrelated GithubClient calls.**
   Wiring a GithubClient introduced a new failure mode when the
   operator's `GITHUB_TOKEN` was stale. Fixed by making SWC never
   use the client — domain knowledge (frozen repo) beats generality.

3. **SWC repo was restructured:** files now at `entries/docs/SWC-NNN.md`,
   not `entries/`. Added a fallback path.

4. **Solodit JSONL format mismatch.** Operators ship dumps in OpenAI
   fine-tuning chat format, not Solodit's native shape. Added
   `parse_any_row` with auto-detect: title from user turn (with
   "Analyze the following vulnerability report:" prefix stripped),
   body from assistant turn, severity from bracket-tag `[H-01]`.

5. **Ingest looked frozen for minutes.** Progress callback only fired
   after the first embedding batch completed. Added an early tick that
   fires the moment scan finishes so `scanned=N upserted=0` shows
   immediately — before any cold-model wait.

6. **Voyage free-tier rate limit stalled forever.** Three independent
   issues compounded:
   - `TokenBudgetGate` looped forever when a single batch exceeded
     the whole window's budget (sleep + retry cycle with no progress).
   - `BatchingProvider` didn't split batches by estimated tokens, so
     every call overshot the 10k cap.
   - Retry backoff was 500ms–4s — far too short for a minute-scoped
     rate-limit window.
   Fixed all three; added `VOYAGE_TOKEN_RATE_PER_MINUTE` env var
   for paid-tier operators (0 disables the gate).

7. **Silent data loss on incremental runs.** Solodit's cursor was the
   lex-largest record id. Fine for chronological ids, catastrophic
   for content-hash ids (where cursor becomes a random cutoff).
   Switched to **file-position cursor** (1-based line count). Legacy
   id-cursors don't parse as usize → full re-ingest, with upsert
   idempotency handling the overlap.

8. **Every `ingest swc` re-cloned the repo even when cached.** Cache
   lookup required knowing the SHA; SHA required a clone. Added a
   `git ls-remote`-style pre-check (via `git2::Remote::list`) that
   resolves Branch/Tag → SHA in one HTTP round-trip. Cached SHA = no
   clone at all.

Each fix is a separate commit so the blast radius of any individual
change is small.

## Deferred work

- **LanceDB-backed store.** Validated as a blocker on CP7.3: cold
  compile ~11 min, target/ dir ~12 GB. Shelved for a follow-up set
  behind a feature flag. `VectorStore` trait is identical — swap is
  transparent to callers.
- **4 additional ingesters.** Code4rena, Sherlock, rekt.news, Trail
  of Bits move to Set 7.5. Same `Ingester` trait; additive.
- **Agent-side RAG in `audit recon`.** Set 9 wires the enhanced
  registry into a dedicated reasoning prompt. Recon itself stays
  enumeration-only.
- **Explicit RPM gate.** Today we only gate on tokens/min; Voyage's
  3-RPM free-tier limit is enforced indirectly because token-sized
  batches pace requests to ~1/min naturally. Edge case: many small
  batches could burst. Low priority.
- **Reembed cost-warning UX.** `audit knowledge reembed <collection>`
  with confirm-yes lands when dim migration is actually needed.

## Final commit graph (Set 7)

```
1e129c1 fix(ingest): Solodit cursor uses file position, not content-hash id
1d4ef5a fix: unblock `audit knowledge ingest` against Voyage free tier
b9c1298 feat(embeddings): OpenRouter support + scan-done early tick
8041a09 feat(cli): live progress line during `audit knowledge ingest`
aa374f9 fix(ingest): SWC new layout + Solodit openai-chat format
fe3ec65 fix(ingest): SWC always pins a ref, never needs GithubClient
3890510 fix(ingest): default-branch lookup for SWC + protocol GithubDir
4d87636 feat: persistent knowledge base and retrieval (phase 3 memory)
52f8b50 feat(agent): knowledge retrieval tools + knowledge-enhanced registry
a74192f feat(knowledge): public API + findings memory with corrections as columns
844f7ff feat(ingest): protocol-context ingestion (url / pdf / file / github)
6ce0289 feat(ingest): swc + openzeppelin advisories ingesters
4419bb3 feat(ingest): solodit pipeline
68b1539 feat(ingest): Ingester trait + normalization + state + CLI skeleton
4ba23ec feat(vector): VectorStore trait + in-memory impl + collection specs
d8b34e1 feat(embeddings): openai + ollama backends + batching wrapper
254f434 feat(embeddings): trait + voyage backend
```

17 commits: 10 planned checkpoints + 7 dogfooding fixes.

## What this unlocks

- **Set 8** can build evaluation harnesses against a real 2500-record
  corpus instead of mocks.
- **Set 9** can wire retrieval into a vulnerability-reasoning prompt
  with a known-good data layer underneath.
- **Every future audit** leaves behind findings + corrections that
  compound into the operator's personal knowledge base. The agent
  doesn't get smarter globally — it gets smarter *for this operator*,
  on this operator's machine, with this operator's judgment.
