# nats-replay

Reads messages back out of a NATS JetStream stream and publishes them again, so
the relayer processes them a second time. Useful when transfers are stuck or the
relayer's consumer skipped a range.

It only ever **adds** messages. It never deletes, purges, or touches the
relayer's own consumer.

## Setup

```bash
kubectl port-forward -n nats-mainnet svc/nats-mainnet 4222:4222

export NATS_URL="nats://localhost:4222"
export NATS_USER="$BRIDGE_NATS_USERNAME"
export NATS_PASSWORD="$BRIDGE_NATS_PASSWORD"

cargo build --release -p nats-replay
```

The binary lands at `target/release/nats-replay`.

## Examples

Check the stream and where the relayer's consumer is:

```bash
nats-replay OMNI_EVENTS --range
```

See what would be replayed from the last 30 minutes (this is a dry run — it
publishes nothing):

```bash
nats-replay OMNI_EVENTS --since 30
```

Replay only the transfers listed in a dashboard CSV export:

```bash
nats-replay OMNI_EVENTS --since 300 --csv unfinalised.csv
```

Replay only one kind of event:

```bash
nats-replay OMNI_EVENTS --since 300 --type UtxoSignTransaction
```

Happy with the dry run? Add `--apply` to actually publish:

```bash
nats-replay OMNI_EVENTS --since 300 --csv unfinalised.csv --apply
```

Need to replay the same messages again within the hour? Add `--force`:

```bash
nats-replay OMNI_EVENTS --since 300 --csv unfinalised.csv --apply --force
```

## Options

| Flag | Meaning |
| --- | --- |
| `--range` | Show stream and consumer positions, then exit |
| `--since N` | Messages published in the last N **minutes** |
| `--seq START END` | An explicit stream-sequence window instead of `--since` |
| `--csv FILE` | Only replay transfers listed in a dashboard export |
| `--type NAME` | Only replay these event types (repeatable or comma-separated) |
| `--apply` | Actually publish. Without it, nothing is sent |
| `--force` | Bypass the 1 hour dedup window |
| `--include-finalised` | Also replay CSV rows already marked `Finalised` |

## Output files

- `nats-replay-matches.jsonl` — one line per selected message, with its payload
  and what happened to it (`dry-run`, `published`, `deduplicated`, or an error).
  A dry run gives you the exact list `--apply` will act on.
- `.nats-replay-cache` — every message seen in the window, for reference.
- `.nats-replay-cache.notfound` — CSV transfers with no message in the window.
  Usually means you need a bigger `--since`.

## Good to know

- **Always dry run first.** Leave `--apply` off, read the output, then re-run
  with it.
- **CSV exports are not all unfinalised.** A "Not Finalized Transfers" export
  usually contains plenty of rows already marked `Finalised`. Those are skipped
  by default, and the log tells you how many.
- **One transfer is several messages.** A transfer emits an event per lifecycle
  step, so 100 transfers can mean 250+ messages. The log shows both numbers.
- **Duplicates are safe.** The relayer re-checks finalisation on chain, so a
  transfer that already went through is just dropped.
- **Re-running within an hour needs `--force`.** Otherwise the server silently
  discards the second run. Without `--force` the tool reports these as
  `deduped` rather than pretending they were published.
- **The end of every run lists the event types it saw**, which is how you find
  the right name for `--type`.
