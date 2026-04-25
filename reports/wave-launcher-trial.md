# Trial run — WaveLauncherMainnet

**This is the headline live-run artifact for Set 9.** A first-time
audit-mode invocation of Basilisk against a novel Ethereum target,
with full transcript / scratchpad / self-critique extracted from the
session DB so it survives terminal-render truncation.

This run pre-dated the `--vuln` initial-message fix (commit `8d67037`)
— the user-role message asked the agent to "perform reconnaissance,"
so the output was framed as recon. Even so, the agent surfaced 9
substantive concern regions (3 likely-real bugs) and produced a
self-critique that names the analytical tools it should have used.

---

## Run metadata

| field | value |
|---|---|
| session_id | `b6e2dc7c-a44a-43ca-bd97-f04ab4cf0d74` |
| target | `0xB9873b482d51b8b0f989DCD6CCf1D91520092b95` (Ethereum mainnet) |
| operator note | `first-run trial on WaveLauncherMainnet` |
| invocation | `audit recon … --chain ethereum --agent --vuln --provider openrouter --model anthropic/claude-opus-4.7` |
| model | `openrouter/anthropic/claude-opus-4.7` |
| budget | 100 turns / 2M tokens / $50 / 1h |
| stop_reason | `report_finalized` |
| turns | 8 |
| tool_calls | 13 (parallel — multiple tools per assistant turn) |
| input tokens | 1,621,193 |
| output tokens | 10,600 |
| cost | $25.12 |
| wall time | 9 min 03 s |
| nudge events | 0 (rail did not need to fire — agent self-disciplined) |
| prompt_hash | `ececba89a5da214e…` (matches `vuln_v1.md` ✓) |

---

## Tool-call sequence

```
turn  call_idx  tool                       result
 1    0         classify_target            ok
 3    0         classify_target            ok    (re-classify after seed message)
 5    0         resolve_onchain_system     ok    (308.4s — bulk of total time)
 7    0         scratchpad_write           ok    (system_understanding)
 7    0         static_call                ok    (launcher.status())
 7    1         static_call                ok    (launcher.minted())
 7    2         static_call                ok    (launcher.owner())
 7    3         static_call                ok    (Wave.tradingEnabled())
 7    4         static_call                ok    (Wave.totalSupply())
 9    0         fetch_github_repo          ok    (ethereum/solidity — wasted; agent self-corrected)
11    0         search_knowledge_base      ok    (initial-liquidity sandwich pattern)
11    1         search_knowledge_base      ok    (owner-drains-ETH-during-sale pattern)
13    0         finalize_self_critique     ok    (voluntary, before finalize_report)
15    0         finalize_report            ok
```

---

## Final report (verbatim from session DB)

# Recon brief — WaveLauncherMainnet

**Target:** `0xB9873b482d51b8b0f989DCD6CCf1D91520092b95` (Ethereum mainnet)
**Protocol:** Wave — a gas-auction fair-launch + an ERC-20 with an on-the-fly "bucket engine" driving tax/dividend distribution.
**Deployer / owner (EOA):** `0x63266fEFB52823c7A889c6a24ddd686cd8487918`
**State at recon:** `status = 1` (sale active), `minted ≈ 53.3M WAVE` (Phase 1 cap is 100M), Wave token `tradingEnabled = false`.

## Summary

Two-contract system. `WaveLauncherMainnet` runs a two-phase batch-mint sale; each `mint100()` call mints 100 × 1,000 = 100,000 WAVE in a single tx, keyed on **gas bid** rather than ETH contributed. When the full 1B WAVE is minted, `graduate()` seeds a Uniswap V2 WAVE/WETH pair and flips the token into trading mode. The token itself (`Wave` at `0x92f5…b193`) is heavy: it contains a custom distribution-tracking engine (24 "big" balance buckets plus up to 6 dynamic "small" buckets for refinement), a quadratic buy-tax / flat+integral sell-tax, a 4-hour dividend round system where only holders inside a derived "dividend band" can claim, and sandwich-blocking that reverts any sell in the same block as a buy.

No confirmed findings — this is a characterisation pass. Several concrete concern regions below; the two most operationally serious are the un-timelocked `emergencyWithdrawETH` and the `graduate()` call that mints initial liquidity with zero slippage floors. Most of the novel code (BucketEngine / TaxEngine math, claim accounting) was not fuzzed and deserves dedicated audit time.

## System map

| Contract | Address | Role |
|---|---|---|
| `WaveLauncherMainnet` | `0xB987…2b95` | Gas-auction minter + graduation / LP seeder. Holds 1B WAVE. |
| `Wave` (ERC20) | `0x92f5…b193` | Token. BucketEngine + TaxEngine + DividendEngine + LiquidityManager. Ownable by deployer. |
| UniV2 Pair WAVE/WETH | `0x92d9…06f2` | Created at `init()`; zero reserves until `graduate()`. |
| UniswapV2Router02 | `0x7a25…88D` | Canonical V2 router. |
| UniswapV2Factory | `0x5C69…aA6F` | Canonical V2 factory. |
| WETH | `0xC02a…56Cc2` | Canonical. |
| `InfoPublisher` | `0x4325…2777` | Tiny Ownable contract with one field `officialWebsite`. Not in the trust path. |

Edges (from `resolve_onchain_system`): launcher → token via slot 2; launcher → router via slot 3; token → router (slot 0x2d), → pair (0x2e), → launcher (0x3a), → InfoPublisher (0x3b). The launcher itself holds no direct WAVE transfer permission from the pair — initial LP goes to `0xdead`, which means **no one can ever retrieve the protocol-owned liquidity**; fees are the only revenue path post-graduation.

## Launch flow

1. Owner calls `init(token, router)` → creates WAVE/WETH pair, stores references. Guarded by a one-shot `initialized` flag.
2. Owner calls `startSale()` → requires launcher already holds 1B WAVE, approves router for max, flips `status = 1`.
3. Anyone calls `mint100()` (payable). Two pricing regimes:
   - **Phase 1** (`minted < 100M`): if `tx.gasprice > 0.5 gwei`, a bid of `100_000_000_000 * (gasPrice / 0.01 gwei) * 100` wei is charged, 80% of batch goes to buyer, 20% accrues to LP pool. If `tx.gasprice ≤ 0.5 gwei`, **the buyer receives the full 100,000 WAVE and pays no bid**, only a per-tx gas-fee reimbursement (`70_000 * gasprice * 99`). Nothing is retained for LP.
   - **Phase 2** (`minted ≥ 100M`): reverts unless `tx.gasprice ≥ 0.5 gwei`; always 80% to buyer, 20% pool.
4. When `minted == 1B`, `graduate()` pushes `lpNative` ETH and `lpCount * 200 WAVE` into `router.addLiquidityETH(...)`, LP → `0xdead`, flips `status = 2`, sweeps any residual ETH to owner.
5. Owner calls `Wave.enableTrading()` on the token (gated by `launcher.status() == 2`).

## Wave token mechanics (abbreviated)

- `BucketEngine`: 24 exponentially-spaced big buckets by balance; after the "P_TARGET = 49%"-th balance percentile is located, `_initSmallBuckets` splits that big bucket into 3 small buckets; small buckets split/merge over time to track the distribution's median more finely. `centralValue` tracks the per-holder balance at the 49th cumulative-supply percentile.
- `TaxEngine`: sell tax uses `_integralTax` (linear rate from 10% at balance 0 to 50% at `centralValue`, flat 5% above). Buy tax is a quadratic quadratic: for a buyer with old balance `a` ≤ `C`, the after-tax tokens received solve `b² + b(11C+4a) − (… )`, interpreted as "the more you're filling up toward C, the more tax". Every buyer gets tagged via `_tagMintParticipant` if they acquired from the launcher, and must call `payMintBackTax` before they can trade or claim dividends — i.e., the back-tax replays the quadratic buy tax against their now-held balance.
- `DividendEngine`: 4-hour rounds. At SELL, `_tryDispatchFee` may trigger `executeDistributeTax` (internal callback; `msg.sender == address(this)` gate) which LP-sells half the collected tax for ETH and LPs the other half, plus routes 33.33% of the tax as a dividend WAVE pool. In a new round, eligible holders call `claim()`, where eligibility = `leftBoundary < bal ≤ centralValue` AND not already claimed this round AND `tx.gasprice ≤ block.basefee + 10 gwei` AND `block.timestamp ≥ lastActivity + 4h` AND `block.timestamp ≥ claimUnlocksAt(bal)` (a staggered-release formula `(1 − (bal/C)²) * 4h`). Reward is paid by swapping WAVE dividend tokens → ETH via the router.
- Sandwich block: `BUY` records `_lastBuyBlock[to] = block.number`; a same-block `SELL` from that address reverts with `SandwichBlocked`. Transfers between non-permitted addresses are disabled entirely (`TransferNotAllowed`) unless `perm[from].transferFree == true`.

## Concern regions for audit (suspicions, not findings)

1. **`emergencyWithdrawETH` has no sale-phase guard.** `WaveLauncherMainnet.emergencyWithdrawETH(address payable to)` is `onlyOwner`+`nonReentrant` and transfers the entire ETH balance out. While `status == 1`, this balance is made up of minters' Phase-1 bid ETH, which is supposed to seed LP at graduation. Owner (currently an EOA) can drain it at any point before `graduate()` runs, which both pulls value from the pool and effectively bricks graduation (`lpNative` state variable is untouched, but the matching ETH is gone → `addLiquidityETH` will revert for lack of balance). Recommendation for auditor: confirm on fork that this is drainable, and flag as Critical centralization risk if so.
2. **`graduate()` sets `amountTokenMin = amountETHMin = 0`.** This is the *initial* pair mint so classic sandwich is limited (the pair reserves are zero until this tx), but any griefer who can somehow donate to the pair before this tx lands (e.g., if a test mint put anything into the pair address earlier, or via a malicious `transferFrom` path, or if a reorg reorders this below the pair's `sync`) can shift the initial price. Low likelihood given the UniV2 pair was freshly created by this launcher, but worth confirming no `skim`/`sync` interaction is reachable beforehand.
3. **Phase-1 zero-bid mints bleed the LP pool without contributing.** In Phase 1 with `gasprice ≤ 0.5 gwei`, `totalBid = 0`, so the minter receives the **full 100,000 WAVE** while `lpNative` and `lpCount` are not incremented. The 20% tokens that were supposed to match that bid at graduation are never reserved; instead, the full batch goes to the buyer. Over the 100M Phase-1 window that's up to 20M WAVE potentially leaving the system without any matching LP ETH. Attack: build a custom tx through flashbots/block-builder at sub-0.5-gwei effective gas and farm the full batch per call. Verify against mainnet tx history whether this is actively happening. Not strictly a bug (it's the documented Phase-1 behavior) but worth confirming against protocol docs that the team intends this.
4. **Dividend-token accounting vs. actual WAVE balance.** `DividendEngine._processClaim` computes `reward = taxFromZero(bal)` and caps it at `roundPool - roundClaimed`, but neither bound is `IERC20(this).balanceOf(this)`. Between `_tryDispatchFee` dispatches (which *sell half of taxPending for ETH and LP*, reducing the contract's WAVE holdings), the recorded `pendingDividend` / `roundPool` can plausibly exceed the actual WAVE balance, at which point `_swapDividendToETH(reward)` would run with `balanceOf(this) < amount` and short-circuit to `0`, reverting `claim()` with `NoDividend`. Early claimers get paid, late claimers are stranded; no refund path. Worth invariant-testing.
5. **`claim()` gas-price ceiling** `tx.gasprice > block.basefee + 10 gwei` reverts `GasTooHigh`. If the tx is included via flashbots (0 priority fee) or the chain's base fee is very high, this is fine; but under an MEV-inclusion regime where searchers pay a tip for reliable inclusion, legitimate claimers at >10 gwei tip are locked out until base fee settles. Not a vulnerability — a liveness concern.
6. **`payMintBackTax` griefability.** A mint participant who receives a tiny balance from the launcher (e.g., after a partial transfer pre-enableTrading — which is actually blocked, but verify) can be stuck unable to pay back-tax if `owed >= bal` (reverts `AmountTooSmall`). Likely unreachable in practice because the launcher always sends in 100k-WAVE chunks and the quadratic formula yields `owed << bal` at low balances, but should be fuzzed.
7. **`registerTeam` is unbounded.** Owner can push an arbitrary-length array into `_team`; the team rotates on every taxed trade via `_nextTeam`. A large enough `_team` makes every trade gas-costly and could be used to grief trading. `_nextTeam` uses `% len` rotation so it's O(1), so this is not a gas-bomb; but `_team` is append-only — a wrongly-added address cannot be removed without redeploying. Informational.
8. **Two separate reentrancy locks.** `DividendEngine._reentrancyLock` and `LiquidityManager._swapping` protect different call sites. The `_update` path on SELL invokes `_tryDispatchFee → this.executeDistributeTax → _distributeTax (lockSwap)`. The cross-invocation `claim()` is under `nonReentrant`, but it calls `_swapDividendToETH` (also `lockSwap`), and that calls the router which can re-enter WAVE on token transfer (WAVE's `_update` is reachable from the router via `transferFrom` pulling tokens into the pair). Worth tracing to confirm no cross-lock path lets a claimant influence `centralValue` mid-swap.
9. **`enableTrading` is owner-gated on top of `status == 2`.** If the owner is lost/unreachable after graduation, the token is permanently frozen (`TradingNotEnabled` on every buy/sell). No timelock or fallback. Informational but worth surfacing.

## Scoping notes

- No upgradeability (pure non-proxy contracts); no governance; no timelock; no role-based access — owner is a single EOA for both the launcher and the token.
- No external oracle dependency. Pricing is internal (bucket engine computes `centralValue` from on-chain balances).
- viaIR compilation (`0.8.30`). Storage layout is mostly hand-managed; slot collisions were not audited here.
- Contracts are fully verified (Sourcify full match for Wave and Launcher, Etherscan full match for InfoPublisher).

## Open questions for the audit team

1. Is the Phase-1 "sub-0.5-gwei = free batch" behavior documented / intentional? If yes, is the reduction in LP pool acceptable?
2. Is `emergencyWithdrawETH` intended to be usable during an active sale, or only after `graduate()`? If the latter, it should revert unless `status == 2`.
3. Do the team / protocol docs specify any invariant on `pendingDividend ≤ balanceOf(address(this)) - taxPending`? If yes, it needs to be enforced in `_tryDispatchFee`.
4. What is the intended interpretation of "dividend band" eligibility (`leftBoundary` stays 0 throughout — seemingly unused)? If it was supposed to be raised over time, this is dead code.

---

## Scratchpad (verbatim)

The agent only populated `system_understanding`; the structured
sections (`hypotheses`, `suspicions_not_yet_confirmed`, etc.) stayed
empty because the run was framed as recon, not vuln-hunting. The
prose below contains *six numbered key observations* that map directly
onto the report's "concern regions" — they were authored as
scratchpad notes before being elevated into the final markdown.

```
# Scratchpad — session b6e2dc7c-a44a-43ca-bd97-f04ab4cf0d74

_schema v1, updated 1777088180855ms, 0 items total_

## System understanding

Target: WaveLauncherMainnet @ 0xB9873b482d51b8b0f989DCD6CCf1D91520092b95 (Ethereum mainnet).

System (resolved via resolve_onchain_system):
- **Launcher** (target) 0xB987…2b95 — `WaveLauncherMainnet`. Fair-launch
  / batch-mint sale; accepts ETH via `mint100()` to mint 100 "batches"
  of 1000 WAVE each (100,000 WAVE) per call. Runs in 2 phases; Phase 1
  (≤100M minted, `PHASE1 = 100_000_000e18`) pays 20% to LP, rest to
  buyer if gas bid > 0.5 gwei; Phase 2 requires gas bid ≥ 0.5 gwei.
  `graduate()` adds liquidity to UniV2 pair (dead address LP), sets
  `status = 2`, sweeps residual ETH to owner. Status is read by Wave
  token's `enableTrading`.
- **Wave token** 0x92f5…b193 — ERC20 with a complex tax/dividend/bucket
  engine. Inherits Ownable, DividendEngine (nonReentrant claim),
  LiquidityManager (swap tokens→ETH via Uniswap V2 router), BucketEngine
  (24 "big" buckets + up to 6 "small" buckets subdividing a "median"
  big bucket for percentile-based pricing/"central value"), TaxEngine
  (buy/sell tax, sell≥C → flat; buy uses quadratic). Tax split: 3333
  bps to dividend pool (rounds) + rest LP'd. 4h rounds; claim() only
  eligible holders in the "dividend band" (leftBoundary < bal ≤
  centralValue) can withdraw rounded ETH. Sandwich block: BUY and
  same-block SELL revert. `registerTeam` adds team members; team fee
  = 2 bps rotated via `_nextTeam`. Mint participants tagged when
  launcher transfers; they must `payMintBackTax` before any further
  trading or claim.
- **UniV2 Pair** 0x92d9…06f2 — WAVE/WETH pair
- **UniswapV2Factory** 0x5C69…aA6F, **UniswapV2Router02** 0x7a25…88D,
  **WETH** 0xC02a…756Cc2
- **InfoPublisher** 0x4325…2777 — just a website URL setter.

State check (fetched_at block ~): status=1 (sale active, initialized),
minted not yet total. Trading on token likely not yet enabled
(`enableTrading` requires `launcher.status() == 2`).

Key observations up front:

1. `mint100()` in launcher: refund loop uses `msg.sender.call{value:
   refund}` after state update → nonReentrant-protected. But
   `token.transfer` before refund — WAVE is ERC20, its `_update` is
   complex; from==launcher→to=buyer marks buyer as mintParticipant in
   Wave. OK.
2. Launcher `token.transfer(msg.sender, (BATCH_AMOUNT * 80)/100)` if
   totalBid > 0 — else full BATCH_AMOUNT. In phase-2, totalBid always
   >0 (gas required). In phase-1 if gas<=0.5gwei, buyer gets full
   BATCH_AMOUNT for free (no bid). `lpNative` only accrues when
   totalBid > 0; at graduation, `tokenAmt = lpCount * ((PER_MINT * 20)
   / 100)` = 200 * lpCount WAVE. But those tokens are NOT deducted
   from buyer in the phase-1-gas-too-low path! However the launcher
   initially received TOTAL=1e9 tokens (via Wave constructor
   `_mint(launcher_, INIT_SUPPLY)`) so it's transferring directly from
   own balance. After graduation, any leftover tokens sit in launcher
   with no way to move them — dead weight — but not a security issue,
   more a design issue.
3. `graduate()` requires `minted >= TOTAL`, runs `addLiquidityETH`
   with `amountTokenMin=0, amountETHMin=0` — sandwichable. Tokens
   sent to dead — LP is not recoverable anyway, but attacker can
   skim value. The LP pair has zero prior reserves at graduation so
   this is actually the INITIAL mint, which sets the price.
   First-liquidity-provider donation attack possible to manipulate
   starting price? Actually `addLiquidityETH` creates pair/initial
   mint → not manipulable by donation because it's the first mint;
   reserves are zero until this call. Someone could however frontrun
   by calling `swap` before `addLiquidityETH`? No — pair has no
   tokens, any swap would revert. OK.
4. `emergencyWithdrawETH` is onlyOwner — owner can drain ETH. But only
   from accumulated ETH; once graduated, that sweeps to owner anyway.
   During sale, owner can pull bids mid-sale (centralization risk),
   which defeats graduation. **Significant owner trust assumption**.
5. Wave token has `_reentrancyLock` separate from LiquidityManager
   `_swapping`. `_tryDispatchFee` invoked on SELL path before
   super._update; calls `this.executeDistributeTax(lpPortion)` via
   external call — requires msg.sender==this. This internal roundtrip
   through distributeTax → LiquidityManager swap → router. Should be
   safe because `lockSwap` guard.
6. Claim path: `_swapDividendToETH(reward)` swaps **WAVE tokens** from
   `address(this)` — requires token.balanceOf(this) >= reward. But
   `reward = taxFromZero(bal)` — is reward denominated in WAVE or ETH?
   Let me check; looks like reward is computed as tax-integral over
   bucket balance (dimensionally tokens). Then swapped to ETH and sent
   to claimer. The reward is computed per-user but dispatched pool is
   `pendingDividend` (in WAVE). Accounting seems to not check whether
   reward ≤ pendingDividend: it checks `remaining = roundPool -
   roundClaimed` and `if (reward > remaining) reward = remaining`.
   But this cap doesn't check whether `address(this)` actually holds
   enough WAVE to swap! The contract's tax funds (taxPending +
   pendingDividend) both live as WAVE in this contract. When
   distributeTax sells half, WAVE drops. `pendingDividend` accounting
   could exceed actual WAVE held. Check more carefully.

Phase: Recon, not full audit. Goal: produce brief + note anything
suspicious.

## Hypotheses
_(empty)_

## Confirmed findings
_(empty)_

## Dismissed hypotheses
_(empty)_

## Open questions
_(empty)_

## Investigations
_(empty)_

## Limitations noticed
_(empty)_

## Suspicions (not yet confirmed)
_(empty)_
```

---

## Self-critique (verbatim)

Recorded by the agent via `finalize_self_critique` immediately
before `finalize_report`. Three fields, full text.

### findings_quality_assessment

> This was a recon pass, not a vulnerability hunt, so I recorded no
> findings. What I *do* trust: the system map (launcher → Wave token
> → UniV2 pair → router/factory/WETH), the current on-chain state
> (status=1, Phase 1 active, ~53M of 100M Phase-1 cap minted, trading
> disabled on the token), and the identification of a handful of
> concrete concern regions in the launcher and Wave token: (a)
> `graduate()` calls `router.addLiquidityETH` with
> `amountTokenMin=0, amountETHMin=0`; (b) `emergencyWithdrawETH` is
> onlyOwner with no lock on the sale state, so the EOA owner can
> drain accumulated bid ETH mid-sale and defeat graduation entirely;
> (c) in Phase 1 with gas bid ≤ 0.5 gwei, `mint100` transfers the full
> 100k WAVE batch for free ETH gas only — no bid ETH is collected,
> yet the same pool of launcher tokens is drawn down, creating a soft
> rug pattern where a cheap-gas minter gets tokens that were supposed
> to bootstrap LP; (d) Wave's `_swapDividendToETH` uses WAVE reward
> sized by `taxFromZero(bal)` with no check that the contract
> actually holds that many WAVE tokens — accounting vs.
> actual-balance drift is plausible after many LP dispatches. I did
> not prove any of these; all are suspicions to be run down in a real
> audit. What I don't trust: I only skimmed TaxEngine math and the
> small/big bucket rebalancing logic, which is non-trivial and the
> most likely place for a real finding. I also did not verify the
> `claim()` → `_swapDividendToETH` path against actual token
> accounting.

### methodology_gaps

> Three real gaps. (1) I relied on a single pass through the source —
> never built/ran a forge test against a mainnet fork to probe
> invariants (e.g., does `pendingDividend` stay ≤
> `balanceOf(address(this))`? does a sub-0.5-gwei mint in Phase 1
> actually bypass bidding? can the owner's `emergencyWithdrawETH`
> actually pull funds while status==1?). For a recon brief this is
> acceptable, but the operator note says "first-run trial" and at
> least one spot-check simulation would have upgraded several
> suspicions to findings or dismissed them. (2) I did not run
> `trace_state_dependencies` on any function — I reasoned about
> SSTORE/CALL ordering by reading the Solidity, not by confirming
> what the compiler actually emitted under viaIR. viaIR can reorder
> in ways that matter for reentrancy reasoning. (3) The BucketEngine
> math (`_splitSmallBucket`, `_mergeFarthest`, `_interpolate`,
> `taxFromZero`) is the most novel part of this code and the most
> likely to contain an integer-boundary or invariant break, but I did
> no fuzzing, symbolic work, or even pen-and-paper on bounds. I
> flagged it for the reviewer but didn't do the work.

### what_to_improve

> On a comparable fair-launch / bonding-curve target: (a) always do
> one fork simulation per hypothesis before writing it up — cheap
> signal, dramatically improves the brief's trust level; (b) run
> `trace_state_dependencies` on every state-mutating entrypoint
> (`mint100`, `graduate`, `emergencyWithdrawETH`, Wave's `_update`,
> `claim`, `payMintBackTax`, `settleRound`) before calling recon
> complete, so the CALL/SSTORE ordering claims in the brief are
> backed by tool output rather than re-reading source; (c) for novel
> math engines like BucketEngine, spend 5–10 turns writing a forge
> test that hammers the buckets with many holders of various sizes
> and asserts invariants (`sum(bigBuckets[i].totalBalance) ==
> totalSystemBalance`,
> `sum(smallBuckets[i].holderCount) ==
> bigBuckets[medianBigId].holderCount`, etc.); (d) check mainnet tx
> history (creation tx, large mint100 calls) to see whether Phase-1
> gas≤0.5gwei mints are actually being used in the wild, which would
> upgrade the "free tokens" observation from a design concern to an
> active exploit.

---

## Why this run is the report's headline artifact

1. **Novel target** — no training-data shortcut; agent had to derive
   custom mechanics (BucketEngine, TaxEngine, DividendEngine) from
   source.
2. **Substantive output** — 9 concern regions, three of which are
   credibly worth follow-up (`emergencyWithdrawETH` drain,
   Phase-1 zero-bid bleed, dividend-vs-balance accounting drift).
3. **Tool composition validated** — `resolve_onchain_system` (308s
   recon) → `static_call ×5` (live state) → `search_knowledge_base ×2`
   (cross-checking patterns against past Solodit findings) →
   `finalize_self_critique` voluntarily → `finalize_report`. The
   compounding-loop arc Set 7 set up is operating as designed.
4. **Self-critique quality** — the agent named the analytical tools
   it should have used (`trace_state_dependencies`,
   `build_and_run_foundry_test`), four concrete process recipes for
   a comparable next target, and the specific forge-test invariants
   worth probing (`sum(bigBuckets[i].totalBalance) ==
   totalSystemBalance`, etc.). This is the operator-feedback channel
   the self-critique infrastructure was built to produce — it works.
5. **Cost envelope** — $25.12 / 9 minutes / 1.6M tokens is the real
   reference point for "Opus on a novel target." Higher than Euler
   ($8.78) because Euler's mechanics had training-data recall;
   WaveLauncher required ground-up derivation.

## What this run did NOT validate (and what would)

- The `--vuln` initial-message fix (commit `8d67037`) hadn't landed
  yet, so the user-role message asked for "reconnaissance" and the
  agent (correctly) produced a recon brief. A re-run on the same
  target post-fix is still pending and would be the canonical
  Set-9 vuln-hunt artifact.
- No `simulate_call_chain` calls — the rail accepted the run
  because the registry is correct, but the agent didn't use the
  exec backend. The post-fix re-run should exercise it on at least
  Concern #1 (`emergencyWithdrawETH` drain).
- No `record_suspicion` calls — the 9 concerns live in the final
  markdown only, not in `~/.basilisk/feedback/suspicions.jsonl`.
  The post-fix re-run should populate that file.

## Reproducing this run

```bash
# Pull the original (this run's) artifacts:
sqlite3 ~/.basilisk/sessions.db \
  "SELECT final_report_markdown FROM sessions WHERE id = 'b6e2dc7c-a44a-43ca-bd97-f04ab4cf0d74';"

audit session scratchpad show b6e2dc7c-a44a-43ca-bd97-f04ab4cf0d74

sqlite3 ~/.basilisk/sessions.db \
  "SELECT payload_json FROM session_feedback WHERE session_id = 'b6e2dc7c-a44a-43ca-bd97-f04ab4cf0d74';"

# Re-run the same target with the post-fix binary (vuln framing):
audit recon 0xB9873b482d51b8b0f989DCD6CCf1D91520092b95 \
  --chain ethereum --agent --vuln \
  --provider openrouter --model anthropic/claude-opus-4.7 \
  --agent-output=pretty \
  --session-note "wave-launcher post-fix vuln hunt"
```
