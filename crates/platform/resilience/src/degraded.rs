//! The degradation registry: one shared, process-wide record of which
//! dependencies are currently running degraded (TASK-023, FR-1113).
//!
//! Adapters flip a reason on when their dependency misbehaves and off when
//! it recovers; the HTTP layer reads it to stamp `X-Sanvi-Degraded` on
//! every response served while any reason is active, so the frontend can
//! say honestly that the response came from a degraded path. The header
//! value is the comma-separated list of active reasons, each one of the
//! stable [`DegradationReason::as_str`] slugs the dependency matrix
//! documents.

use std::sync::atomic::{AtomicBool, Ordering};

use prometheus::{IntGaugeVec, Opts, Registry};

/// The documented degradation reasons. One per dependency the matrix names
/// that can degrade independently; adding a row to the matrix means adding
/// a variant here, and the `tests/resilience` suite for that row asserts
/// the flag flips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradationReason {
    Redis,
    Keto,
    Stripe,
    Ads,
    ObjectStorage,
    Postgres,
}

impl DegradationReason {
    pub const ALL: [DegradationReason; 6] = [
        DegradationReason::Redis,
        DegradationReason::Keto,
        DegradationReason::Stripe,
        DegradationReason::Ads,
        DegradationReason::ObjectStorage,
        DegradationReason::Postgres,
    ];

    /// The stable slug carried in `X-Sanvi-Degraded` and the matrix.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Redis => "redis",
            Self::Keto => "keto",
            Self::Stripe => "stripe",
            Self::Ads => "ads",
            Self::ObjectStorage => "object_storage",
            Self::Postgres => "postgres",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Redis => 0,
            Self::Keto => 1,
            Self::Stripe => 2,
            Self::Ads => 3,
            Self::ObjectStorage => 4,
            Self::Postgres => 5,
        }
    }
}

/// Process-wide degradation flags. Cheap to share (`Arc`), lock-free to
/// read on the response path.
pub struct DegradationRegistry {
    flags: [AtomicBool; DegradationReason::ALL.len()],
    gauges: Option<IntGaugeVec>,
}

impl std::fmt::Debug for DegradationRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DegradationRegistry")
            .field("active", &self.active_strings())
            .finish()
    }
}

impl Default for DegradationRegistry {
    fn default() -> Self {
        Self::new(None)
    }
}

impl DegradationRegistry {
    /// `registry` may be `None` in tests; the gauges then do not register.
    pub fn new(registry: Option<&Registry>) -> Self {
        let gauges = registry.map(|registry| {
            let gauges = IntGaugeVec::new(
                Opts::new(
                    "resilience_degraded",
                    "1 while the named dependency is degraded",
                ),
                &["reason"],
            )
            .expect("degradation gauge definition is valid");
            registry
                .register(Box::new(gauges.clone()))
                .expect("degradation gauges register once per process");
            gauges
        });
        Self {
            flags: Default::default(),
            gauges,
        }
    }

    /// Flip one reason's degradation state. Idempotent; the gauge follows.
    pub fn set(&self, reason: DegradationReason, active: bool) {
        let previous = self.flags[reason.index()].swap(active, Ordering::SeqCst);
        if previous != active {
            tracing::warn!(
                reason = reason.as_str(),
                active,
                "degradation state changed"
            );
        }
        if let Some(gauges) = &self.gauges {
            gauges
                .with_label_values(&[reason.as_str()])
                .set(i64::from(active));
        }
    }

    /// Convenience for the adapters: degrade while `failing`, clear when it
    /// recovers, exactly as the matrix rows describe.
    pub fn set_degraded(&self, reason: DegradationReason, failing: bool) {
        self.set(reason, failing);
    }

    pub fn is_active(&self, reason: DegradationReason) -> bool {
        self.flags[reason.index()].load(Ordering::SeqCst)
    }

    /// Every currently active reason, matrix-stable order.
    pub fn active(&self) -> Vec<DegradationReason> {
        DegradationReason::ALL
            .into_iter()
            .filter(|reason| self.is_active(*reason))
            .collect()
    }

    /// The `X-Sanvi-Degraded` header value: the active slugs, comma
    /// separated. `None` when nothing is degraded (no header is stamped).
    pub fn header_value(&self) -> Option<String> {
        let active = self.active_strings();
        if active.is_empty() {
            None
        } else {
            Some(active.join(","))
        }
    }

    fn active_strings(&self) -> Vec<&'static str> {
        self.active().into_iter().map(|r| r.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_value_lists_active_reasons_and_none_when_clear() {
        let registry = DegradationRegistry::default();
        assert_eq!(registry.header_value(), None);

        registry.set(DegradationReason::Redis, true);
        registry.set(DegradationReason::Keto, true);
        assert_eq!(registry.header_value().as_deref(), Some("redis,keto"));

        // Idempotent set keeps the flag, not the count.
        registry.set(DegradationReason::Redis, true);
        assert_eq!(registry.header_value().as_deref(), Some("redis,keto"));

        registry.set(DegradationReason::Redis, false);
        assert_eq!(registry.header_value().as_deref(), Some("keto"));
        assert!(!registry.is_active(DegradationReason::Redis));
        registry.set(DegradationReason::Keto, false);
        assert_eq!(registry.header_value(), None);
    }

    #[test]
    fn reason_slugs_are_stable() {
        assert_eq!(DegradationReason::Redis.as_str(), "redis");
        assert_eq!(DegradationReason::ObjectStorage.as_str(), "object_storage");
        assert_eq!(DegradationReason::Ads.as_str(), "ads");
    }
}
