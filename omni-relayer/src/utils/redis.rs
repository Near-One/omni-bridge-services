use anyhow::{Context, Result};
use redis::{AsyncCommands, aio::ConnectionManager};
use tracing::warn;

use crate::config;

use super::bridge_api::TransferFee;

pub const EVENTS: &str = "events";

pub const FEE_MAPPING: &str = "fee_mapping";

pub fn composite_key(parts: &[&str]) -> String {
    parts.join(":")
}

/// Shared handle around the Redis connection manager.
///
/// When Redis is not configured, the inner `Option` is `None` and every
/// operation becomes a no-op (reads return `None`, writes log a warning).
/// Code paths that must have Redis (native indexers, mongo-ingestion, fee
/// bumping) should validate once at startup via [`RelayerStore::require`].
#[derive(Clone, Default)]
pub struct RelayerStore(Option<ConnectionManager>);

impl RelayerStore {
    pub fn new(manager: Option<ConnectionManager>) -> Self {
        Self(manager)
    }

    pub fn require(self, feature: &str) -> Result<Self> {
        self.0
            .as_ref()
            .with_context(|| format!("Redis is required for {feature}"))?;
        Ok(self)
    }

    fn conn(&self) -> Option<ConnectionManager> {
        self.0.clone()
    }

    pub async fn get_fee(&self, config: &config::Config, transfer_id: &str) -> Option<TransferFee> {
        let mut conn = self.conn()?;
        let redis_config = config.redis_config();

        for _ in 0..redis_config.query_retry_attempts {
            match conn
                .hget::<&str, &str, Option<String>>(FEE_MAPPING, transfer_id)
                .await
            {
                Ok(Some(serialized)) => match serde_json::from_str(&serialized) {
                    Ok(fee) => return Some(fee),
                    Err(err) => {
                        warn!("Failed to deserialize Fee for transfer_id {transfer_id}: {err:?}");
                        return None;
                    }
                },
                Ok(None) => {
                    return None;
                }
                Err(_) => {
                    tokio::time::sleep(tokio::time::Duration::from_secs(
                        redis_config.query_retry_sleep_secs,
                    ))
                    .await;
                }
            }
        }

        warn!(
            "Failed to get fee for transfer_id {transfer_id} from redis after {} attempts",
            redis_config.query_retry_attempts
        );
        None
    }

    pub async fn add_event<F, E>(&self, config: &config::Config, key: &str, field: F, event: E)
    where
        F: redis::ToRedisArgs + Clone + Send + Sync,
        E: serde::Serialize + std::fmt::Debug + Send,
    {
        let Some(mut conn) = self.conn() else { return };
        let redis_config = config.redis_config();

        let Ok(serialized_event) = serde_json::to_string(&event) else {
            warn!("Failed to serialize event: {event:?}");
            return;
        };

        for _ in 0..redis_config.query_retry_attempts {
            if conn
                .hset::<&str, F, String, ()>(key, field.clone(), serialized_event.clone())
                .await
                .is_ok()
            {
                return;
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(
                redis_config.query_retry_sleep_secs,
            ))
            .await;
        }

        warn!("Failed to add event to redis db");
    }

    pub async fn remove_event<F>(&self, config: &config::Config, key: &str, field: F)
    where
        F: redis::ToRedisArgs + Clone + Send + Sync,
    {
        let Some(mut conn) = self.conn() else { return };
        let redis_config = config.redis_config();

        for _ in 0..redis_config.query_retry_attempts {
            if conn.hdel::<&str, F, ()>(key, field.clone()).await.is_ok() {
                return;
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(
                redis_config.query_retry_sleep_secs,
            ))
            .await;
        }

        warn!("Failed to remove event from redis db");
    }

    pub async fn add<T>(&self, config: &config::Config, key: &str, object: &T)
    where
        T: serde::Serialize + std::fmt::Debug + Sync,
    {
        let Some(mut conn) = self.conn() else { return };
        let redis_config = config.redis_config();

        let Ok(serialized) = serde_json::to_string(object) else {
            warn!("Failed to serialize object: {object:?}");
            return;
        };

        let score = u64::try_from(chrono::Utc::now().timestamp_micros()).unwrap_or(0);

        for _ in 0..redis_config.query_retry_attempts {
            if conn
                .zadd::<&str, u64, String, ()>(key, serialized.clone(), score)
                .await
                .is_ok()
            {
                return;
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(
                redis_config.query_retry_sleep_secs,
            ))
            .await;
        }

        warn!("Failed to add object to {key}");
    }

    pub async fn remove<T>(&self, config: &config::Config, key: &str, object: &T)
    where
        T: serde::Serialize + std::fmt::Debug + Sync,
    {
        let Some(mut conn) = self.conn() else { return };
        let redis_config = config.redis_config();

        let Ok(serialized) = serde_json::to_string(object) else {
            warn!("Failed to serialize object: {object:?}");
            return;
        };

        for _ in 0..redis_config.query_retry_attempts {
            if conn
                .zrem::<&str, String, usize>(key, serialized.clone())
                .await
                .is_ok()
            {
                return;
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(
                redis_config.query_retry_sleep_secs,
            ))
            .await;
        }

        warn!("Failed to remove object from {key}");
    }

    pub async fn get_oldest<T: serde::de::DeserializeOwned>(
        &self,
        config: &config::Config,
        key: &str,
    ) -> Option<T> {
        let mut conn = self.conn()?;
        let redis_config = config.redis_config();

        for _ in 0..redis_config.query_retry_attempts {
            if let Ok(members) = conn.zrange::<&str, Vec<String>>(key, 0, 0).await {
                return members.first().and_then(|serialized| {
                    serde_json::from_str(serialized)
                        .map_err(|err| {
                            warn!("Failed to deserialize object from {key}: {err:?}");
                            err
                        })
                        .ok()
                });
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(
                redis_config.query_retry_sleep_secs,
            ))
            .await;
        }

        warn!("Failed to get oldest object from {key}");
        None
    }
}

// --- Feature-gated helpers for indexer checkpoint tracking ---

#[cfg(feature = "mongo-ingestion")]
pub const MONGODB_OMNI_EVENTS_RT: &str = "mongodb_omni_events_rt";

#[cfg(any(feature = "native-indexers", feature = "mongo-ingestion"))]
impl RelayerStore {
    pub async fn get_last_processed<K, V>(&self, config: &config::Config, key: K) -> Option<V>
    where
        K: redis::ToRedisArgs + Copy + Send + Sync,
        V: redis::FromRedisValue + Send + Sync,
    {
        let mut conn = self.conn()?;
        let redis_config = config.redis_config();

        for _ in 0..redis_config.query_retry_attempts {
            if let Ok(res) = conn.get::<K, V>(key).await {
                return Some(res);
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(
                redis_config.query_retry_sleep_secs,
            ))
            .await;
        }

        warn!("Failed to get last processed block from redis db");
        None
    }

    pub async fn update_last_processed<K, V>(&self, config: &config::Config, key: K, value: V)
    where
        K: redis::ToRedisArgs + Copy + Send + Sync,
        V: redis::ToRedisArgs + Copy + Send + Sync,
    {
        let Some(mut conn) = self.conn() else { return };
        let redis_config = config.redis_config();

        for _ in 0..redis_config.query_retry_attempts {
            if conn.set::<K, V, ()>(key, value).await.is_ok() {
                return;
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(
                redis_config.query_retry_sleep_secs,
            ))
            .await;
        }

        warn!("Failed to update last processed block in redis db");
    }
}

// --- Native indexers only ---

#[cfg(feature = "native-indexers")]
pub const SOLANA_EVENTS: &str = "solana_events";

#[cfg(feature = "native-indexers")]
pub fn get_last_processed_key(chain_kind: omni_types::ChainKind) -> String {
    use omni_types::ChainKind;
    match chain_kind {
        ChainKind::Sol => "SOLANA_LAST_PROCESSED_SIGNATURE".to_string(),
        _ => format!("{chain_kind:?}_LAST_PROCESSED_BLOCK"),
    }
}

#[cfg(feature = "native-indexers")]
const MAX_EVENTS_PER_BATCH: usize = 30;

#[cfg(feature = "native-indexers")]
impl RelayerStore {
    pub async fn get_events(
        &self,
        config: &config::Config,
        key: String,
    ) -> Option<Vec<(String, String)>> {
        let mut conn = self.conn()?;
        let redis_config = config.redis_config();

        for _ in 0..redis_config.query_retry_attempts {
            let mut iter = match conn.hscan::<String, (String, String)>(key.clone()).await {
                Ok(iter) => iter,
                Err(err) => {
                    warn!("Redis hscan failed: {err:?}");
                    tokio::time::sleep(tokio::time::Duration::from_secs(
                        redis_config.query_retry_sleep_secs,
                    ))
                    .await;
                    continue;
                }
            };

            let mut events = Vec::new();
            loop {
                if events.len() >= MAX_EVENTS_PER_BATCH {
                    break;
                }
                match tokio::time::timeout(
                    tokio::time::Duration::from_secs(redis_config.query_timeout_secs),
                    iter.next_item(),
                )
                .await
                {
                    Ok(Some(event)) => events.push(event),
                    Ok(None) => break,
                    Err(_) => {
                        warn!("Redis hscan iteration timed out");
                        break;
                    }
                }
            }

            return Some(events);
        }

        warn!("Failed to get events from redis db");
        None
    }
}
