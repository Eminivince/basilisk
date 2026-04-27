# Roadmap

Work that has been deliberately deferred from the main build sequence, with
rationale. Items here are not abandoned — they are scheduled for the right
moment in the project's evolution. Moving an item out of this file into an
instruction set is the mechanism for un-deferring.

## Active deferrals

Each section below: what it is, current state, what unlocking it looks like,
why it's deferred.

### Hardcoded recon dispatch replacement

`basilisk recon <target>` without `--agent` still uses the deterministic Rust
pipeline shipped in Sets 1–5: `detect` → `OnchainIngester::resolve_system` /
`analyze_project` / clone-and-analyze. With `--agent` the same targets flow
through the LLM-driven loop. Unlock: delete the deterministic dispatch, make
`--agent` the only path (or invert it — `--no-agent` for a fast local
fallback). Deferred until the agent has enough production mileage that we
trust it as the default; the three live tests are a necessary but not
sufficient signal. The tension is that the deterministic path is both the
old shape and our regression fixture — we rip it out only once the agent is
producing briefs we'd publish unattended.

### Context compaction for long agent runs

Every turn of the loop re-sends the full prior transcript plus every tool
result. For Aave V3 this pushed the session to 1M tokens across 8 turns —
quadratic growth dominated by accumulated `resolve_onchain_system` output.
Unlock: an explicit `compact_context` tool the agent calls when it judges
the context heavy, plus automatic older-turn summarization after N turns.
The `AgentObserver::on_nudge_fired` telemetry shipped in set-6.5 is the
scaffolding — we can see when the loop is stressed before layering in
mitigation. Deferred to Set 7 when vulnerability reasoning will make long
runs routine.

### Parallelism in `resolve_system`

`ExpansionLimits::parallelism` is accepted as a flag but unwired — BFS
expansion is sequential. Unlock: fan out contract resolutions via
`tokio::join!` gated by a semaphore sized to `parallelism`, respecting the
shared explorer rate limiter so we don't trip per-key quotas. Deferred
because correctness came first; sequential is slower but predictable, and
the agent's budget caps are the user-visible bound anyway. Revisit when
Aave-scale resolutions feel too slow in practice and the agent starts
timing out on `resolve_onchain_system` calls.

### Recursive expansion into external dependencies in `analyze_project`

`Dependency` records are captured (OpenZeppelin, forge-std, Uniswap, etc.)
but the dep repos aren't cloned and their Solidity files aren't merged
into the project graph. Unlock: per-dep clone into the content-addressed
`RepoCache`, then graph their sources under a namespaced path so imports
resolve transitively. Deferred because most deps are well-known — the
agent can reason about them via training data, and Set 8's RAG layer will
close the remaining gap. Recursive cloning adds I/O without proportional
audit value until we find targets where it actually matters.

### Multi-chain expansion

Currently ~15 well-known chains via a `Chain` enum with an `Other {
chain_id, name }` escape hatch for anything else. Unlock: bundle
chainlist.org data as a build-time resource, wire Routescan as a
fallback explorer, auto-instantiate the right explorer client from the
registry, build a per-chain EVM-caveats database (Arbitrum-specific
opcodes, BNB's gas quirks, etc.), and probe capabilities with graceful
degradation. Deferred to after Phase 3 validation: the agent is the
differentiator; shipping it on 15 chains with real tool use beats
shipping 500 chains with no agent.

### Storage-layout recovery via `foundry-compilers`

The types and JSON parsers for storage layouts are in place, every call
site returns `Ok(None)` with a logged skip note. Unlock: wire
`foundry-compilers` with managed solc via svm-rs — downloads solc on
first use, caches under `~/.basilisk/solc/`, picks the right version
from the project's `pragma`. Deferred because it adds a substantial
transitive dependency tree and requires network access on first run;
not load-bearing for the agent until Set 9 (PoC synthesis) where we
need typed storage access to write exploit tests.

### Constructor-argument ABI decoding via `alloy-dyn-abi`

`ResolvedContract.constructor_args` ships the raw bytes; `decoded: None`
always. Unlock: pull `alloy-dyn-abi`, walk the constructor signature
from the verified ABI, decode bytes to typed values. Deferred because
the raw bytes are the authoritative artifact — decoded values are
enrichment the agent can do itself via a dedicated tool when it needs
them, and hard-wiring it into every resolve call would burn CPU on
data most audits don't read.

### LanceDB-backed vector store

`crates/vector/` currently ships `FileVectorStore` — a JSON snapshot
written on every mutation. The `VectorStore` trait is identical to
what a LanceDB implementation would expose, so the swap is transparent
to callers. Unlock: a `LanceDbStore` behind a `lancedb` feature flag,
migrating `~/.basilisk/knowledge/` from `store.json` to Arrow/Parquet
collection directories. Deferred after a CP7.3 validation spike
measured ~11-min cold compile and ~12GB `target/` dir with the
`lancedb` dependency pulled in — a bad trade at the current corpus
scale (thousands of records search in milliseconds from JSON). Revisit
when a single operator's corpus crosses ~50k records or when we need
concurrent cross-process reads, whichever comes first.

### Additional corpus ingesters

Set 7 shipped three external ingesters: Solodit, SWC registry,
OpenZeppelin advisories. Four more — Code4rena, Sherlock, rekt.news,
Trail of Bits — were scoped out of Set 7 to keep the set at 10
checkpoints. Unlock: one `Ingester` trait impl per source, additive
against the existing crate. Deferred because the three shipped
ingesters already seed ~2500 records and let Set 8/9 evaluate
retrieval quality on real data; broadening the corpus can wait until
the retrieval path itself is wired into reasoning.

### Explicit requests-per-minute gate

`TokenBudgetGate` paces embedding calls by tokens/min and in practice
this limits Voyage free-tier runs to ~1 request/min naturally (each
batch saturates the 10k token budget). Unlock: a sibling
`RequestBudgetGate` that enforces a hard RPM cap so bursty workloads
(many small batches) can't sneak past the implicit pacing. Deferred
because the token gate already covers the realistic failure mode;
this is belt-and-suspenders against a pathological case we haven't
seen. Add when we first see an RPM-class 429 in a real run.

### Reembed cost-warning UX

`basilisk knowledge reembed <collection>` is the documented migration
path when the operator swaps embedding providers (different
dimensionality, different model). Unlock: the command itself plus a
pre-flight estimator that prints record count × tokens × target-
provider price, requires explicit `--confirm yes`. Deferred because
dim migration hasn't happened yet in practice — the operator's only
provider swap so far has been Ollama → Voyage where we cleared the
collection manually. Ships alongside the first real migration event.

### Scratchpad revisit cadence in the system prompt

Set 8's live run showed the agent writes the scratchpad once
(early) and doesn't revisit it. Against a clean recon target this
is correct — nothing changes — but Set 9's vulnerability-reasoning
pass will run 30–100 turns where mid-run updates matter. Unlock:
a prompt-language change ("before every `finalize_report`, read
back your scratchpad and confirm it reflects what you now
believe") plus optionally a runner hook that forces a
`scratchpad_read` call before `finalize_report` accepts input.
Deferred to Set 9 because the right prompt wording depends on
how the reasoning loop is structured; no point tuning against
today's recon shape.

### Item-status lifecycle coverage

Set 8's live test exercised `append_item` but nothing set a
status transition (Open → InProgress → Confirmed / Dismissed).
Clean recon targets rarely have evidence that accumulates against
a hypothesis the way vulnerability reasoning does. Unlock:
natural coverage from Set 9's live tests, which will flip items
through statuses dozens of times per run. Deferred because
writing a synthetic test that exercises the transitions doesn't
validate the same thing — we want to see real reasoning drive
the lifecycle.

### `basilisk session resume` live test

The resume path is wired end-to-end and covered by unit tests
(`SessionStore::mark_resumed`, `AgentRunner::resume_with_observer`),
but no `#[ignore]`-gated live test exercises it against a real API.
Unlock: a fresh live test that starts an agent, interrupts after N
turns, reconstructs the runner, resumes, asserts the final report
arrives. Deferred because it costs money for marginal signal — the
three existing live tests already prove the full-session path, and
resume's delta is only the history-rehydration code which unit tests
cover. Add opportunistically when we have a reproducible
interruption scenario worth replaying.