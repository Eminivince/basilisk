# On-Chain Analysis

The on-chain layer resolves deployed contracts into a complete `ContractGraph` — following proxies, diamonds, and upgrade histories — before the agent begins reasoning.

## Resolution pipeline

```text
address + chain
  → fetch bytecode (RPC)
  → detect proxy type
  → fetch verified source (Sourcify → Etherscan → Blockscout)
  → recurse into implementation / facets
  → build ContractGraph with typed edges
```

`resolve_system()` does this recursively with a configurable depth limit and cycle detection.

## Proxy detection

| Standard | Detection method |
|---|---|
| EIP-1967 Transparent | Storage slot `0x360894...` for implementation |
| EIP-1967 UUPS | Same slot, upgrade logic in implementation |
| EIP-1967 Beacon | Slot `0xa3f0ad74...` for beacon, then `implementation()` call |
| EIP-1167 Minimal proxy | Bytecode pattern match (`363d3d37...`) |
| EIP-2535 Diamond | `facets()` call to `DiamondLoupeFacet` |
| Historical implementations | Constructor event logs + creation-tx analysis |

## ContractGraph

The graph uses typed edges to capture relationships:

| Edge kind | Meaning |
|---|---|
| `ProxiesTo` | This contract delegates to an implementation |
| `FacetOf` | This facet belongs to a diamond proxy |
| `HistoricalImplementation` | Previous implementation (pre-upgrade) |
| `StorageRef` | Address stored in a slot (not a proxy, but referenced) |
| `BytecodeRef` | Address hardcoded in bytecode |
| `ImmutableRef` | Address set via immutable variable at deploy time |

The graph can be exported as DOT format for visualisation.

## Source resolution

Source resolution uses a waterfall:

1. **Sourcify** — fully open, no API key needed; best coverage for recent contracts
2. **Etherscan V2** — requires `ETHERSCAN_API_KEY`; covers the most chains
3. **Blockscout** — open-source explorer; good fallback for L2s

Returns a `VerifiedSource` struct with Solidity file contents, compiler version, optimizer settings, and constructor arguments.

## RPC provider

All RPC calls go through `AlloyProvider`, which wraps alloy's HTTP transport with:

- **Retry logic** — exponential backoff on 429/503
- **Bytecode caching** — raw bytecode is cached to `~/.cache/basilisk/rpc/` with a long TTL (bytecode is immutable)
- **Multi-chain URL resolution** — Alchemy multi-chain → `RPC_URL_<CHAIN>` → public endpoint

Supported chains and their canonical names:

```text
ethereum  polygon   arbitrum  optimism  base
avalanche bsc       fantom    gnosis    celo
moonbeam  moonriver aurora    cronos    harmony
```

Add any chain via `RPC_URL_<CHAIN>=https://...` in `.env`.

## Caching

Explorer and RPC responses are cached under `~/.cache/basilisk/` with per-namespace TTLs:

| Namespace | TTL | Contents |
|---|---|---|
| `rpc/bytecode` | indefinite | Raw bytecode (immutable) |
| `rpc/storage` | 1 hour | Storage slot reads |
| `sourcify` | 24 hours | Verified source responses |
| `etherscan` | 24 hours | Verified source + ABI |
| `blockscout` | 24 hours | Verified source |
| `github` | 1 hour | Ref resolution + branch info |

Writes are atomic (tempfile + rename). Expired entries are lazily evicted on next access.

## Static calls

The `static_call` tool wraps `eth_call` — read-only on-chain calls. Used by the agent to:
- Read oracle prices
- Check balances and allowances
- Inspect state before/after a simulated exploit
- Call view functions not available in source

## Storage inspection

`get_storage_slot` reads raw storage values. Used for:
- Proxy implementation slot reads (EIP-1967)
- Mapping slot preimage verification
- Reading packed storage layout values
