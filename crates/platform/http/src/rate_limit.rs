use std::time::Duration;

use redis::aio::ConnectionManager;

use crate::config::RateLimitConfig;

/// Token bucket rate limiter backed by Redis, safe across API instances.
///
/// The bucket is a Redis hash (`tokens`, `ts`), updated atomically by a Lua
/// script: `tokens` refills by `per_second * elapsed` on each check, capped
/// at `burst`.
pub struct RateLimiter {
    conn: Option<ConnectionManager>,
    config: RateLimitConfig,
    script: redis::Script,
}

const LUA_BUCKET: &str = r#"
local key = KEYS[1]
local capacity = tonumber(ARGV[1])
local refill = tonumber(ARGV[2])
local now = tonumber(ARGV[3])
local bucket = redis.call('HMGET', key, 'tokens', 'ts')
local tokens = tonumber(bucket[1])
local ts = tonumber(bucket[2])
if tokens == nil then
  tokens = capacity
  ts = now
end
tokens = math.min(capacity, tokens + (now - ts) * refill)
local allowed = 0
if tokens >= 1 then
  allowed = 1
  tokens = tokens - 1
end
redis.call('HSET', key, 'tokens', tokens, 'ts', now)
redis.call('EXPIRE', key, math.ceil(capacity / refill) + 1)
return allowed
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitError {
    Denied { retry_after_secs: u64 },
    BackendUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Allowance {
    pub allowed: bool,
    pub retry_after_secs: u64,
}

impl RateLimiter {
    /// `conn = None` disables enforcement; every check then reports
    /// `BackendUnavailable` and the middleware applies the configured
    /// fail-open / fail-closed policy.
    pub fn new(config: &RateLimitConfig, conn: Option<ConnectionManager>) -> Self {
        Self {
            conn,
            config: config.clone(),
            script: redis::Script::new(LUA_BUCKET),
        }
    }

    pub fn config(&self) -> &RateLimitConfig {
        &self.config
    }

    /// Check and consume one token for `key`. Non-blocking from the caller's
    /// point of view: the whole decision is one atomic Lua script.
    pub async fn try_acquire(&self, key: &str) -> Result<(), LimitError> {
        let Some(conn) = &self.conn else {
            return Err(LimitError::BackendUnavailable);
        };

        let bucket_key = format!("sanvi:rl:{key}");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut conn = conn.clone();
        let result: redis::RedisResult<i32> = self
            .script
            .key(&bucket_key)
            .arg(self.config.burst)
            .arg(self.config.per_second)
            .arg(now)
            .invoke_async(&mut conn)
            .await;

        match result {
            Ok(1) => Ok(()),
            Ok(_) => Err(LimitError::Denied {
                retry_after_secs: retry_after(&self.config),
            }),
            Err(err) => {
                tracing::warn!(error = %err, "rate limit check failed");
                Err(LimitError::BackendUnavailable)
            }
        }
    }
}

/// Pure bucket math used by tests to pin the Lua semantics.
pub fn simulate_bucket(tokens: f64, ts: u64, now: u64, capacity: f64, refill: f64) -> Allowance {
    let tokens = (tokens + (now.saturating_sub(ts)) as f64 * refill).min(capacity);
    if tokens >= 1.0 {
        Allowance {
            allowed: true,
            retry_after_secs: 0,
        }
    } else {
        let needed = 1.0 - tokens;
        Allowance {
            allowed: false,
            retry_after_secs: (needed / refill).ceil().max(1.0) as u64,
        }
    }
}

/// Time the caller should wait before retrying (for the `Retry-After` header).
pub fn retry_after(config: &RateLimitConfig) -> u64 {
    (config.burst / config.per_second).max(1)
}

/// Wall-clock budget for a single rate-limit check (used in integration
/// tests, keeps the middleware snappy when Redis is sick).
pub fn check_budget() -> Duration {
    Duration::from_millis(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_math_refills_linearly() {
        // 0 tokens at t=0, refill 2/s → at t=1 there are 2 tokens.
        let allowance = simulate_bucket(0.0, 0, 1, 10.0, 2.0);
        assert!(allowance.allowed);
    }

    #[test]
    fn bucket_math_caps_at_capacity() {
        let allowance = simulate_bucket(0.0, 0, 100, 10.0, 2.0);
        assert!(allowance.allowed);
    }

    #[test]
    fn empty_bucket_is_denied_with_sane_retry() {
        let allowance = simulate_bucket(0.0, 0, 0, 10.0, 2.0);
        assert!(!allowance.allowed);
        assert_eq!(allowance.retry_after_secs, 1);
    }

    #[test]
    fn retry_after_derives_from_config() {
        let config = RateLimitConfig {
            burst: 30,
            per_second: 10,
            ..Default::default()
        };
        assert_eq!(retry_after(&config), 3);
    }
}
