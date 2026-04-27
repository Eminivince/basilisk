# Agent System

The agent is a tool-use LLM loop that drives the entire audit. It runs until it calls `finalize_report`, a budget cap is hit, or the user interrupts with Ctrl-C.

## How it works

```
┌─────────────────────────────────────────────────────────┐
│                    Agent Loop                           │
│                                                         │
│  system prompt + scratchpad                             │
│       │                                                 │
│       ▼                                                 │
│   LLM turn  →  tool_calls[]  →  execute tools          │
│       ▲               │                                 │
│       └───────────────┘  (tool results appended)        │
│                                                         │
│  repeat until: report finalized | budget hit            │
└─────────────────────────────────────────────────────────┘
```

Each turn the model receives the full conversation history (system prompt + all past turns + tool results) and decides which tools to call. Tool results are appended and the next turn begins.

## Tool registries

### Recon mode (14 tools)

| Tool | Purpose |
|---|---|
| `classify_target` | Identify input as on-chain address / GitHub repo / local path |
| `resolve_onchain_system` | Expand full contract graph (proxies, diamonds, etc.) |
| `resolve_onchain_contract` | Resolve a single contract's source and bytecode |
| `analyze_project` | Parse Foundry/Hardhat project: remappings, file graph, imports |
| `fetch_github_repo` | Shallow-clone a repo; reuse cache if available |
| `list_directory` | List files under a path |
| `read_file` | Read file contents (line range supported) |
| `grep_project` | Regex search across the resolved project |
| `static_call` | Read-only on-chain call (eth_call) |
| `get_storage_slot` | Read raw storage slot value |
| `search_knowledge_base` | Semantic search over the KB corpus |
| `search_protocol_docs` | Search protocol-specific indexed docs |
| `search_similar_code` | Find similar code patterns in the KB |
| `finalize_report` | Emit the structured audit report and end the session |

### Vuln mode (25 tools — recon tools plus)

| Tool | Purpose |
|---|---|
| `find_callers_of` | Find all callers of a function across the system |
| `trace_state_dependencies` | List storage slots read/written + external calls for a function |
| `simulate_call_chain` | Run ordered call sequences on a forked EVM |
| `build_and_run_foundry_test` | Write and execute a Foundry test (PoC synthesis) |
| `record_suspicion` | Add an unverified hypothesis to the scratchpad |
| `record_limitation` | Document an analysis gap |
| `record_finding` | Record a confirmed vulnerability |
| `finalize_self_critique` | Produce a critique of the report before final submission |

## Budget enforcement

Every session has hard caps. When any cap is reached, the agent stops, marks the session as `interrupted`, and prints what it has so far. Interrupted sessions can be resumed.

| Dimension | Recon default | Vuln default | CLI override |
|---|---|---|---|
| Turns | 40 | 100 | `--max-turns` |
| Total tokens | 500,000 | 2,000,000 | `--max-tokens` |
| Cost (USD) | $5.00 | $50.00 | `--max-cost` |
| Wall clock | 20 min | 60 min | `--max-duration` |

## Session persistence

Every turn is persisted to SQLite at `~/.basilisk/sessions.db` as it completes. Fields stored per session:

- Target, mode, model, provider
- Start/end timestamps
- Status (`running`, `completed`, `interrupted`, `error`)
- Total turns, input/output tokens, cost in cents

Tool calls are stored separately with their inputs, outputs, duration, and whether they errored.

## Scratchpad (working memory)

The agent maintains a structured scratchpad that it updates across turns — separate from the LLM context window. Sections:

| Section | Purpose |
|---|---|
| `system_understanding` | Architecture + trust model notes |
| `hypotheses` | Active theories under investigation |
| `confirmed_findings` | Validated vulnerabilities |
| `dismissed_hypotheses` | Dead ends with reasons |
| `open_questions` | Things that need more investigation |
| `investigations` | Ongoing analysis threads |
| `limitations_noticed` | Analysis gaps and missing data |
| `suspicions_not_yet_confirmed` | Weak signals |

The compact form of the scratchpad is injected into the system prompt on each turn, keeping it visible without consuming full context.

## System prompts

System prompts are embedded in the binary at compile time:

- `recon_v1.md` — classification, resolution, synthesis mandate
- `vuln_v2.md` — adversarial-mode, drainage-only vulnerability mandate (Set 9.5)

The vuln prompt instructs the model to focus exclusively on vulnerabilities that could drain funds or cause protocol-level damage — no informational findings.

## Streaming and output

The agent streams its thinking and tool calls to the terminal in real time. Tool results are printed as they complete. The final report is printed in Markdown when `finalize_report` is called.

`AgentObserver` trait lets callers hook into: turn start/end, tool call/result, and the final outcome. The CLI uses this for progress display and stats.

## Stop reasons

| Reason | Meaning |
|---|---|
| `ReportFinalized` | Agent called `finalize_report` — normal completion |
| `BudgetHit` | A cost cap was reached |
| `MaxTurns` | Turn limit hit |
| `MaxTokens` | Token limit hit |
| `MaxCost` | Dollar limit hit |
| `MaxDuration` | Time limit hit |
| `Interrupted` | User pressed Ctrl-C |
