# Light-client relayers

Relayers that feed block data to the on-chain light clients.

| Directory | Binary | Images | Source repo |
| --- | --- | --- | --- |
| [`eth2-relayer`](eth2-relayer) | `relayer` | `eth-relayer` | `rainbow-bridge/relayer` |
| [`btc-relayer`](btc-relayer) | `btc-relayer` | `btc-relayer`, `ltc-relayer`, `zec-relayer` | `btc-light-client-contract/relayer` |

Both are **separate cargo workspaces**, excluded from the root workspace — their
dependency trees cannot share the root lockfile:

- `btc-relayer`: `btc-types` pins `near-sdk =5.29.0`, `omni-types` pins `=5.28.1`.
- `eth2-relayer`: lighthouse v7.1.0 needs `c-kzg 1.x` (a `links = "ckzg"` crate) while
  omni-relayer's alloy 1.1 needs `c-kzg 2.x`.

So build and test them from their own directory (`cd light-clients/<crate> && cargo test`);
CI does the same in the `light-clients` matrix job of `.github/workflows/rust.yaml`.

The contract crates they depend on (`eth-types`/`eth2-utility`, `btc-types`/`merkle-tools`)
stay in their original repos and are pinned as git dependencies.
