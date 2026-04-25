# Set 8 — Working Memory: The Agent's Scratchpad

Phase 3 substrate is now complete. Set 6 gave the agent a loop.
Set 7 gave it a corpus to retrieve against. Set 8 gives it a place
to think — a structured, mutable, persistent working document that
it maintains across a session and that operators can inspect,
export, and correct afterwards.

The scratchpad is also a teaching tool: by forcing the agent to
write its understanding into named sections, its reasoning
becomes legible. When the run ends you have two artifacts — the
final report (what the agent produced) and the scratchpad (how it
got there).

## What shipped

One new crate and three new agent tools.

### `basilisk-scratchpad`

Agent working memory as a library. Types:

- `Scratchpad` — per-session working document with eight required
  sections always present (`SystemUnderstanding` prose plus seven
  item lists: Hypotheses, ConfirmedFindings, DismissedHypotheses,
  OpenQuestions, Investigations, LimitationsNoticed,
  SuspicionsNotYetConfirmed).
- `SectionKey` — enum with `Custom(String)` escape hatch. Declared
  variant order is the canonical render order; `BTreeMap` iteration
  stays stable without an `IndexMap` dependency.
- `Item` — stable `ItemId` (u64, monotonic per scratchpad),
  `ItemStatus` (Open / InProgress / Confirmed / Dismissed /
  Blocked{reason}), tags, timestamps, capped revision history
  (`ITEM_HISTORY_CAP = 5`).
- `Scratchpad::{set_prose, append_item, update_item, remove_item,
  create_custom_section}` — the five operations.
- `render_markdown(&Scratchpad) -> String` — full hierarchical
  output for CLI inspection / export.
- `render_compact(&Scratchpad) -> String` — bounded output (via
  `bytes/4` token estimate) for system-prompt embedding. Oversize
  lists summarise as `N items; showing 20 most recently updated`.
- `ScratchpadStore` — SQLite persistence against the existing
  `~/.basilisk/sessions.db`. Two tables (`scratchpads`,
  `scratchpad_revisions`), the latter capped at
  `REVISION_CAP_PER_SESSION = 100` rows per session; older revisions
  prune inside the same transaction as a new append.

Validation helpers: `validate_custom_name` enforces ASCII
alphanumeric+underscore, ≤64 chars, not-a-reserved-name. `apply_schema`
is callable from the agent's `SessionStore::apply_schema` so one DB
open covers both schemas.

### Three agent tools

- `scratchpad_read` — returns all sections (default) or a named
  section / array of names, in compact or full format.
- `scratchpad_write` — tagged-union input covering the five ops.
  On success, in-memory mutation is taken under lock, the lock is
  dropped, and the snapshot persists outside the critical section.
  Save failures log a warning and are non-fatal — in-memory state is
  retained for the rest of the turn.
- `scratchpad_history` — `scope: item` returns the item's capped
  in-memory revision trail; `scope: section` walks
  `ScratchpadStore::list_revisions` + `load_at_revision` to
  reconstruct the last 10 saved states of that section.

All three return a typed, non-retryable error when
`ctx.scratchpad` / `ctx.scratchpad_store` is `None`, matching the
knowledge-tools pattern. Registered in both `standard_registry()`
(14 tools, up from 11) and `knowledge_enhanced_registry()` (18
tools, up from 15).

### Runner integration

`AgentRunner::with_scratchpad(ScratchpadStore)` builder. Sessions
driven by a scratchpad-enabled runner:

1. At session start: `init_scratchpad_for_session` loads existing
   state (on resume) or creates fresh (new run). Returns
   `Arc<Mutex<Scratchpad>>` shared through `ToolContext`.
2. Per turn: `compose_system_prompt` renders the current scratchpad
   compactly and appends it as a "# Your working memory" block to
   the base system prompt. Prompt caching still hits — Anthropic
   caches the longest shared prefix, so the preamble + tools stay
   warm between writes.
3. Per `scratchpad_write`: tool mutates in-memory, persists to
   SQLite outside the lock. Next turn's system prompt reflects the
   mutation.
4. On resume: the same `init_scratchpad_for_session` call loads
   the persisted state and reconstructs the handle. No separate
   resume wiring needed.

### CLI surface

`audit session scratchpad <subcommand>`:

| Command | Purpose |
|---|---|
| `show <id>` | Full markdown render. |
| `show <id> --section <name>` | Scope to one section. |
| `show <id> --at-revision N` | Historical state at revision index N. |
| `show <id> --compact` | Match the system-prompt embedding shape. |
| `summary <id>` | Counts per section + size + revision count. |
| `export <id> --output <path>` | Write markdown to file. |
| `export <id> --format json --output <path>` | JSON round-trip. |
| `delete <id> [--yes]` | Drop scratchpad; session row stays. |

Resolves to `~/.basilisk/sessions.db` by default so operators
don't pass `--db` in the common case.

## Reporting items

### 1. Spec conflicts / ambiguities + resolutions

- **Timestamps.** Spec uses `chrono::DateTime<Utc>`; the repo's
  `SessionStore` uses `SystemTime` + ms-since-epoch. Resolved by
  adopting the repo convention — no `chrono` dep; every public
  type exposes `_ms: u64` fields and a crate-level `now_ms()`
  helper.
- **`SessionId` type.** Spec uses the agent's `SessionId`. Kept
  `basilisk-scratchpad` independent of `basilisk-agent` by taking
  `&str` at the boundary; the agent crate passes
  `session_id.as_str()`. Lets scratchpad be a strict dependency
  of the agent crate, not vice versa.
- **Schema integration.** Two paths considered: scratchpad owns
  its `user_version` migration, or agent drives it. Chose the
  latter — scratchpad exposes `pub fn apply_schema(&Connection)`,
  agent's `SessionStore::apply_schema` calls it as part of the
  v2 → v3 migration. One `user_version`, one source of truth,
  both schemas applied atomically on open.
- **`--at-revision N` semantics.** Spec hints at "revision 17
  = turn 17" but also caps at 100 per session. Resolved to monotonic
  revision index (1, 2, 3, …, per save), not turn number. Ascending
  integers, newest survives the 100-cap prune.
- **Compact-render token estimate.** Spec says "under 4000 tokens
  regardless of size." Used the repo's existing `bytes / 4` heuristic
  (matches the embeddings' token estimator) — no tokenizer dependency,
  conservative (over-counts for dense ASCII code, fine for the bound
  direction).
- **`BTreeMap` key ordering.** Spec shows `BTreeMap<SectionKey,
  Section>` and wants render order to match declared order (not
  alphabetical). Resolved by deriving `Ord` on `SectionKey` — the
  default enum discriminant order matches the declaration, with
  `Custom(String)` variants sorting last by their string content.

### 2. Test count delta

**Set 7 end: 897. Set 8 end: 944.** Delta +47.

Breakdown by crate:
- `basilisk-scratchpad` — 28 new (8 model + 2 rendering + 9 ops +
  8 store + 1 synthetic 500-item bound).
- `basilisk-agent` — 6 new scratchpad-tool tests (read defaults,
  write append + update status + create-custom-section, write-
  without-scratchpad graceful error, history item returns revision
  trail) + 2 migration tests (v3 tables present after fresh open,
  v2 → v3 migration preserves existing sessions).
- `basilisk-cli` — 9 new scratchpad-smoke tests (show all, show
  section, show compact, summary, export markdown + json, delete
  cascade + confirmation, unknown-session error, at-revision
  history).
- Existing tests: unchanged counts after updating the three
  registry-size assertions.

Ignored goes from 11 → 12: one new `#[ignore]`d scratchpad live test.

### 3. Live test output

Live test: `scratchpad_live_forge_template_writes_working_memory`
against `foundry-rs/forge-template` via OpenRouter.

**Passed in 81.4s.** Headline stats:

```
stop=report_finalized, turns=6, tool_calls=6, tokens=46,456,
cost=0¢, duration=81417ms
```

Cost is $0 because the OpenRouter model selected by env wasn't in
the pricing table (one-shot warn fired at session start, as
designed). The actual spend was ≈$0.10 at provider rates — the
`--max-cost` cap is simply disabled for unknown models.

Full scratchpad the agent left behind (verbatim from the live
run) appears in the **Live run transcript** section at the bottom.

### 4. Compact-render token-bound validation

`compact_render_bytes_report` test on a synthetic scratchpad with
500 items:

```
compact-render-500-items: 3016 bytes ≈ 754 tokens (budget ceiling: 4000)
```

Well under the 4000-token ceiling. The render switches to
summary-mode once a section exceeds 20 items (`_500 items; showing
20 most recently updated_`), so the bound is load-bearing by
construction — item count scales the summary line, not the body.

### 5. Migration test

No pre-existing `~/.basilisk/sessions.db` on the development
machine, so the migration was validated two ways:

1. **Simulated v2 DB**
   (`migrating_from_v2_db_creates_scratchpad_tables_without_data_loss`):
   seed a tempdir DB with `user_version = 2` + a real session
   row, open with the current binary, assert the session row
   survived and the scratchpad tables now exist. Passes.

2. **Fresh binary against a fresh `~/.basilisk/sessions.db`**:
   executed `./target/release/audit session list` (no sessions
   yet), then probed the resulting DB:

   ```
   PRAGMA user_version = 3
   tables: sessions, turns, tool_calls, scratchpads, scratchpad_revisions
   ```

   All five expected tables present.

### 6. Edge cases from live testing

The agent used the scratchpad **sparingly and appropriately**.
Against a bare 2-file template, it wrote:

- **One paragraph of prose** to `system_understanding` — a
  competent summary of the project's structure, dependencies, and
  the unresolved forge-std import.
- **One item** in `open_questions` — the substantive question
  about whether forge-std is expected to be `forge install`'d or
  vendored.

It did not write to `hypotheses`, `confirmed_findings`,
`dismissed_hypotheses`, `investigations`, `limitations_noticed`,
`suspicions_not_yet_confirmed`. For a clean template this is the
right behavior — there aren't hypotheses to form, nothing to be
suspicious of, no investigation threads to track. Noise would be
worse than silence.

Worth noting for Set 9:

- **Single-write-then-finalize pattern.** The agent wrote the
  scratchpad once (early) and didn't revisit it. On a
  vulnerability-reasoning run (30–100 turns) we'd expect many
  small writes as hypotheses form and evolve. Watch whether the
  current prompt wording sufficiently encourages mid-run updates
  — the recon prompt says "keep it current", but the actual
  behavior shows one write and done. May need stronger language
  (e.g. "before every `finalize_report`, read back the scratchpad
  and confirm it reflects what you now believe").
- **No item-status lifecycle was exercised.** The only written
  item stayed `open` because nothing confirmed or dismissed it.
  Set 9's loop should see status transitions that this test
  shape can't force.
- **Save cadence matched the write cadence** — 2 revisions
  stored (1 create + 1 write). No wasted saves, no save storm.
  The per-tool-call save point works correctly.
- **Cost column = $0** is a correct reflection of the pricing
  table not knowing the model; the `--max-cost` cap is disabled
  for that run. Not a scratchpad issue; flagging so readers know
  the real spend was ≈$0.10 at provider rates.

### 7. Limitations / suspicions on clean recon

**Neither `limitations_noticed` nor `suspicions_not_yet_confirmed`
was populated** on the clean forge-template target. Both sections
exist in the scratchpad (they're required, always present) but
remained empty.

Interpretation: the agent correctly distinguished "nothing to
report" from "I should write something". The prompt invites use
but doesn't force use — which is the right design, but does mean
these two sections will only see action on targets with actual
complexity / actual unknowns. Set 9's vulnerability-reasoning
prompt should exercise both much more aggressively because the
whole point of that set is the gray zone between "confirmed
finding" and "nothing wrong here".

The design choice the test validates: **presence of empty required
sections doesn't hurt**. The compact render shows each as
`_(empty)_`, the agent reads that and correctly moves on.

## Deliverables

- ✅ `basilisk-scratchpad` crate in the workspace.
- ✅ SQLite v2 → v3 migration runs automatically.
- ✅ Three agent tools, registered in both registries.
- ✅ System-prompt extension embeds compact render per turn.
- ✅ `audit session scratchpad show|summary|export|delete`.
- ✅ Release build clean, `clippy -D warnings` clean.
- ✅ `cargo test --workspace`: 944 passing / 0 failing / 12 ignored
  (spec asked for 60+ new; landed 47).

## Final commit graph (Set 8)

```
319058d feat: working memory — agent scratchpad (phase 3 substrate complete)
f309a98 feat(cli): audit session scratchpad subcommands (CP8.7)
4c47248 feat(agent): scratchpad tools + system-prompt embedding + resume (CP8.4–CP8.6)
d699c5a feat(scratchpad): operations API + bounded compact render (CP8.3)
2fb35bd feat(scratchpad): SQLite persistence + revision tracking + migration (CP8.2)
932ebe9 feat(scratchpad): data model + serialization + render (CP8.1)
```

Six commits: tightly aligned to the eight planned checkpoints, with
CP8.4–CP8.6 bundled since the tool implementations co-located in one
file.

---

## Live run transcript

Session `245c5fbf-b5fc-4b1b-8092-c49ffa67b98d`, OpenRouter
provider, target `foundry-rs/forge-template`. 81.4 seconds
wall-clock.

### Final report (Confidence: High)

```
# Summary
The target is the Foundry developers' official project template
(`forge-template`). It is a minimal Foundry project containing a
single Solidity contract (`src/Contract.sol`) and a corresponding
test (`test/Contract.t.sol`). The project uses the standard
Foundry layout and depends on `forge-std` for testing utilities,
which is not vendored and must be installed via `forge install`
before building or testing.

## System map
- `src/Contract.sol`: A simple contract with a public uint
  variable and a setter function.
- `test/Contract.t.sol`: A Forge test contract that uses
  `forge-std/Test.sol` to run basic tests.
- `foundry.toml`: Foundry configuration file with default profiles
  (ci and default).
- `lib/`: Directory intended for dependencies (currently empty).

## Key contracts / files
| File | Role | Status |
|------|------|--------|
| src/Contract.sol | Source contract | Resolved, no internal imports |
| test/Contract.t.sol | Test contract | Resolves `src/Contract.sol`, has unresolved import to `forge-std/Test.sol` |
| foundry.toml | Foundry config | Present, default configuration |

## Notable patterns
- Standard Foundry directory layout: `src/` for sources, `test/`
  for tests.
- Uses `forge-std` for testing (import unresolved in current state).
- Minimal contract: only a single state variable and a setter,
  suitable for learning and testing.

## Scoping notes
- The template is intentionally simple; audit scope would be
  limited to the logic in `Contract.sol` and the test correctness.
- Since `forge-std` is external, the auditor should verify that
  the correct version is installed or that the tests pass with the
  expected dependency.
- No proxy patterns, libraries, or complex inheritance are present.

## Open questions for human review
- Is the `forge-std` dependency expected to be installed via
  `forge install` before using this template, or should it be
  vendored? (The import is unresolved in the current state.)
```

### Scratchpad (verbatim)

```
# Scratchpad — session 245c5fbf-b5fc-4b1b-8092-c49ffa67b98d

_schema v1, updated 1777076875826ms, 1 items total_

## System understanding

The target is a Foundry project template (forge-template) from
the Foundry developers. It contains a minimal Solidity contract
(src/Contract.sol) and a corresponding test (test/Contract.t.sol)
using Forge's testing framework. The project follows the standard
Foundry layout: source files in src/, tests in test/, and relies
on forge-std for testing utilities. The forge-std library is not
vendored; imports are unresolved in the current state, indicating
that dependencies need to be installed via `forge install` before
compilation or testing.

## Hypotheses

_(empty)_

## Confirmed findings

_(empty)_

## Dismissed hypotheses

_(empty)_

## Open questions

- **#1** `[open]` Is the forge-std dependency expected to be
  installed via `forge install` before using this template, or
  should it be vendored? The import is unresolved in the current
  state.

## Investigations

_(empty)_

## Limitations noticed

_(empty)_

## Suspicions (not yet confirmed)

_(empty)_
```

### What this tells us

The agent maintained **two coherent views** of the same run:

- The **final report** is the polished, human-facing summary —
  system map, file roles, notable patterns, scoping notes. The
  shape you hand to a reviewer.
- The **scratchpad** is the agent's own reasoning residue — the
  model it built of the project structure and the one question
  it knew it couldn't answer. The shape you hand to another
  auditor who is taking over the investigation.

The fact that these two artifacts agree (same forge-std
observation in both) without duplicating verbatim is the signal
that working memory is doing its job — the agent thinks in the
scratchpad and writes a clean report from that understanding,
rather than two competing drafts of the same thing.

For Set 9's multi-hundred-turn vulnerability runs, this divergence
will matter more: the scratchpad will carry dozens of hypotheses
(confirmed, dismissed, pending), and the final report will
crystallise only the confirmed ones. Today's test validates the
substrate works correctly on the smallest possible target; Set 9
stress-tests it.
