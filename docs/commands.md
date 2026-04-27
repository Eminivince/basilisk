# Commands Reference

The `audit` binary exposes six top-level subcommands.

```
audit [--json-logs | --pretty-logs] <COMMAND>

COMMANDS:
  recon       Audit a target via the LLM agent
  session     Inspect, resume, and delete sessions
  knowledge   Manage the knowledge base
  cache       Inspect and manage on-disk cache
  bench       Benchmark harness (5 calibration targets)
  doc         Serve this documentation on localhost
```

---

## `audit recon`

Start an agent session against a target.

```
audit recon <TARGET> [OPTIONS]

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
audit recon 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 --chain ethereum

# Deep vulnerability hunt on a GitHub repo
audit recon https://github.com/compound-finance/compound-protocol --vuln

# Use a cheaper model with a tighter budget
audit recon ./my-protocol --model claude-sonnet-4-6 --max-cost 200

# Target a specific chain
audit recon 0x... --chain arbitrum
```

---

## `audit session`

Manage persisted agent sessions.

```
audit session <COMMAND>

COMMANDS:
  list                    List recent sessions (newest first)
  show <SESSION_ID>       Print full transcript
  resume <SESSION_ID>     Continue an interrupted session
  delete <SESSION_ID>     Remove session and all its turns
  scratchpad <CMD>        Inspect agent working memory
```

### `audit session list`

```bash
audit session list
# Shows: id, target, mode, status, turns, cost, timestamp
```

### `audit session show`

```bash
audit session show <id>
audit session show <id> --report-only    # just the final report
audit session show <id> --format json    # machine-readable
```

### `audit session resume`

Resumes an interrupted session from where it left off. The system prompt must match (same mode). Budget is reset relative to what was consumed.

```bash
audit session resume <id>
```

### `audit session scratchpad`

The agent maintains a working scratchpad — a structured document it updates across turns.

```bash
audit session scratchpad show <id>        # full scratchpad
audit session scratchpad summary <id>     # compact summary
audit session scratchpad export <id>      # JSON export
audit session scratchpad delete <id>      # wipe (cannot undo)
```

---

## `audit knowledge`

Manage the semantic knowledge base.

```
audit knowledge <COMMAND>

COMMANDS:
  ingest <SOURCE>      Ingest a corpus into the KB
  search <QUERY>       Natural-language KB search
  add-protocol         Index a protocol's documentation
  list-findings        List curated KB findings
  show-finding <ID>    Show one finding in detail
  correct <ID>         Mark a finding as incorrect
  dismiss <ID>         Mark a finding as a false positive
  confirm <ID>         Confirm a finding as valid
```

### Ingest sources

```bash
audit knowledge ingest solodit        # from solodit_dump.jsonl
audit knowledge ingest swc            # Smart Contract Weakness Classification
audit knowledge ingest openzeppelin   # OZ security advisories
audit knowledge ingest code4rena      # clones Code4rena findings repos
audit knowledge ingest sherlock       # clones Sherlock audit reports
audit knowledge ingest rekt           # from rekt.jsonl (operator-curated)
audit knowledge ingest trailofbits    # from trailofbits.jsonl (operator-curated)
audit knowledge ingest --all          # all sources
```

### Search

```bash
audit knowledge search "reentrancy via ERC777 callback"
audit knowledge search "flash loan oracle manipulation" --limit 10
```

### Add a protocol

```bash
audit knowledge add-protocol uniswap-v3 --github https://github.com/Uniswap/v3-core
audit knowledge add-protocol aave-v3 --pdf ./aave-v3-technical-paper.pdf
audit knowledge add-protocol curve --url https://docs.curve.fi
```

---

## `audit cache`

```
audit cache <COMMAND>

COMMANDS:
  stats           Entry counts and byte totals per namespace
  repos           Manage cloned Git repository cache
  clear           Reclaim disk space
```

```bash
audit cache stats
audit cache repos list
audit cache repos clear
audit cache clear
```

Cache lives at `~/.cache/basilisk/` (RPC/explorer) and `~/.basilisk/repos/` (Git).

---

## `audit bench`

The benchmark harness runs the auditor against five real post-exploit protocols and scores findings.

```
audit bench <COMMAND>

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
audit bench list
audit bench run euler
audit bench history
audit bench compare <run-a-id> <run-b-id>
```

---

## `audit doc`

Serve this documentation locally.

```
audit doc [OPTIONS]

OPTIONS:
  --port <PORT>    Port to listen on [default: 3000]
  --open           Open in default browser immediately
```

```bash
audit doc                   # serve at http://localhost:3000
audit doc --port 8080       # custom port
audit doc --open            # auto-open browser
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
