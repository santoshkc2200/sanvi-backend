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

// Returns `{allowed, tokens}` rather than just `allowed`, so the caller can
// compute a `Retry-After` proportional to how close the bucket actually is
// to its next token, instead of a static "time to refill from empty."
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
return {allowed, tostring(tokens)}
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
        self.try_acquire_bucket(&format!("sanvi:rl:{key}")).await
    }

    /// Like [`Self::try_acquire`], but over an explicit bucket key. Used by
    /// per-tenant limiters whose buckets are namespaced by tenant id.
    pub async fn try_acquire_bucket(&self, bucket_key: &str) -> Result<(), LimitError> {
        let Some(conn) = &self.conn else {
            return Err(LimitError::BackendUnavailable);
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut conn = conn.clone();
        let result: redis::RedisResult<(i32, String)> = self
            .script
            .key(bucket_key)
            .arg(self.config.burst)
            .arg(self.config.per_second)
            .arg(now)
            .invoke_async(&mut conn)
            .await;

        match result {
            Ok((1, _)) => Ok(()),
            Ok((_, tokens)) => {
                let tokens: f64 = tokens.parse().unwrap_or(0.0);
                Err(LimitError::Denied {
                    retry_after_secs: retry_after_from_tokens(
                        tokens,
                        self.config.per_second as f64,
                    ),
                })
            }
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
        Allowance {
            allowed: false,
            retry_after_secs: retry_after_from_tokens(tokens, refill),
        }
    }
}

/// Seconds until the bucket holds one more token than it currently does —
/// the real wait, not a static "time to refill the whole bucket" estimate.
/// Shared by the live path (fed the token count the Lua script just wrote)
/// and `simulate_bucket` (the model the unit tests below pin), so a test
/// passing here is a guarantee about what `try_acquire` actually returns.
pub fn retry_after_from_tokens(tokens: f64, refill: f64) -> u64 {
    let needed = (1.0 - tokens).max(0.0);
    (needed / refill).ceil().max(1.0) as u64
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
    fn retry_after_scales_with_how_close_the_bucket_is_to_a_token() {
        // Nearly full deficit (almost 0 tokens): needs ~0.5s at refill 2/s.
        assert_eq!(retry_after_from_tokens(0.0, 2.0), 1);
        // Already 0.9 tokens in: only 0.05s needed, rounds up to 1s.
        assert_eq!(retry_after_from_tokens(0.9, 2.0), 1);
        // A slow refill rate (0.1/s) starting from empty needs 10s.
        assert_eq!(retry_after_from_tokens(0.0, 0.1), 10);
    }

    #[test]
    fn retry_after_never_reflects_a_full_bucket_refill_when_close_to_a_token() {
        // Burst 60 / refill 10 per second: the old static formula (burst /
        // per_second) would report a flat 6s regardless of state. A client
        // that is one tick away from a token must not be told to wait 6s.
        let almost_full = retry_after_from_tokens(0.95, 10.0);
        assert!(
            almost_full < 6,
            "expected a short wait near a full bucket, got {almost_full}s"
        );
    }
}
