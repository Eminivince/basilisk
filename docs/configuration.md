# Configuration

Basilisk reads configuration from environment variables. Values can be set in a `.env` file in the project root (loaded automatically via `dotenvy`) or in the shell environment.

## LLM provider keys

| Variable | Purpose | Default |
|---|---|---|
| `ANTHROPIC_API_KEY` | Direct Anthropic API (default provider) | — |
| `OPENROUTER_API_KEY` | OpenRouter routing (supports all providers) | — |
| `OPENAI_API_KEY` | OpenAI native; also embeddings fallback | — |
| `BASILISK_LLM_PROVIDER` | Override default provider (`anthropic`, `openrouter`, `openai`, `ollama`) | `anthropic` |
| `BASILISK_LLM_MODEL` | Override default model (e.g. `claude-opus-4-7`) | Provider default |

At least one of `ANTHROPIC_API_KEY` or `OPENROUTER_API_KEY` must be set.

## Embedding keys

| Variable | Purpose | Default |
|---|---|---|
| `VOYAGE_API_KEY` | Voyage AI embeddings (primary; voyage-code-3 model) | — |
| `OPENAI_API_KEY` | OpenAI embeddings fallback | — |
| `OLLAMA_HOST` | Local Ollama embeddings endpoint | `http://localhost:11434` |
| `EMBEDDINGS_PROVIDER` | Explicit override (`voyage`, `openai`, `ollama`) | Auto-selected |

The knowledge base requires embeddings. Basilisk auto-selects the first configured provider.

## Blockchain data keys

| Variable | Purpose | Default |
|---|---|---|
| `ALCHEMY_API_KEY` | Multi-chain RPC (primary) | — |
| `ETHERSCAN_API_KEY` | Verified source + creation-tx lookup | — |
| `RPC_URL_ETHEREUM` | Chain-specific RPC override | Via Alchemy or public |
| `RPC_URL_<CHAIN>` | Any chain by uppercase name | Via Alchemy or public |

Supported chain names: `ETHEREUM`, `POLYGON`, `ARBITRUM`, `OPTIMISM`, `BASE`, `AVALANCHE`, `BSC`, `FANTOM`, `GNOSIS`, `CELO`, `MOONBEAM`, `MOONRIVER`, `AURORA`, `CRONOS`, `HARMONY`.

Without `ETHERSCAN_API_KEY`, source resolution falls back to Sourcify → Blockscout.

## GitHub access

| Variable | Purpose | Default |
|---|---|---|
| `GITHUB_TOKEN` | GitHub PAT — raises rate limit from 60 to 5000 req/hr; required for private repos | — |

## Cost and budget

| Variable | Purpose | Default |
|---|---|---|
| `BASILISK_MAX_COST_CENTS` | Hard USD cap per session (in cents) | 500 (recon) / 5000 (vuln) |

CLI flags (`--max-cost`, `--max-turns`, `--max-tokens`) override these per-run.

## Logging

| Variable | Purpose | Default |
|---|---|---|
| `LOG_LEVEL` | Tracing filter directive (`info`, `debug`, `warn`, `error`, `off`) | `info` |
| `RUST_LOG` | Fine-grained filter (e.g. `basilisk_agent=debug,basilisk_llm=trace`) | — |

## Example `.env`

```env
# LLM
ANTHROPIC_API_KEY=sk-ant-api03-...
BASILISK_LLM_MODEL=claude-sonnet-4-6   # cheaper for iteration

# Embeddings
VOYAGE_API_KEY=pa-...

# Blockchain
ALCHEMY_API_KEY=...
ETHERSCAN_API_KEY=...

# GitHub
GITHUB_TOKEN=ghp_...

# Logging
LOG_LEVEL=info
```

## Prompt caching

When using the Anthropic backend (directly or via OpenRouter), Basilisk automatically enables prompt caching (`cache_control: ephemeral`) on the system prompt when `cache_system_prompt = true` is set in a request. This is the default for long tool-use sessions and significantly reduces token costs on repeated turns — the system prompt (often 8–15k tokens) is billed at the cache-read rate (~10× cheaper) after the first call.
