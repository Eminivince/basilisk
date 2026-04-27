# Commands Reference

The `basilisk` binary exposes six top-level subcommands.

```
basilisk [--json-logs | --pretty-logs] <COMMAND>

COMMANDS:
  recon       Audit a target via the LLM agent
  session     Inspect, resume, and delete sessions
  knowledge   Manage the knowledge base
  cache       Inspect and manage on-disk cache
  bench       Benchmark harness (5 calibration targets)
  doc         Serve this documentation on localhost
```

---

## `basilisk recon`

Start an agent session against a target.

```
basilisk recon <TARGET> [OPTIONS]

ARGS:
  <TARGET>    On-chain address, GitHub URL, or local path

OPTIONS:
  --chain <CHAIN>               Chain for on-chain targets [default: ethereum]
  --vuln                        Enable vulnerability-hunting mode (25 tools, higher budget)
  --model <MODEL>               LLM model override (e.g. claude-opus-4-7)
  --provider <PROVIDER>         LLM provider (anthropic | openrouter | openai | ollama)
  --max-turns <N>               Turn cap [default: 40 recon / 100 vuln]
  --max-cost <CENTS>            USD cap in cents [default: 500 / 5000]
  --max-tokens <N>              Total token cap
  --max-duration <SECS>         Wall-clock time cap in seconds
```

### Examples

```bash
# Audit a deployed contract on Ethereum mainnet
basilisk recon 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 --chain ethereum

# Deep vulnerability hunt on a GitHub repo
basilisk recon https://github.com/compound-finance/compound-protocol --vuln

# Use a cheaper model with a tighter budget
basilisk recon ./my-protocol --model claude-sonnet-4-6 --max-cost 200

# Target a specific chain
basilisk recon 0x... --chain arbitrum
```

---

## `basilisk session`

Manage persisted agent sessions.

```
basilisk session <COMMAND>

COMMANDS:
  list                    List recent sessions (newest first)
  show <SESSION_ID>       Print full transcript
  resume <SESSION_ID>     Continue an interrupted session
  delete <SESSION_ID>     Remove session and all its turns
  scratchpad <CMD>        Inspect agent working memory
```

### `basilisk session list`

```bash
basilisk session list
# Shows: id, target, mode, status, turns, cost, timestamp
```

### `basilisk session show`

```bash
basilisk session show <id>
basilisk session show <id> --report-only    # just the final report
basilisk session show <id> --format json    # machine-readable
```

### `basilisk session resume`

Resumes an interrupted session from where it left off. The system prompt must match (same mode). Budget is reset relative to what was consumed.

```bash
basilisk session resume <id>
```

### `basilisk session scratchpad`

The agent maintains a working scratchpad — a structured document it updates across turns.

```bash
basilisk session scratchpad show <id>        # full scratchpad
basilisk session scratchpad summary <id>     # compact summary
basilisk session scratchpad export <id>      # JSON export
basilisk session scratchpad delete <id>      # wipe (cannot undo)
```

---

## `basilisk knowledge`

Manage the semantic knowledge base — ingest vulnerability corpora, index protocol docs, search findings, and curate agent output.

```
basilisk knowledge <COMMAND>

COMMANDS:
  ingest <SOURCE>      Ingest a corpus into the KB
  stats                Show entry counts per collection
  search <QUERY>       Natural-language KB search
  add-protocol         Index a protocol's documentation
  list-findings        List curated KB findings
  show-finding <ID>    Show one finding in detail
  correct <ID>         Mark a finding as incorrect
  dismiss <ID>         Mark a finding as a false positive
  confirm <ID>         Confirm a finding as valid
```

### First-time setup

```bash
# Seed the public corpus (requires VOYAGE_API_KEY or OPENAI_API_KEY for embeddings)
basilisk knowledge ingest swc            # Smart Contract Weakness Classification (~50 entries)
basilisk knowledge ingest openzeppelin   # OZ security advisories
basilisk knowledge ingest code4rena      # Code4rena findings repos — needs GITHUB_TOKEN
basilisk knowledge ingest sherlock       # Sherlock audit reports — needs GITHUB_TOKEN
basilisk knowledge ingest --all          # all sources (+ solodit/rekt/trailofbits if dumps present)

# Confirm what was indexed
basilisk knowledge stats
```

### Ingest sources

| Source | Requires | Notes |
|---|---|---|
| `swc` | nothing | Standard weakness taxonomy |
| `openzeppelin` | nothing | OZ library security advisories |
| `code4rena` | `GITHUB_TOKEN` | Clones `code-423n4/<contest>-findings` repos |
| `sherlock` | `GITHUB_TOKEN` | Clones Sherlock audit report repos |
| `solodit` | `~/.basilisk/knowledge/solodit_dump.jsonl` | Operator-provided JSONL dump |
| `rekt` | `~/.basilisk/knowledge/rekt_dump.jsonl` | Operator-provided post-mortems |
| `trailofbits` | `~/.basilisk/knowledge/tob_dump.jsonl` | Operator-provided blog writeups |

```bash
basilisk knowledge ingest code4rena --max-records 5000   # cap large corpora
```

### Search

```bash
basilisk knowledge search "reentrancy via ERC777 callback"
basilisk knowledge search "flash loan oracle manipulation" --limit 10
basilisk knowledge search "rounding error" --collection public_findings
basilisk knowledge search "aave liquidation flow" --collection protocol_docs
```

### Add protocol documentation

Index engagement-specific docs before auditing a target so the agent can retrieve them with `search_protocol_docs`.

```bash
# From a GitHub repo (indexes .md and .sol files)
basilisk knowledge add-protocol uniswap-v3 --github https://github.com/Uniswap/v3-core
basilisk knowledge add-protocol aave-v3 --github aave/aave-v3-core:docs   # subdirectory

# From a PDF whitepaper
basilisk knowledge add-protocol aave-v3 --pdf ./aave-v3-technical-paper.pdf

# From a documentation URL
basilisk knowledge add-protocol curve --url https://docs.curve.fi
basilisk knowledge add-protocol compound --url https://docs.compound.finance
```

### Manage findings

```bash
basilisk knowledge list-findings
basilisk knowledge show-finding <id>
basilisk knowledge correct <id>  --reason "unreachable — guard catches it"
basilisk knowledge dismiss <id>  --reason "false positive — invariant maintained by caller"
basilisk knowledge confirm <id>
```

---

## `basilisk cache`

```
basilisk cache <COMMAND>

COMMANDS:
  stats           Entry counts and byte totals per namespace
  repos           Manage cloned Git repository cache
  clear           Reclaim disk space
```

```bash
basilisk cache stats
basilisk cache repos list
basilisk cache repos clear
basilisk cache clear
```

Cache lives at `~/.cache/basilisk/` (RPC/explorer) and `~/.basilisk/repos/` (Git).

---

## `basilisk bench`

The benchmark harness runs the auditor against five real post-exploit protocols and scores findings.

```
basilisk bench <COMMAND>

COMMANDS:
  list             Show the 5 calibration targets
  show <TARGET>    Dossier for one target
  run [<TARGET>]   Run a vuln session and score it
  history          Newest-first run log
  score <RUN_ID>   Re-score an existing run
  compare <A> <B>  Side-by-side diff of two runs
  review <RUN_ID>  Interactively label misses / false positives
```

Calibration targets: **Euler Finance**, **Visor Finance**, **Cream Finance**, **Beanstalk**, **Nomad Bridge** — each pinned to the block before the exploit.

```bash
basilisk bench list
basilisk bench run euler
basilisk bench history
basilisk bench compare <run-a-id> <run-b-id>
```

---

## `basilisk doc`

Serve this documentation locally.

```
basilisk doc [OPTIONS]

OPTIONS:
  --port <PORT>    Port to listen on [default: 3000]
  --open           Open in default browser immediately
```

```bash
basilisk doc                   # serve at http://localhost:3000
basilisk doc --port 8080       # custom port
basilisk doc --open            # auto-open browser
```

Press `Ctrl-C` to stop the server.

---

## Global flags

These apply to all subcommands:

| Flag | Description |
|---|---|
| `--json-logs` | Emit structured JSON logs (useful for piped output) |
| `--pretty-logs` | Force human-readable logs (overrides TTY detection) |
| `--help` | Show help for any command or subcommand |
| `--version` | Print the build version |
