# Omni Bridge Services

Off-chain services for [Omni Bridge](https://github.com/near-one/omni-bridge) — a cross-chain bridge connecting NEAR with Ethereum, Solana, Bitcoin, and more.

## Crates

| Crate | Description |
|-------|-------------|
| [omni-relayer](omni-relayer/) | Relayer that watches source chains for bridge events and finalizes transfers on destination chains |
| [bridge-indexer-types](bridge-indexer-types/) | Shared type definitions for the bridge indexer API |

## Development

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
```
