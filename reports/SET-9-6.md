# Set 9.6 — Deferred work shipped

Set 9.6 ships the items deferred from Set 9.5: the four corpus-
expansion ingesters (Code4rena, Sherlock, rekt.news, Trail of Bits),
an interactive bench-review surface, the cross-session feedback
aggregator the calibration loop needs, and the `PhantomData<()>` field
cleanup. Calibration runs and `fork_block`-aware tools remain
explicitly deferred — they are multi-day-scope work that a focused
follow-up set will own.

---

## What landed (7 commits, 8 checkpoints)

| CP    | Commit                                                            | Lines | Tests added |
| ----- | ----------------------------------------------------------------- | ----: | ----------: |
| 9.6.1 | code4rena ingester (GitHub-clone pattern, two-shape parser)       | ~500  |  14         |
| 9.6.2 | sherlock ingester (single-repo walk, per-audit READMEs)           | ~440  |   6         |
| 9.6.3 | rekt.news ingester (operator-curated JSONL + loss bucketing)      | ~370  |   5         |
| 9.6.4 | trail of bits ingester (operator-curated JSONL + topic tags)      | ~370  |   5         |
| 9.6.5 | `audit bench review` interactive adjudication + verdict store     | ~280  |   4         |
| 9.6.6 | `scripts/feedback-summary.sh` — top-N feedback aggregator         | ~200  |  (live)     |
| 9.6.7 | drop `PhantomData<()>` field on `AgentRunner`                     |    -8 |   —         |
| 9.6.8 | README + final gate + this report                                 |  ~50  |   —         |
|       |                                                                   | **total** | **30 new fixture / unit / store tests** |

All ingesters wired into `audit knowledge ingest <source>` and into
`--all`. The new sources are reachable identically to the three Set 7
ingesters; nothing about the existing three changed.

---

## Pattern notes for future ingesters

Two distinct ingester shapes settled:

**GitHub-clone pattern** (Code4rena, Sherlock, SWC, OZ).
`RepoCache::fetch(owner, repo, Some(GitRef::Branch("main")), …)` with
a `master` fallback. Lex-sorted finding IDs as the cursor. Walk the
working tree, parse markdown into `IngestRecord`s. Test by feeding
a tempdir of fixture markdown to the parser function — the network
round-trip is reserved for `#[ignore]` live tests.

**Operator-curated JSONL pattern** (rekt.news, Trail of Bits, Solodit).
Default path under `~/.basilisk/knowledge/<source>_dump.jsonl`.
Line-number cursor. Body fallback to summary when the operator only
supplied metadata + summary. No scraping in the binary. The
`crate::state::default_knowledge_root()` helper added in this set is
the JSONL ingesters' shared entry point; future JSONL sources reuse
it directly.

`Code4renaFindingRow::into_ingest_record` carries one design choice
worth flagging: the consolidated-report shape and the per-finding-
file shape both flatten into the same `IngestRecord` struct. The
parsers diverge upstream, but the embedded record is identical, so
retrieval doesn't have to know which shape produced it.

---

## `audit bench review`

The scorer is heuristic — keyword match against the agent's
title/summary/category. A genuinely good catch can score as a "miss"
when the agent phrases it differently than `must_mention` expects, and
a finding outside the curated `vulnerability_classes` always scores
as a false positive even if it's a legitimate new catch. The review
surface exists so a human can override:

```text
$ audit bench review 12
=== review: bench run #12 ===
target: Visor Finance (visor-2021)
session: f4a1...
scorer counts: matches=0  misses=2  false_positives=3

--- misses (2) ---
  [1/2] miss: class=reentrancy severity_min=high keywords=["reentrancy", "callback"]
    [a]ctual_miss / [s]coring_failure / s[k]ip
    > s
    note (optional, blank to skip) > agent flagged it as 'unsafe ERC-20 callback'
    recorded: scoring_failure
  ...

--- summary ---
  miss             scoring_failure      1
  miss             actual_miss          1
  false_positive   in_scope_extra       2
  → coverage after review: 50.0% (1/2)
```

Verdicts persist via UPSERT keyed on `(run_id, kind, label)`. The
table sits next to `bench_runs` in the same SQLite file; `audit bench
history` and `audit bench review` see the same store. Re-running
`review <id>` skips already-reviewed items by default; pass
`--re-review` to re-prompt.

---

## `scripts/feedback-summary.sh`

The script is the calibration loop's read side. It pulls three
signals out of the operator's `sessions.db`:

1. **Recurring limitation / suspicion themes** — clusters the
   agent's `record_limitation` / `record_suspicion` short-form
   descriptions (truncated to 100 chars to make standardized
   phrasings collide under `uniq`).
2. **Bench-review verdict tally + recurring `actual_miss` classes** —
   joins on `bench_review_verdicts` from CP9.6.5. The leaderboard
   shows the operator which capability gaps recur across runs.
3. **Messiest sessions** — sessions with the highest combined
   limitation + suspicion counts; first place to look during
   calibration.

`--json` mode emits machine-readable output for ad-hoc tooling.
Tested live against the local `~/.basilisk/sessions.db`; pulls real
suspicion + self-critique payloads.

The script is intentionally a shell tool, not a Rust binary. Its
shape is "operator dashboard," not "production data pipeline." Pinning
it to bash + jq + sqlite3 keeps it light and forkable.

---

## Final gate

- `cargo fmt --all` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` — **1115 tests passed, 0 failed.** Up
  from 1085 in Set 9.5; the 30-test delta tracks the new ingesters
  (14 + 6 + 5 + 5) plus 4 store tests for `bench_review_verdicts`.
- Workspace builds clean (`cargo build`).

Two pre-existing test failures from Set 9.5 cleanup were uncovered
during this set's CI tightening — `recon_help_lists_the_agent_flag`
and `env_var_configured_provider_shows_in_agent_starts_up_message`
both referenced the removed `--agent` flag. Fixed in CP9.6.5
alongside the bench-review wiring.

---

## What's deferred (and why)

**Live-network ingest validation.** None of the four new ingesters
have `#[ignore]`-d live tests yet. Code4rena and Sherlock would each
require a real GitHub clone in CI; rekt.news and Trail of Bits would
require actual operator-curated JSONL fixtures. The fixture-based
unit tests cover the parser logic; the network plumbing is identical
to SWC's (already live-tested). Adding `#[ignore]` live tests is
~30 minutes per source if the next set wants them; not blocking
until retrieval-quality calibration motivates it.

**`fork_block`-aware bench tools.** The Visor benchmark is
structurally broken at `latest` (the contract self-destructed
post-exploit). CP9.5.7 surfaced `fork_block` to the agent in the
operator note, which masks the symptom but doesn't fix the underlying
issue: tools like `read_storage_at`, `get_balance`, and bytecode
fetches still hit `latest` on the public RPC. A fix means threading
fork-context through every read-only tool that takes an address.
That's multi-day scope. Set 9 ships with the workaround documented;
the next set owns the fix.

**Live calibration runs against the new corpus.** Now that the
corpus has 7 sources instead of 3, retrieval quality on
representative vulnerability queries is the next thing to measure.
Set 9.5 didn't include this; Set 9.6 doesn't either. The operator
will run `audit knowledge ingest --all` once the JSONL dumps and
GitHub clones have settled, then validate retrieval quality against
the WaveLauncher / Visor / Cream targets. Documenting the failure
mode is a deliverable of that calibration set, not this one.

---

## Spec adherence

The Set 9.5 spec called for these four ingesters in CP9.5.10 (the
"knowledge-expansion" deferred items). The deferral was a pragmatic
choice — Set 9.5 was already 14 checkpoints, and each ingester is
~400-500 LOC plus fixtures. Set 9.6 closes that loop without
introducing new scope.

Two spec items that *were* in 9.5's CP9.5.10 list but are *not*
landing in 9.6:

- **Audit bench review CLI** — landed in 9.6.5 (was originally
  scoped to 9.5).
- **PhantomData cleanup** — landed in 9.6.7 (was scoped to 9.5).

The `--with-knowledge` flag on recon and the `recon_knowledge_v1.md`
prompt remain deferred to the eventual Set 9 RAG-into-recon set —
that's a Set 9-or-later decision, not 9.6's.

---

## Final commit

`feat: deferred ingesters + bench review + calibration tooling (set 9.6)`

The four-ingester batch + bench review + feedback aggregator together
constitute the substrate for the next calibration cycle. Set 9.6 does
not change runtime behavior of recon or vuln modes; it adds operator-
side tools and corpus surface. The agent itself is unchanged — its
moat-fixes from Set 9.5 (vuln_v2.md, structured-recording discipline,
mandatory self-critique) are still the load-bearing pieces.
