//! Redis-backed VAA store + txHash-resolution cache.
//!
//! Keys:
//!   * `vaa:{wh_chain_id}:{emitter_hex32}:{sequence}` → raw VAA bytes (TTL ≥14d)
//!   * `txres:{normalized_hash}`                       → JSON resolution (TTL ~1d)
//!
//! A redeploy must not drop recent VAAs, so the store is Redis (not in-memory) and
//! VAAs carry a long TTL. The resolution cache lets repeated relayer polls skip the
//! chain-RPC probe; the VAA itself is always looked up fresh so a pre-quorum miss
//! (404 now) succeeds once the spy delivers it.

use std::time::Duration;

use redis::AsyncCommands;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};

use crate::address::emitter_hex;
use crate::vaa::VaaId;

/// Per-command timeout (mirrors omni-relayer's `set_response_timeout`) so a Redis stall
/// surfaces as an error rather than hanging an HTTP handler.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard cap on the *initial* connection so an unreachable Redis fails fast with a clear
/// error instead of retrying forever (`ConnectionManager` retries indefinitely by default).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct Store {
    conn: ConnectionManager,
    vaa_ttl_secs: u64,
    txres_ttl_secs: u64,
}

impl Store {
    pub async fn connect(
        redis_url: &str,
        vaa_ttl_secs: u64,
        txres_ttl_secs: u64,
    ) -> redis::RedisResult<Self> {
        let client = redis::Client::open(redis_url)?;
        // Auto-reconnecting manager (same as omni-relayer); `set_response_timeout` bounds
        // each command. The outer timeout bounds only the initial connect — once built,
        // the manager transparently reconnects on its own for runtime blips.
        let config = ConnectionManagerConfig::new().set_response_timeout(RESPONSE_TIMEOUT);
        let conn = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            ConnectionManager::new_with_config(client, config),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                return Err(redis::RedisError::from((
                    redis::ErrorKind::IoError,
                    "timed out establishing the initial Redis connection",
                )));
            }
        };
        Ok(Self {
            conn,
            vaa_ttl_secs,
            txres_ttl_secs,
        })
    }

    fn vaa_key(chain: u16, emitter: &str, sequence: u64) -> String {
        format!("vaa:{chain}:{emitter}:{sequence}")
    }

    fn txres_key(hash: &str) -> String {
        format!("txres:{hash}")
    }

    /// Store raw VAA bytes keyed by its identifying triple, idempotently (`SET … EX`).
    pub async fn put_vaa(&self, id: &VaaId, raw: &[u8]) -> redis::RedisResult<()> {
        let key = Self::vaa_key(
            id.emitter_chain,
            &emitter_hex(&id.emitter_address),
            id.sequence,
        );
        let mut conn = self.conn.clone();
        conn.set_ex::<_, _, ()>(key, raw, self.vaa_ttl_secs).await
    }

    /// Fetch raw VAA bytes by `(chain, emitter, sequence)`; `None` if absent.
    pub async fn get_vaa(
        &self,
        chain: u16,
        emitter: &[u8; 32],
        sequence: u64,
    ) -> redis::RedisResult<Option<Vec<u8>>> {
        let key = Self::vaa_key(chain, &emitter_hex(emitter), sequence);
        let mut conn = self.conn.clone();
        conn.get(key).await
    }

    /// Cached txHash → resolution JSON, `None` if not cached.
    pub async fn get_txres(&self, hash: &str) -> redis::RedisResult<Option<String>> {
        let mut conn = self.conn.clone();
        conn.get(Self::txres_key(hash)).await
    }

    /// Cache a txHash → resolution JSON with the resolution TTL.
    pub async fn put_txres(&self, hash: &str, json: &str) -> redis::RedisResult<()> {
        let mut conn = self.conn.clone();
        conn.set_ex::<_, _, ()>(Self::txres_key(hash), json, self.txres_ttl_secs)
            .await
    }

    /// Liveness probe for `/healthz`.
    pub async fn ping(&self) -> redis::RedisResult<()> {
        let mut conn = self.conn.clone();
        redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vaa_key_format() {
        let emitter = [0u8; 32];
        let hex = emitter_hex(&emitter);
        assert_eq!(Store::vaa_key(1, &hex, 42597), format!("vaa:1:{hex}:42597"));
    }

    #[test]
    fn txres_key_format() {
        assert_eq!(Store::txres_key("0xabc"), "txres:0xabc");
    }
}
