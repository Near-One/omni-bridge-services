# Omni Relayer

Off-chain component of [Omni Bridge](https://github.com/near-one/omni-bridge) that relays transfers between NEAR and other networks (Ethereum, Solana, BTC, and more).

## Deployment

Docker Compose is the recommended way to deploy the relayer. It bundles the relayer with NATS (message queue) and Redis (state store), and automatically creates the required JetStream streams.

### 1. Configure secrets

```bash
cp .example-env .env
```

Edit `.env` and fill in your credentials:

| Variable | Required | Description |
|----------|----------|-------------|
| `NEAR_OMNI_ACCOUNT_ID` / `NEAR_OMNI_PRIVATE_KEY` | Yes | NEAR account for signing relay transactions |
| `ETH_PRIVATE_KEY`, `BASE_PRIVATE_KEY`, ... | Per chain | EVM chain private keys (only for chains you enable) |
| `SOLANA_PRIVATE_KEY` | If Solana enabled | Solana keypair (bs58-encoded) |
| `INFURA_API_KEY` | If using Infura | API key for EVM RPC endpoints |
| `MONGODB_USERNAME` / `MONGODB_PASSWORD` / `MONGODB_HOST` | If using bridge indexer | Bridge indexer database credentials |
| `BRIDGE_NATS_USERNAME` / `BRIDGE_NATS_PASSWORD` | Yes | NATS authentication credentials |

### 2. Configure the relayer

```bash
cp example-docker-config.toml config.toml
```

Edit `config.toml`:
- **Enable only the chains you want to relay** — comment out or remove sections for chains you don't support
- **Set your RPC endpoints** — replace Infura URLs with your own providers if preferred
- **Adjust fee settings** — `fee_discount` in `[bridge_indexer]` controls how much discount you accept (0-100)
- **Token whitelist** (optional) — restrict to specific tokens via `whitelisted_tokens`

### 3. Set NATS credentials

The included `nats.conf` reads credentials from environment variables passed by docker-compose. Set `BRIDGE_NATS_USERNAME` and `BRIDGE_NATS_PASSWORD` in your `.env` file — these are used for both the NATS server and the relayer connection.

### 4. Deploy

```bash
docker compose up -d
```

This starts:
- **NATS** — JetStream message queue
- **nats-init** — creates `OMNI_EVENTS` and `RELAYER` streams (runs once, idempotent)
- **Redis** — state and checkpoint storage
- **relayer** — the omni-relayer process

All services are configured with `restart: unless-stopped` for automatic recovery.

### Verify

```bash
# Check all services are running
docker compose ps

# View relayer logs
docker compose logs -f relayer

# Check NATS streams were created
docker compose logs nats-init
```

### Update

```bash
git pull
docker compose up -d --build
```

## Configuration Reference

The relayer is configured through a TOML file with environment variable substitution at parse time (variables like `INFURA_API_KEY` in RPC URLs are replaced automatically from the environment).

Example configs:
- `example-docker-config.toml` — docker-compose deployment (recommended starting point)
- `example-devnet-config.toml` — devnet/testnet with all chains
- `example-testnet-config.toml` — testnet
- `example-mainnet-config.toml` — mainnet

| Section | Purpose |
|---------|---------|
| `[redis]` | Connection URL and retry settings |
| `[nats]` | Connection URL, consumer names, retry backoff, worker count |
| `[bridge_indexer]` | Bridge API URL, MongoDB, fee discount, token whitelist |
| `[near]` | NEAR RPC, bridge contract IDs, signer credentials |
| `[eth]`, `[base]`, `[arb]`, `[bnb]`, `[pol]` | Per-chain RPC URLs, bridge addresses, finalization settings |
| `[solana]` | Solana RPC, program IDs, discriminators |
| `[btc]`, `[zcash]` | UTXO chain RPC and light client settings |
| `[eth.fee_bumping]` | EVM transaction fee bumping thresholds |

## Architecture

```
Indexers (NEAR / EVM / Solana / MongoDB)
  └─► NATS OMNI_EVENTS stream
        └─► Bridge Indexer consumer
              └─► NATS RELAYER stream
                    └─► Worker pool (process_events)
                          └─► OmniConnector SDK ─► destination chain
```

1. **Indexers** watch source chains for bridge events
2. Events flow through NATS JetStream with at-least-once delivery
3. **Workers** validate fees, build proofs, and finalize transfers via the OmniConnector SDK
4. **Fee bumping** monitors pending EVM transactions and resubmits with higher gas when stuck

## Triggering Transfers via NATS

You can manually trigger the relayer to process a transfer by publishing a JSON message to the **RELAYER** JetStream stream. The subject format is `relayer.tasks.{chain}`, where `{chain}` is the chain where the relayer needs to perform the action, in lowercase (e.g., `eth`, `near`, `sol`).

The `Nats-Msg-Id` header is used for deduplication — use a unique key per event (typically `{tx_hash}:{nonce}`).

### InitTransfer (NEAR → EVM)

When a user initiates a transfer on NEAR destined for another chain, the relayer needs to call `near_sign_transfer` on NEAR. Publish to `relayer.tasks.near`:

```bash
nats pub relayer.tasks.near \
  --header "Nats-Msg-Id:9uFz3YQBAA7S4uxRCbM2VSaQPLYTZGN3MJRBv4gKLMpQ:18069" \
  '{
    "init_transfer": "Near",
    "transfer_message": {
      "origin_nonce": 18069,
      "token": "near:wrap.testnet",
      "amount": "2061009676062263083008",
      "recipient": "eth:0x5a08feed678c056650b3eb4a5cb1b9bb6f0fe265",
      "fee": {
        "fee": "10000000000000000",
        "native_fee": "0"
      },
      "sender": "near:dev-giraffe.testnet",
      "msg": "",
      "destination_nonce": 2133
    }
  }'
```

| Field | Description |
|-------|-------------|
| `init_transfer` | Always `"Near"` for transfers initiated on NEAR |
| `transfer_message.origin_nonce` | Transfer nonce from the NEAR bridge contract |
| `transfer_message.token` | Token address, prefixed with chain (e.g., `near:wrap.testnet`) |
| `transfer_message.amount` | Transfer amount (as string) |
| `transfer_message.recipient` | Destination address, prefixed with chain (e.g., `eth:0x...`) |
| `transfer_message.fee` | Fee object with `fee` and `native_fee` (as strings) |
| `transfer_message.sender` | Sender address on NEAR, prefixed with `near:` |
| `transfer_message.msg` | Optional message attached to the transfer |
| `transfer_message.destination_nonce` | Nonce on the destination chain |

### SignTransfer (NEAR → EVM)

When a user initiates a transfer on NEAR destined for an EVM chain, the NEAR bridge contract emits a `SignTransferEvent`. Publish to `relayer.tasks.{destination_chain}`:

```bash
nats pub relayer.tasks.eth \
  --header "Nats-Msg-Id:CdBZmmWsiHmftnwuJp777dMT7M1CNs64QcCRvkHoNXWj:18069" \
  '{
    "SignTransferEvent": {
      "signature": {
        "big_r": {
          "affine_point": "02445067dc7cb50f6d03a5e69f36651d0787a26d6a4d0cb62f1d3012a25e794743"
        },
        "s": {
          "scalar": "7e4ae7106c9462a74d99a2b618fb73b342ab7adde19aec07638830b19eb8197e"
        },
        "recovery_id": 0
      },
      "message_payload": {
        "prefix": "TransferMessage",
        "destination_nonce": 2133,
        "transfer_id": {
          "origin_chain": "Near",
          "origin_nonce": 18069
        },
        "token_address": "eth:0x1f89e263159f541182f875ac05d773657d24eb92",
        "amount": "2061009676062263083008",
        "recipient": "eth:0x5a08feed678c056650b3eb4a5cb1b9bb6f0fe265",
        "fee_recipient": "omni-relayer-testnet.testnet",
        "message": []
      }
    }
  }'
```

| Field | Description |
|-------|-------------|
| `signature` | MPC signature from the NEAR bridge contract (`big_r`, `s`, `recovery_id`) |
| `message_payload.destination_nonce` | Nonce on the destination chain |
| `message_payload.transfer_id` | Origin chain and nonce identifying the transfer |
| `message_payload.token_address` | Token address, prefixed with destination chain (e.g., `eth:0x...`) |
| `message_payload.amount` | Transfer amount (as string) |
| `message_payload.recipient` | Destination address, prefixed with chain |
| `message_payload.fee_recipient` | NEAR account of the relayer that should collect the fee |

### Notes

- Messages are processed with at-least-once delivery — duplicates are safe thanks to `Nats-Msg-Id` deduplication and on-chain finalization checks.
- The relayer validates fees against the bridge API before processing. If the fee is insufficient, the event will be retried or dropped.
- If NATS authentication is enabled, pass credentials: `nats pub --user $BRIDGE_NATS_USERNAME --password $BRIDGE_NATS_PASSWORD ...`

## Building from Source

If you prefer to run without Docker:

```bash
# Prerequisites: Rust 1.86+, running Redis, running NATS with JetStream

# Create JetStream streams (one-time setup)
nats stream add OMNI_EVENTS --subjects "omni-events.>" --retention limits --storage file --discard old
nats stream add RELAYER --subjects "relayer.tasks.>" --retention limits --storage file --discard old

# Build and run
cp .example-env .env
cp example-devnet-config.toml config.toml
# Edit .env and config.toml

cargo run -- --config config.toml
```
