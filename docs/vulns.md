# Vulnerability Analysis

Vuln mode (`--vuln`) gives the agent three additional analytical tools and a higher budget. These tools bridge the gap between "reading source" and "proving an exploit works."

## The three analytical tools

### `find_callers_of`

Given a target contract address and a function selector, finds every location in the system that calls that function.

```text
find_callers_of(address="0x...", selector="0xa9059cbb")  // transfer(address,uint256)
→ [
    CallerHit { file: "contracts/Vault.sol", line: 142, confidence: High },
    CallerHit { file: "contracts/Strategy.sol", line: 89, confidence: Medium },
  ]
```

**Precision levels:**
- `AstPrecision` — full match from Solidity AST (requires source)
- `BytecodePattern` — pattern match in EVM bytecode (works without source)

Used to answer: "who can trigger this function?", "is this access-controlled?", "what paths lead here?"

### `trace_state_dependencies`

For a specific function in a contract, returns the complete set of storage slots it reads and writes, plus all external calls it makes.

```text
trace_state_dependencies(address="0x...", selector="0x...")
→ StateDeps {
    reads:  [SlotRef { slot: 0, label: "balances" }, ...],
    writes: [SlotRef { slot: 0, label: "balances" }],
    calls:  [ExternalCall { target: "0x...", selector: "0x..." }],
  }
```

Used for:
- Reentrancy analysis (external call before state update?)
- State-machine invariant checking (what state must hold before this runs?)
- Cross-contract dependency mapping

### `simulate_call_chain`

Runs an ordered sequence of calls against a forked copy of mainnet state and returns per-call outcomes plus state diffs.

```text
simulate_call_chain([
  { from: attacker, to: protocol, data: "0x..." },   // setup
  { from: attacker, to: flashloan, data: "0x..." },  // borrow
  { from: attacker, to: protocol, data: "0x..." },   // exploit
  { from: attacker, to: flashloan, data: "0x..." },  // repay
])
→ SimulationResult {
    steps: [
      CallStep { success: true, gas_used: 21000, ... },
      CallStep { success: true, state_diff: { balance_delta: +1000 ETH }, ... },
      CallStep { success: true, state_diff: { balance_delta: -900 ETH }, ... },
      CallStep { success: true },
    ],
  }
```

Used to verify that a hypothesised exploit actually works on real state before writing the PoC test.

## PoC synthesis

### `build_and_run_foundry_test`

The agent writes a Foundry test file — Solidity code implementing the exploit — and this tool compiles and runs it against a forked network.

```text
build_and_run_foundry_test(
  test_code: "pragma solidity ^0.8.0; contract Exploit is Test { ... }",
  fork_block: 17_000_000,
  chain: "ethereum",
)
→ { passed: true, gas: 450000, stdout: "...", stderr: "" }
```

This is the final verification step: a passing Foundry test is proof that the vulnerability is real and exploitable, not just theoretical.

## EVM execution backend

Simulations run via `AnvilForkBackend` in `basilisk-exec`:

- **Fork at block** — state snapshot at any historical block
- **Impersonation** — `eth_impersonateAccount` for arbitrary sender
- **Cheatcodes** — full Foundry cheatcode support (`vm.prank`, `vm.deal`, etc.)
- **Anvil lifecycle** — each simulation spawns a fresh `anvil` subprocess; cleaned up on completion or Ctrl-C via the global fork registry

`MockExecutionBackend` provides deterministic in-process simulation for unit tests.

## Scratchpad tools (vuln mode only)

| Tool | Purpose |
|---|---|
| `record_suspicion` | Add a hypothesis not yet backed by evidence |
| `record_limitation` | Note a gap in the analysis (missing source, inaccessible state, etc.) |
| `record_finding` | Record a confirmed, exploitable vulnerability |
| `finalize_self_critique` | Self-review before final report — check for false positives, missed severity, incomplete PoCs |

The self-critique step runs automatically before `finalize_report` in vuln mode.

## Calibration benchmark

Five real post-exploit targets test the agent's ability to find known vulnerabilities:

| Target | Vulnerability type | Block |
|---|---|---|
| Euler Finance | Flash loan + donation attack | 16,817,995 |
| Visor Finance | Reentrancy in deposit | 13,770,907 |
| Cream Finance | Flash loan + price manipulation | 13,492,447 |
| Beanstalk | Governance flash loan | 14,595,904 |
| Nomad Bridge | Merkle root initialisation flaw | 15,259,100 |

```bash
audit bench run euler
audit bench history
audit bench compare <run-a> <run-b>
```

Scoring uses keyword matching against expected findings. Use `audit bench review <run-id>` to manually label misses and false positives for calibration feedback.
