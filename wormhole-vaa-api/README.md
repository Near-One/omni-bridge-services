# wormhole-vaa-api

A self-hosted, [wormholescan](https://wormholescan.io)-compatible VAA service. It is a
drop-in primary for the parts of the wormholescan REST API that the Omni Bridge relayer
(`Near-One/bridge-sdk-rs` → `wormhole-bridge-client`) depends on, so the bridge keeps
working when `api.wormholescan.io` is down or rate-limiting.

It is deployed behind the in-cluster `omni-proxy` as the **primary** upstream of a
`/wormhole-api` route, with `api.wormholescan.io` as the **fallback**. This service does
**not** implement the fallback — the proxy fails over on our `404`/`5xx`.

## How it works

```
relayer ─▶ omni-proxy /wormhole-api ─▶ [primary: wormhole-vaa-api] [fallback: api.wormholescan.io]

wormhole-vaa-api:
  • Spy gRPC subscriber (filtered to our emitters) → VAA store keyed by (chain, emitter, sequence)
  • GET /api/v1/vaas/{chain}/{emitter}/{sequence}   → store lookup
  • GET /api/v1/vaas/?txHash=<h>                     → resolve (chain,emitter,seq) from the
                                                        source tx via omni-proxy, then store lookup
```

* **Spy subscriber** opens `SubscribeSignedVAA` to a Wormhole `guardiand spy`, filtered
  by `EmitterFilter`s for our configured `(chain, emitter)` pairs, and stores each raw
  VAA in Redis under `vaa:{wh_chain_id}:{emitter_hex32}:{sequence}` (TTL ≥14 days). The
  spy has no backfill: a VAA missed while disconnected is served by the proxy fallback.
* **REST API** (axum) serves the two endpoints verbatim (the proxy strips its
  `/wormhole-api` prefix), plus `/healthz`.
* **txHash resolver** turns the relayer's `txHash` into `(chain, emitter, sequence)`:
  EVM hashes are resolved by probing the configured EVM chains' `eth_getTransactionReceipt`
  in parallel (via omni-proxy) and parsing the `LogMessagePublished` logs from each
  chain's Wormhole core; Solana/Fogo signatures via `getTransaction` (emitter is the
  configured bridge `config` PDA, sequence parsed from the `Sequence:` log). Resolutions
  are cached in `txres:{hash}`.

## Status-code contract (drives proxy fallback)

| Situation | Status | Body |
|---|---|---|
| VAA found | `200` | `{"data":{"vaa":"<b64>"}}` (endpoint 1) / `{"data":[{"vaa":"<b64>"}]}` (endpoint 2) |
| Not in store / unresolved txHash | `404` | `{"code":5,"message":"NOT FOUND"}` (1) / `{"data":[]}` (2) |
| Internal error (Redis / all RPC probes failed) | `5xx` | `{"code":13,"message":"INTERNAL"}` |

`200` is **only** ever returned with a real, fully-signed VAA. `vaa` is standard padded
base64, identical to wormholescan. The infra `/wormhole-api` route must fail over on
`404`, `429`, and `5xx`.

## Running

```bash
wormhole-vaa-api --network mainnet --config /config/config.toml --port 8080
```

| Flag | Default | Meaning |
|---|---|---|
| `--network` | (required) | `mainnet` or `testnet` — used for log/metric labels |
| `--config` | (required) | path to the TOML config |
| `--port` | `8080` | HTTP listen port (binds `0.0.0.0`) |

### Config

See [`example-mainnet-config.toml`](example-mainnet-config.toml) /
[`example-testnet-config.toml`](example-testnet-config.toml). `${ENV}` references are
expanded at load time. Top-level keys: `spy_addr`, `redis_url`, `proxy_base_url`,
`vaa_ttl_secs` (default 15d), `txres_ttl_secs` (default 1d), and a `[[chains]]` table of
`{ name, family (evm|svm), wh_chain_id, emitter, core (EVM only), program_id (SVM only,
base58 bridge program — scopes Solana sequence attribution), proxy_prefix }`.

### Environment

No secrets are required (all dependencies are in-cluster and unauthenticated). Optional
observability vars match omni-proxy — see [`.example-env`](.example-env): `GRAFANA_LOKI_*`
/ `GRAFANA_OTLP_*` / `GRAFANA_CLOUD_API_KEY` (logging→Loki, metrics→OTLP push),
`CLUSTER_NAME`, `POD_NAME`, `RUST_LOG`. There is **no** Prometheus scrape endpoint;
metrics are pushed via OTLP.

### Endpoints

| Path | Purpose |
|---|---|
| `GET /api/v1/vaas/{chain}/{emitter}/{sequence}` | VAA by identity (store lookup) |
| `GET /api/v1/vaas/?txHash=<hash>` | VAA by source tx hash (resolve + lookup) |
| `GET /healthz` | `200` iff the spy stream is connected **and** Redis is reachable |

## Build / test

```bash
cargo build -p wormhole-vaa-api      # build script uses vendored protoc (no system protoc)
cargo test  -p wormhole-vaa-api      # unit + SDK round-trip tests (no network/Redis needed)
```

## Manual end-to-end check

CI covers unit + SDK-round-trip tests; the live path (spy → store → resolve) is verified
by hand against a deployed instance, e.g.:

```bash
BASE=http://localhost:8080   # or the in-cluster service

# 1) Health: spy connected + Redis reachable.
curl -fsS "$BASE/healthz"

# 2) Endpoint (1): a (chain, emitter, sequence) the spy has captured → 200 with a VAA.
#    (testnet Solana emitter shown; compare against api.testnet.wormholescan.io)
curl -fsS "$BASE/api/v1/vaas/1/68113dacfe11fefbe08cb8e61cbde3f336aaff607a070a9052cc8da397995d78/14182" | jq .data.vaa

# 3) Endpoint (2): a real recent bridge txHash per supported chain → 200 with a VAA.
#    EVM: 0x-hash; Solana/Fogo: base58 signature.
curl -fsS "$BASE/api/v1/vaas/?txHash=<tx>" | jq '.data[0].vaa'

# 4) Missing → 404 (proxy then falls over to wormholescan).
curl -s -o /dev/null -w '%{http_code}\n' "$BASE/api/v1/vaas/?txHash=0x$(printf '0%.0s' {1..64})"
```

Confirm the returned `vaa` (after base64-decode) equals what `api.wormholescan.io`
returns for the same identity/tx.

## Image

Built by CI to
`europe-west4-docker.pkg.dev/bridge-misc/omni-bridge-docker-images/wormhole-vaa-api`
(infra pins by `@sha256:`). `EXPOSE 8080`, runs as non-root.

## Non-goals

No UI/analytics/history, no chains beyond the config table, no Ethereum txHashes (ETH
uses a light client, not Wormhole), no in-service fallback, no auth, no backfill.
