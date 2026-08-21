//! The checkout throttle: session-creation flooding is Sanvi's problem
//! (card-testing at the provider's hosted page is the tenant's).
//!
//! Buckets are checked **before any database or provider work**: one per
//! tenant, one per client IP, one per customer reference. The shipped
//! backend is an in-process fixed window — honest protection for a single
//! instance and a deterministic test double; multi-instance deployments
//! wire the same port over Redis in the composition root, and a missing
//! backend fails closed (`RateLimited`) rather than open.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::domain::error::PaymentsError;
use crate::domain::ports::CheckoutThrottle;

const WINDOW: Duration = Duration::from_secs(60);

struct Bucket {
    window_start: Instant,
    count: u32,
}

/// Fixed-window limiter, one bucket per key. `limit` is requests/minute.
pub struct FixedWindowThrottle {
    limit: u32,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl FixedWindowThrottle {
    pub fn new(limit: u32) -> Self {
        Self {
            limit: limit.max(1),
            buckets: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl CheckoutThrottle for FixedWindowThrottle {
    async fn acquire(&self, bucket: &str) -> Result<(), PaymentsError> {
        let mut buckets = self.buckets.lock().expect("throttle lock");
        let now = Instant::now();
        let entry = buckets.entry(bucket.to_string()).or_insert(Bucket {
            window_start: now,
            count: 0,
        });
        if now.duration_since(entry.window_start) >= WINDOW {
            entry.window_start = now;
            entry.count = 0;
        }
        if entry.count >= self.limit {
            return Err(PaymentsError::RateLimited(format!(
                "bucket {bucket} exceeded {}/minute",
                self.limit
            )));
        }
        entry.count += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn buckets_allow_up_to_the_limit_then_fail_closed() {
        let throttle = FixedWindowThrottle::new(3);
        for _ in 0..3 {
            throttle.acquire("tenant:x").await.expect("within limit");
        }
        let err = throttle.acquire("tenant:x").await.unwrap_err();
        assert!(matches!(err, PaymentsError::RateLimited(_)));

        // A different tenant shares nothing — isolation extends to abuse
        // controls.
        throttle.acquire("tenant:y").await.expect("separate bucket");
    }

    #[test]
    fn the_limit_never_lands_on_zero() {
        // A misconfigured zero would brick checkout entirely; it clamps
        // to one request per window instead.
        assert_eq!(FixedWindowThrottle::new(0).limit, 1);
    }
}
