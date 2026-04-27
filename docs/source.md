# Source Analysis

Once verified Solidity source is fetched, Basilisk locates the underlying Git repository, clones it, and constructs a full picture of the project layout.

## Git repository cache

`basilisk-git` maintains a shallow-clone cache at `~/.basilisk/repos/`. Clones are keyed by commit SHA — the same commit is never cloned twice, and the cache survives across sessions.

```text
~/.basilisk/repos/
  <sha>/          ← bare git clone
  <sha>.meta.json ← {url, cloned_at, depth, commit}
```

On a cache hit, the tool returns the cached path immediately. On a miss, it shallow-clones at depth 1 (or deeper if history is needed) and stores the result.

## Project detection

`basilisk-project` inspects the cloned tree and detects the build framework:

| Framework | Detection signal |
|---|---|
| Foundry | `foundry.toml` present |
| Hardhat | `hardhat.config.ts` or `hardhat.config.js` |
| Truffle | `truffle-config.js` |
| Mixed | Multiple config files coexist |
| Unknown | None of the above |

## Foundry project analysis

For Foundry projects, `analyze_project` parses `foundry.toml` and recovers:

- **Source roots** — `src/`, `contracts/`, or custom
- **Test roots** — `test/`
- **Script roots** — `script/`
- **Remappings** — from `foundry.toml` + `remappings.txt`
- **Import graph** — which files import which
- **Unresolved imports** — libs not present in the repo (useful for spotting missing context)

Remappings are used when reading files — the agent can resolve `@openzeppelin/...` imports correctly.

## Hardhat project analysis

For Hardhat projects:

- Parses `hardhat.config.ts` / `hardhat.config.js` for compiler settings
- Recovers `sources` path (default `contracts/`)
- Extracts network configurations and named accounts

## File tools

The agent uses three low-level file tools:

### `list_directory`

Lists files under a directory path. Respects project boundaries — won't traverse outside the cloned repo root.

```text
list_directory("contracts/")
→ ["ERC20.sol", "Vault.sol", "interfaces/IVault.sol", ...]
```

### `read_file`

Reads file contents. Supports line range selection to avoid overwhelming the context window with large files.

```text
read_file("contracts/Vault.sol", start_line=1, end_line=120)
```

### `grep_project`

Regex search across the entire project tree. Returns filename + line number + matched line. Used to find all callers of a function, all uses of a storage variable, etc.

```text
grep_project("onlyOwner")
grep_project("transfer\\(.*,.*\\)")
grep_project("delegatecall")
```

## GitHub integration

`basilisk-github` provides a thin GitHub REST v3 client used for:

- **Ref resolution** — convert a branch name or tag to a commit SHA
- **Default branch lookup** — find `main` vs `master` vs custom
- **Rate limit management** — unauthenticated (60 req/hr) vs authenticated with `GITHUB_TOKEN` (5000 req/hr)

All GitHub responses are cached for 1 hour.

## Source → agent flow

```text
audit recon https://github.com/protocol/contracts

  1. fetch_github_repo(url)
       → GitHub API: resolve HEAD → commit SHA
       → cache hit? return path : shallow clone
       → return local path

  2. analyze_project(path)
       → detect framework (Foundry)
       → parse foundry.toml
       → build import graph
       → return project summary

  3. Agent: list_directory, read_file, grep_project
       → navigate source tree
       → identify critical contracts and functions
       → build mental model of the system
```
