# Installation

## Prerequisites

| Requirement | Version | Notes |
|---|---|---|
| Rust toolchain | 1.94+ | Install via [rustup](https://rustup.rs) |
| Foundry (`forge`, `anvil`) | Latest | Required for PoC synthesis and forked simulation |
| Git | Any | Used for shallow repo cloning |

### Install Foundry

```bash
curl -L https://foundry.paradigm.xyz | bash
foundryup
```

Verify: `forge --version && anvil --version`

## Building from source

```bash
# Clone the repository
git clone https://github.com/eminivance/basilisk
cd basilisk

# Build and install the `audit` binary
cargo install --path crates/cli

# Verify
audit --version
```

The build compiles all 21 workspace crates. Expect 2–4 minutes on a first build; subsequent builds are incremental.

## Development build

For faster iteration during development:

```bash
cargo build --package basilisk-cli
# Binary at: target/debug/audit
./target/debug/audit --version
```

## Configuration

Copy the example environment file and fill in your API keys:

```bash
cp .env.example .env   # if provided, or create manually
$EDITOR .env
```

At minimum you need one LLM provider key. See [Configuration](/configuration) for the full list.

### Minimal `.env` for recon

```env
ANTHROPIC_API_KEY=sk-ant-...
ETHERSCAN_API_KEY=...
ALCHEMY_API_KEY=...
```

### Minimal `.env` for vuln mode

```env
ANTHROPIC_API_KEY=sk-ant-...
ETHERSCAN_API_KEY=...
ALCHEMY_API_KEY=...
VOYAGE_API_KEY=...          # for knowledge base search
```

## Verifying the install

```bash
# Should print usage
audit --help

# Should list 5 benchmark targets
audit bench list

# Should print cache stats (zero on first run)
audit cache stats
```

## Updating

```bash
git pull
cargo install --path crates/cli --force
```

## Storage locations

| Path | Contents |
|---|---|
| `~/.basilisk/sessions.db` | Session history (SQLite) |
| `~/.basilisk/repos/` | Cloned Git repositories |
| `~/.basilisk/knowledge/` | Vector store + ingest state |
| `~/.cache/basilisk/` | RPC and explorer response cache |
