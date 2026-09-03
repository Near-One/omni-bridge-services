//! Replays JetStream messages by re-publishing them to their original subject,
//! optionally filtered to a set of transfers exported from the "Not Finalized
//! Transfers" dashboard.
//!
//! The relayer creates its consumers with `DeliverPolicy::Last` (see
//! `omni-relayer/src/utils/nats.rs`), so a durable consumer that gets recreated
//! starts at the newest message and never delivers what was published while it
//! was gone. The deliver policy is immutable after creation and the streams are
//! `--deny-delete`/`--deny-purge`, so the only way to reprocess is to read the
//! messages back out and publish them again. This only ever appends.
//!
//! The scan uses one ephemeral, no-ack consumer over a single connection, so
//! the whole window streams in on one socket rather than one request per
//! sequence.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use async_nats::jetstream::{
    self,
    consumer::{self, DeliverPolicy},
};
use clap::Parser;
use futures::StreamExt;
use serde_json::Value;
use time::OffsetDateTime;

#[derive(Parser, Debug)]
#[command(
    about = "Replay JetStream messages back onto their original subject",
    long_about = "Examples:\n  \
      nats-replay OMNI_EVENTS --range\n  \
      nats-replay OMNI_EVENTS --csv export.csv --since 300\n  \
      nats-replay OMNI_EVENTS --csv export.csv --since 300 --type UtxoSignTransaction --apply\n\n\
      --since takes MINUTES. Connection comes from NATS_URL / NATS_USER / NATS_PASSWORD, matching the `nats` CLI."
)]
struct Args {
    /// Stream to read from (OMNI_EVENTS or RELAYER).
    stream: String,

    /// Print stream and consumer positions, then exit.
    #[arg(long)]
    range: bool,

    /// Dashboard CSV export; only transfers listed in it are replayed.
    #[arg(long)]
    csv: Option<String>,

    /// Replay messages published within the last N minutes.
    #[arg(long, value_name = "MINUTES")]
    since: Option<u64>,

    /// Replay an explicit stream-sequence window.
    #[arg(long, num_args = 2, value_names = ["START", "END"])]
    seq: Option<Vec<u64>>,

    /// Only replay these event types, e.g. `--type UtxoSignTransaction`.
    /// Repeatable, or comma-separated. Omit to replay every type. The
    /// end-of-run breakdown lists the types present in the window.
    #[arg(long = "type", value_name = "EVENT_TYPE", value_delimiter = ',')]
    event_types: Vec<String>,

    /// Also replay rows whose status is already `Finalised`.
    #[arg(long)]
    include_finalised: bool,

    /// Actually publish. Without this the run is a dry run.
    #[arg(long)]
    apply: bool,

    /// Appended to `Nats-Msg-Id` so replays survive the stream's 1h dupe window.
    #[arg(long, default_value = ":replay")]
    replay_suffix: String,

    /// Bypass the 1h dedup window entirely by adding a unique per-run token to
    /// every replayed `Nats-Msg-Id`. Needed to replay the same messages twice
    /// within an hour — without it the server silently drops the second run.
    #[arg(long)]
    force: bool,

    /// Log a progress line every N messages.
    #[arg(long, default_value_t = 1000)]
    progress_every: u64,

    /// Write the scan index for every message in the window here.
    #[arg(long, default_value = ".nats-replay-cache")]
    cache: String,

    /// Write every filtered (matched) message here, as JSON Lines with payload
    /// and outcome.
    #[arg(long, default_value = "nats-replay-matches.jsonl")]
    matches: String,
}

fn log(msg: impl AsRef<str>) {
    let now = OffsetDateTime::now_utc();
    eprintln!(
        "[{:02}:{:02}:{:02}] {}",
        now.hour(),
        now.minute(),
        now.second(),
        msg.as_ref()
    );
}

/// `{chain}:{nonce-or-utxo}` — the same string the dashboard exports as `_id`:
/// `Near:412132` for `{origin_chain: "Near", kind: {Nonce: 412132}}`, and
/// `Btc:8fa31e...@0` for `{origin_chain: "Btc", kind: {Utxo: "8fa31e...@0"}}`.
fn transfer_id(payload: &Value) -> Option<String> {
    let t = payload
        .get("Transaction")
        .and_then(|tx| tx.get("transfer_id"))
        .or_else(|| payload.get("transfer_id"))?;
    let chain = t.get("origin_chain")?.as_str()?;
    let (_, value) = t.get("kind")?.as_object()?.iter().next()?;
    let value = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    Some(format!("{chain}:{value}"))
}

/// `OmniEvent` flattens `OmniEventData`, which in turn flattens
/// `OmniTransferMessage` / `OmniMetaEventDetails` (see
/// `bridge-indexer-types/src/documents_types.rs`). Both are externally tagged,
/// so the variant name arrives as a JSON key alongside the struct's own fields
/// — the event type is the one key that is not a known field.
fn event_kind(payload: &Value) -> (String, Option<String>) {
    if let Some(tx) = payload.get("Transaction").and_then(Value::as_object) {
        let status = tx.get("status").and_then(Value::as_str).map(str::to_string);
        let kind = tx
            .keys()
            .find(|k| {
                !matches!(
                    k.as_str(),
                    "sender" | "transfer_id" | "status" | "enrichment_data"
                )
            })
            .cloned()
            .unwrap_or_else(|| "Transaction".to_string());
        return (kind, status);
    }
    if let Some(meta) = payload.get("Meta").and_then(Value::as_object) {
        let kind = meta.keys().next().cloned().unwrap_or_default();
        return (format!("Meta/{kind}"), None);
    }
    ("<unknown>".to_string(), None)
}

fn unquote(field: &str) -> &str {
    field.trim().trim_matches('"')
}

/// Reads the dashboard export. Column 0 is `_id`, the last column is `status`.
/// Rows already `Finalised` are dropped unless the caller asks for them: the
/// relayer would only re-check them on chain and drop them as duplicates.
fn load_csv(path: &str, include_finalised: bool) -> Result<(HashSet<String>, usize)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?;
    let mut wanted = HashSet::new();
    let mut skipped = 0usize;

    // `lines()` handles the missing trailing newline these exports ship with.
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        let id = unquote(fields[0]);
        if id == "_id" || id.is_empty() {
            continue;
        }
        let status = unquote(fields[fields.len() - 1]);
        if !include_finalised && status == "Finalised" {
            skipped += 1;
            continue;
        }
        wanted.insert(id.to_string());
    }
    Ok((wanted, skipped))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());
    let mut opts = async_nats::ConnectOptions::new();
    if let (Ok(user), Ok(pass)) = (std::env::var("NATS_USER"), std::env::var("NATS_PASSWORD")) {
        opts = opts.user_and_password(user, pass);
    }

    log(format!("connecting to {url}"));
    let client = opts.connect(&url).await.with_context(|| {
        format!(
            "cannot connect to {url} — is the port-forward up and are NATS_USER/NATS_PASSWORD set?"
        )
    })?;
    let js = jetstream::new(client);

    let mut stream = js
        .get_stream(&args.stream)
        .await
        .with_context(|| format!("cannot open stream '{}'", args.stream))?;
    let info = stream.info().await?.clone();
    let (first_seq, last_seq) = (info.state.first_sequence, info.state.last_sequence);
    log(format!(
        "stream {}: messages={} seq=[{first_seq}..{last_seq}]",
        args.stream, info.state.messages
    ));

    if args.range {
        let consumer_name = match args.stream.as_str() {
            "OMNI_EVENTS" => "omni-relayer",
            "RELAYER" => "relayer-worker",
            _ => "",
        };
        if !consumer_name.is_empty() {
            match stream.consumer_info(consumer_name).await {
                Ok(ci) => log(format!(
                    "consumer {consumer_name}: delivered_seq={} ack_floor={} pending={} ack_pending={} redelivered={}",
                    ci.delivered.stream_sequence,
                    ci.ack_floor.stream_sequence,
                    ci.num_pending,
                    ci.num_ack_pending,
                    ci.num_redelivered
                )),
                Err(err) => {
                    log(format!("consumer '{consumer_name}' unavailable: {err}"));
                    log("if it does not exist the relayer is not attached; starting it recreates the consumer at DeliverPolicy::Last and skips anything published meanwhile, so start it BEFORE replaying");
                }
            }
        }
        return Ok(());
    }

    // Resolve the window. `ByStartTime` lets the server do the seeking, so
    // --since costs nothing extra client-side.
    let (deliver_policy, start_seq, end_seq) = match (&args.seq, args.since) {
        (Some(range), _) => {
            let (start, end) = (range[0], range[1]);
            if start > end {
                bail!("start ({start}) is after end ({end})");
            }
            if start > last_seq || end < first_seq {
                bail!(
                    "window [{start},{end}] lies entirely outside the stream's [{first_seq}..{last_seq}] — run with --range first"
                );
            }
            (
                DeliverPolicy::ByStartSequence {
                    start_sequence: start,
                },
                start,
                end,
            )
        }
        (None, Some(minutes)) => {
            let cutoff = OffsetDateTime::now_utc() - Duration::from_secs(minutes * 60);
            log(format!(
                "window: messages published in the last {minutes} minute(s)"
            ));
            (
                DeliverPolicy::ByStartTime { start_time: cutoff },
                0,
                last_seq,
            )
        }
        (None, None) => bail!("need one of --since MINUTES, --seq START END, or --range"),
    };

    if start_seq > 0 && start_seq < first_seq {
        log(format!(
            "WARNING: start {start_seq} is below first_seq {first_seq}; older messages have aged out (--max-age 14d)"
        ));
    }

    let (wanted, skipped_finalised) = match &args.csv {
        Some(path) => {
            let (w, s) = load_csv(path, args.include_finalised)?;
            if w.is_empty() {
                bail!(
                    "no replayable rows in {path} (all {s} rows were already Finalised; pass --include-finalised to replay them anyway)"
                );
            }
            log(format!(
                "CSV filter: {} transfers to look for, {s} already-Finalised rows skipped",
                w.len()
            ));
            (w, s)
        }
        None => {
            log("no --csv filter: every message in the window will be replayed");
            (HashSet::new(), 0)
        }
    };
    let _ = skipped_finalised;

    let type_filter: HashSet<String> = args.event_types.iter().cloned().collect();
    if type_filter.is_empty() {
        log("no --type filter: every event type in the window is eligible");
    } else {
        let mut names: Vec<&String> = type_filter.iter().collect();
        names.sort();
        log(format!(
            "type filter: only {}",
            names
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    // Unix seconds is enough to be unique per run: two runs cannot start in the
    // same second, and it makes the resulting ids sortable and traceable.
    let run_token = args
        .force
        .then(|| OffsetDateTime::now_utc().unix_timestamp().to_string());
    match run_token.as_deref() {
        Some(token) => log(format!(
            "--force: replay ids get the unique token '.{token}', bypassing the 1h dedup window"
        )),
        None => log("dedup active: messages replayed under an id already used in the last hour will be dropped (use --force to override)"),
    }

    log(if args.apply {
        "mode=APPLY — matches will be republished to their original subjects"
    } else {
        "mode=DRY-RUN — nothing will be published"
    });

    // Ephemeral, no-ack consumer: reading the window must not disturb the
    // relayer's own durable consumer or its ack floor.
    let consumer = stream
        .create_consumer(consumer::pull::Config {
            deliver_policy,
            ack_policy: consumer::AckPolicy::None,
            inactive_threshold: Duration::from_secs(60),
            ..Default::default()
        })
        .await
        .context("failed to create the ephemeral scan consumer")?;

    let mut messages = consumer.messages().await?;
    let mut cache = std::io::BufWriter::new(
        std::fs::File::create(&args.cache)
            .with_context(|| format!("cannot write {}", args.cache))?,
    );
    let mut matches_out = std::io::BufWriter::new(
        std::fs::File::create(&args.matches)
            .with_context(|| format!("cannot write {}", args.matches))?,
    );

    let started = Instant::now();
    let mut seen: HashSet<String> = HashSet::new();
    let mut scanned_kinds: BTreeMap<String, u64> = BTreeMap::new();
    let mut matched_kinds: BTreeMap<String, u64> = BTreeMap::new();
    let (mut scanned, mut matched, mut republished, mut failed) = (0u64, 0u64, 0u64, 0u64);
    let mut deduped = 0u64;

    while let Some(msg) = messages.next().await {
        let msg = msg.context("error reading from the scan consumer")?;
        let seq = msg.info().map(|i| i.stream_sequence).unwrap_or(0);
        if seq > end_seq {
            break;
        }
        scanned += 1;

        if scanned % args.progress_every == 0 {
            let elapsed = started.elapsed().as_secs_f64().max(0.001);
            let rate = scanned as f64 / elapsed;
            log(format!(
                "progress: scanned={scanned} seq={seq} found={}/{} matched={matched} republished={republished} failed={failed} | {rate:.0}/s elapsed={elapsed:.0}s",
                seen.len(),
                wanted.len()
            ));
        }

        let payload: Value = match serde_json::from_slice(&msg.payload) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let tid = transfer_id(&payload);
        let (kind, status) = event_kind(&payload);
        writeln!(
            cache,
            "{seq}\t{}\t{}\t{kind}\t{}",
            msg.subject,
            tid.as_deref().unwrap_or("<none>"),
            status.as_deref().unwrap_or("-")
        )?;
        *scanned_kinds.entry(kind.clone()).or_insert(0u64) += 1;

        if !wanted.is_empty() {
            let Some(tid) = tid.as_ref() else { continue };
            if !wanted.contains(tid) {
                continue;
            }
            // Recorded before the type filter runs: `seen` answers "was this
            // transfer present in the window", which stays true even when the
            // type filter declines to publish this particular event.
            seen.insert(tid.clone());
        }

        if !type_filter.is_empty() && !type_filter.contains(&kind) {
            continue;
        }
        matched += 1;
        *matched_kinds.entry(kind.clone()).or_insert(0u64) += 1;

        // The streams carry --dupe-window 1h, so re-publishing under the
        // original Nats-Msg-Id inside that window is silently dropped by the
        // server. Tagging the id forces the replay through; duplicates are safe
        // because the handlers re-check finalisation on chain.
        // Must be looked up through the typed constant: the server sends this as
        // `HeaderName::Standard(NatsMessageId)`, and a `&str` lookup builds a
        // `Custom("Nats-Msg-Id")` that never compares equal, silently yielding
        // None and publishing the replay with no dedup id at all.
        let original_msg_id = msg
            .headers
            .as_ref()
            .and_then(|h| h.get(async_nats::header::NATS_MESSAGE_ID))
            .map(|v| v.as_str().to_string());
        let replay_msg_id = original_msg_id
            .as_ref()
            .map(|orig| match run_token.as_deref() {
                // --force: the per-run token makes every id unique, so the dedup
                // window has nothing to match and the message always lands.
                Some(token) => format!("{orig}{}.{token}", args.replay_suffix),
                None => format!("{orig}{}", args.replay_suffix),
            });
        let mut headers = async_nats::HeaderMap::new();
        if let Some(id) = replay_msg_id.as_deref() {
            headers.insert(async_nats::header::NATS_MESSAGE_ID, id);
        }

        // One JSON line per filtered message, written whether or not it is
        // published, so a dry run produces the exact manifest that --apply will
        // act on and an apply run records what actually landed.
        let mut record = serde_json::json!({
            "seq": seq,
            "subject": msg.subject.to_string(),
            "transfer_id": tid,
            "event_type": kind,
            "status": status,
            "original_msg_id": original_msg_id,
            "replay_msg_id": replay_msg_id,
            "published_at": msg.info().ok().map(|i| i.published.to_string()),
            "payload": payload,
        });

        if !args.apply {
            log(format!(
                "MATCH [{matched}] seq={seq} id={} type={kind} status={} subject={} transfers={}/{} (dry run)",
                tid.as_deref().unwrap_or("<none>"),
                status.as_deref().unwrap_or("-"),
                msg.subject,
                seen.len(),
                wanted.len()
            ));
            record["outcome"] = Value::String("dry-run".into());
            writeln!(matches_out, "{record}")?;
            continue;
        }

        match js
            .publish_with_headers(msg.subject.to_string(), headers, msg.payload.clone())
            .await
        {
            Ok(ack) => match ack.await {
                // A dedup drop is still a *successful* ack: the server reports
                // `duplicate: true` and assigns no new sequence. Counting it as
                // published would over-report the replay, so it gets its own
                // outcome and its own counter.
                Ok(ack) if ack.duplicate => {
                    deduped += 1;
                    log(format!(
                        "DEDUPED [{deduped}] seq={seq} id={} type={kind} — dropped by the 1h dupe window; re-run with --force to push it through",
                        tid.as_deref().unwrap_or("<none>"),
                    ));
                    record["outcome"] = Value::String("deduplicated".into());
                }
                Ok(ack) => {
                    republished += 1;
                    log(format!(
                        "PUBLISHED [{republished}] seq={seq} id={} type={kind} status={} -> {} new_seq={} transfers={}/{}",
                        tid.as_deref().unwrap_or("<none>"),
                        status.as_deref().unwrap_or("-"),
                        msg.subject,
                        ack.sequence,
                        seen.len(),
                        wanted.len()
                    ));
                    record["outcome"] = Value::String("published".into());
                    record["new_seq"] = Value::from(ack.sequence);
                }
                Err(err) => {
                    failed += 1;
                    log(format!("seq={seq} publish ack failed: {err}"));
                    record["outcome"] = Value::String(format!("ack-failed: {err}"));
                }
            },
            Err(err) => {
                failed += 1;
                log(format!("seq={seq} publish failed: {err}"));
                record["outcome"] = Value::String(format!("publish-failed: {err}"));
            }
        }
        writeln!(matches_out, "{record}")?;
    }

    cache.flush()?;
    matches_out.flush()?;
    let elapsed = started.elapsed().as_secs_f64();
    log(format!(
        "done: stream={} scanned={scanned} matched={matched} republished={republished} deduped={deduped} failed={failed} in {elapsed:.1}s ({:.0}/s) mode={}",
        args.stream,
        scanned as f64 / elapsed.max(0.001),
        if args.apply { "apply" } else { "dry-run" }
    ));
    if deduped > 0 {
        log(format!(
            "WARNING: {deduped} message(s) were dropped by the 1h dedup window and did NOT reach the stream — re-run with --force to push them through"
        ));
    }
    log(format!(
        "scan index written to {} (seq, subject, transfer_id, event_type, status)",
        args.cache
    ));
    log(format!(
        "{matched} filtered messages written to {} (JSON Lines, with payload and outcome)",
        args.matches
    ));

    // Which event types the window held, and which of them the filter selected.
    // A type that appears under `scanned` but never under `matched` is the usual
    // sign the CSV filter is narrower than expected.
    log("event types seen in window (scanned -> matched):");
    for (kind, count) in &scanned_kinds {
        log(format!(
            "  {kind}: {count} scanned -> {} matched",
            matched_kinds.get(kind).copied().unwrap_or(0)
        ));
    }

    // Transfers with no message in the window are the important negative
    // result: either older than --since, or already aged out of the stream.
    if !wanted.is_empty() {
        let missing: Vec<&String> = wanted.difference(&seen).collect();
        if missing.is_empty() {
            log(format!("all {} CSV transfers were found", wanted.len()));
        } else {
            let path = format!("{}.notfound", args.cache);
            let body: String = missing
                .iter()
                .map(|id| format!("{id}\n"))
                .collect::<Vec<_>>()
                .concat();
            std::fs::write(&path, body)?;
            log(format!(
                "WARNING: {} of {} CSV transfers had no message in this window — see {path}. Widen --since, or they have aged out",
                missing.len(),
                wanted.len()
            ));
        }
    }

    if republished == 0 && args.apply {
        log("nothing was republished — check the CSV filter and the window");
        std::process::exit(1);
    }
    Ok(())
}
