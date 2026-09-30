//! Redis access for budget counters.
//!
//! One cached `MultiplexedConnection`, rebuilt lazily. Every call carries a
//! [`StoreBound`]: the pre-call/status path is held to 50 ms so an unavailable
//! store fails fast (and closed, per the caller), while the post-call path runs
//! off the response path and gets 1 s so a transient blip does not drop a write.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use redis::aio::MultiplexedConnection;
use redis::{AsyncConnectionConfig, RedisError};
use tokio::sync::RwLock;

/// Bound for the pre-call check and status reads.
const CHECK_TIMEOUT_MS: u64 = 50;
/// Bound for post-call writes; also the outer limit on the connection itself.
const RECORD_TIMEOUT_MS: u64 = 1000;

/// Which latency budget a store call runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreBound {
    /// Pre-call check / status read (50 ms).
    Check,
    /// Post-call reconciliation (1 s).
    Record,
}

impl StoreBound {
    fn duration(self) -> Duration {
        Duration::from_millis(match self {
            StoreBound::Check => CHECK_TIMEOUT_MS,
            StoreBound::Record => RECORD_TIMEOUT_MS,
        })
    }
}

/// The counter store could not answer within its bound.
#[derive(Debug, thiserror::Error)]
#[error("budget counter store unavailable: {0}")]
pub struct StoreError(pub String);

#[derive(Debug, Clone, Copy, Default)]
pub struct StoreStats {
    pub mget_calls: u64,
}

pub struct RedisBudgetStore {
    client: redis::Client,
    conn: RwLock<Option<MultiplexedConnection>>,
    mget_calls: AtomicU64,
}

fn is_transport_error(e: &RedisError) -> bool {
    e.is_timeout() || e.is_connection_dropped() || e.is_io_error()
}

impl RedisBudgetStore {
    pub fn new(client: redis::Client) -> Self {
        Self {
            client,
            conn: RwLock::new(None),
            mget_calls: AtomicU64::new(0),
        }
    }

    pub fn stats(&self) -> StoreStats {
        StoreStats {
            mget_calls: self.mget_calls.load(Ordering::Relaxed),
        }
    }

    async fn connection(&self) -> Result<MultiplexedConnection, RedisError> {
        if let Some(c) = self.conn.read().await.as_ref() {
            return Ok(c.clone());
        }
        let limit = Duration::from_millis(RECORD_TIMEOUT_MS);
        let cfg = AsyncConnectionConfig::new()
            .set_connection_timeout(limit)
            .set_response_timeout(limit);
        let fresh = self
            .client
            .get_multiplexed_async_connection_with_config(&cfg)
            .await?;
        *self.conn.write().await = Some(fresh.clone());
        Ok(fresh)
    }

    async fn reset(&self) {
        *self.conn.write().await = None;
    }

    /// Run `op` on the cached connection under `bound`. A transport error
    /// clears the connection and retries the command exactly once.
    async fn run<T, F, Fut>(&self, bound: StoreBound, op: F) -> Result<T, StoreError>
    where
        F: Fn(MultiplexedConnection) -> Fut,
        Fut: Future<Output = Result<T, RedisError>>,
    {
        let attempt = async {
            let mut retried = false;
            loop {
                let outcome = match self.connection().await {
                    Ok(conn) => op(conn).await,
                    Err(e) => Err(e),
                };
                match outcome {
                    Ok(v) => return Ok(v),
                    Err(e) if is_transport_error(&e) => {
                        self.reset().await;
                        if retried {
                            return Err(e);
                        }
                        retried = true;
                    }
                    Err(e) => return Err(e),
                }
            }
        };
        match tokio::time::timeout(bound.duration(), attempt).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(StoreError(e.to_string())),
            Err(_) => {
                self.reset().await;
                Err(StoreError(format!(
                    "no answer within {} ms",
                    bound.duration().as_millis()
                )))
            }
        }
    }

    /// One `MGET` for all keys.
    pub async fn mget(
        &self,
        keys: &[String],
        bound: StoreBound,
    ) -> Result<Vec<Option<i64>>, StoreError> {
        self.mget_calls.fetch_add(1, Ordering::Relaxed);
        self.run(bound, |mut conn| async move {
            redis::cmd("MGET")
                .arg(keys)
                .query_async::<Vec<Option<i64>>>(&mut conn)
                .await
        })
        .await
    }

    /// `SET key value NX EX ttl`; true when this call created the key.
    pub async fn set_nx_ex(
        &self,
        key: &str,
        value: i64,
        ttl_secs: u64,
        bound: StoreBound,
    ) -> Result<bool, StoreError> {
        self.run(bound, |mut conn| async move {
            let reply: Option<String> = redis::cmd("SET")
                .arg(key)
                .arg(value)
                .arg("NX")
                .arg("EX")
                .arg(ttl_secs)
                .query_async(&mut conn)
                .await?;
            Ok(reply.is_some())
        })
        .await
    }

    /// One `DEL` for all keys.
    pub async fn del(&self, keys: &[String], bound: StoreBound) -> Result<(), StoreError> {
        if keys.is_empty() {
            return Ok(());
        }
        self.run(bound, |mut conn| async move {
            redis::cmd("DEL")
                .arg(keys)
                .query_async::<i64>(&mut conn)
                .await
                .map(|_| ())
        })
        .await
    }
}
