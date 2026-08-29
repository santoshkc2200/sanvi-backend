//! Advertising domain metrics: conversion upload totals by platform and result,
//! conversion suppression totals by reason and signal source, upload lag histogram,
//! and conversion DLQ depth gauge (TASK-015, slice 10.6).

use chrono::{DateTime, Utc};

#[derive(Clone)]
pub struct AdvertisingMetrics {
    conversion_upload_total: prometheus::IntCounterVec,
    conversion_suppressed_total: prometheus::IntCounterVec,
    conversion_retraction_total: prometheus::IntCounterVec,
    conversion_upload_lag_seconds: prometheus::Histogram,
    conversion_dlq_depth: prometheus::IntGauge,
    audience_sync_total: prometheus::IntCounterVec,
}

impl AdvertisingMetrics {
    pub fn new(registry: &prometheus::Registry) -> Self {
        let conversion_upload_total = prometheus::IntCounterVec::new(
            prometheus::Opts::new(
                "conversion_upload_total",
                "Total advertising conversion upload attempts by platform and result",
            ),
            &["platform", "result"],
        )
        .expect("conversion upload counter construction is infallible");

        let conversion_suppressed_total = prometheus::IntCounterVec::new(
            prometheus::Opts::new(
                "conversion_suppressed_total",
                "Total advertising conversion suppressions by reason and signal source",
            ),
            &["reason", "source"],
        )
        .expect("conversion suppressed counter construction is infallible");

        let conversion_retraction_total = prometheus::IntCounterVec::new(
            prometheus::Opts::new(
                "conversion_retraction_total",
                "Total advertising conversion retraction attempts by platform and result",
            ),
            &["platform", "result"],
        )
        .expect("conversion retraction counter construction is infallible");

        let conversion_upload_lag_seconds =
            prometheus::Histogram::with_opts(prometheus::HistogramOpts::new(
                "conversion_upload_lag_seconds",
                "Seconds between conversion capture and successful upload",
            ))
            .expect("conversion upload lag histogram construction is infallible");

        let conversion_dlq_depth = prometheus::IntGauge::new(
            "conversion_dlq_depth",
            "Count of conversion events currently parked in the DLQ",
        )
        .expect("conversion dlq depth gauge construction is infallible");

        let audience_sync_total = prometheus::IntCounterVec::new(
            prometheus::Opts::new(
                "audience_sync_total",
                "Total advertising audience sync attempts by platform and result",
            ),
            &["platform", "result"],
        )
        .expect("audience sync counter construction is infallible");

        let _ = registry.register(Box::new(conversion_upload_total.clone()));
        let _ = registry.register(Box::new(conversion_suppressed_total.clone()));
        let _ = registry.register(Box::new(conversion_retraction_total.clone()));
        let _ = registry.register(Box::new(conversion_upload_lag_seconds.clone()));
        let _ = registry.register(Box::new(conversion_dlq_depth.clone()));
        let _ = registry.register(Box::new(audience_sync_total.clone()));

        Self {
            conversion_upload_total,
            conversion_suppressed_total,
            conversion_retraction_total,
            conversion_upload_lag_seconds,
            conversion_dlq_depth,
            audience_sync_total,
        }
    }

    pub fn conversion_upload_total(&self) -> &prometheus::IntCounterVec {
        &self.conversion_upload_total
    }

    pub fn conversion_suppressed_total(&self) -> &prometheus::IntCounterVec {
        &self.conversion_suppressed_total
    }

    pub fn conversion_retraction_total(&self) -> &prometheus::IntCounterVec {
        &self.conversion_retraction_total
    }

    pub fn conversion_upload_lag_seconds(&self) -> &prometheus::Histogram {
        &self.conversion_upload_lag_seconds
    }

    pub fn conversion_dlq_depth(&self) -> &prometheus::IntGauge {
        &self.conversion_dlq_depth
    }

    pub fn audience_sync_total(&self) -> &prometheus::IntCounterVec {
        &self.audience_sync_total
    }

    /// Observe the lag of one successfully uploaded conversion event.
    pub fn observe_lag(&self, captured_at: DateTime<Utc>, now: DateTime<Utc>) {
        self.conversion_upload_lag_seconds
            .observe((now - captured_at).num_seconds() as f64);
    }
}

/// No-op metrics for tests and non-worker processes.
impl Default for AdvertisingMetrics {
    fn default() -> Self {
        Self::new(&prometheus::Registry::new())
    }
}
