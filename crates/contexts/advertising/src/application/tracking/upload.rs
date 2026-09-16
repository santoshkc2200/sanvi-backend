//! Worker job that sweeps pending conversion events and uploads them to connected ad platforms (TASK-015, slice 10.6).

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{DirectiveResolver, DirectiveState, ProcessingPurpose};
use serde::Serialize;
use tracing::Instrument;

use super::retry::rebuild_subject;
use crate::adapters::AdvertisingMetrics;
use crate::domain::{
    AdvertisingConnectionRepository, AdvertisingError, ConnectionHandle, ConnectionStatus,
    ConversionEventRepository, ConversionSinkPort, PlatformKey, UploadState,
};

const ACTOR: &str = "advertising-conversion-upload";
const MAX_ATTEMPTS: u32 = 5;

/// One sweep pass outcome, for the job log line and metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct UploadSweepReport {
    pub checked: u64,
    pub uploaded: u64,
    pub failed: u64,
    pub parked: u64,
    pub suppressed_late: u64,
}

pub struct UploadPendingConversions {
    tx: TxManager,
    conversion_repo: Arc<dyn ConversionEventRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    directive_resolver: Arc<dyn DirectiveResolver>,
    sinks: BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>>,
    metrics: Arc<AdvertisingMetrics>,
}

impl UploadPendingConversions {
    pub fn new(
        tx: TxManager,
        conversion_repo: Arc<dyn ConversionEventRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        directive_resolver: Arc<dyn DirectiveResolver>,
        sinks: BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>>,
        metrics: Arc<AdvertisingMetrics>,
    ) -> Self {
        Self {
            tx,
            conversion_repo,
            connections,
            directive_resolver,
            sinks,
            metrics,
        }
    }

    pub async fn execute(&self, limit: i64) -> Result<UploadSweepReport, AdvertisingError> {
        let mut report = UploadSweepReport::default();

        let batch = {
            let mut scope = self
                .tx
                .begin_unscoped("advertising_conversion_upload_sweep", ACTOR)
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            let batch = self
                .conversion_repo
                .list_pending_upload(&mut scope, limit)
                .await?;
            scope
                .commit()
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            batch
        };

        for mut event in batch {
            report.checked += 1;

            // 1. Rebuild subject & re-check ads_measurement directive
            let subject = rebuild_subject(event.tenant_id, &event.subject_key);
            let resolved_directive = match subject {
                Some(ref sub) => self
                    .directive_resolver
                    .resolve(sub, ProcessingPurpose::AdsMeasurement)
                    .await
                    .ok(),
                None => None,
            };

            let directive_allowed = match &resolved_directive {
                Some(directive) => directive.state != DirectiveState::Denied,
                None => false, // Contact-identified subject cannot be safely re-queried or resolver failed
            };

            if !directive_allowed {
                let (reason, source) = if let Some(denied_purpose) = event
                    .consent
                    .answers
                    .iter()
                    .find(|(_, state)| *state == "denied")
                    .map(|(p, _)| p.as_str())
                {
                    (denied_purpose, event.consent.signal_source.as_str())
                } else {
                    let src = resolved_directive
                        .as_ref()
                        .map(|d| d.source.as_str())
                        .unwrap_or(event.consent.signal_source.as_str());
                    (ProcessingPurpose::AdsMeasurement.as_str(), src)
                };

                let mut modified = false;
                for (platform, state) in event.upload_states.iter_mut() {
                    match state {
                        UploadState::Pending { attempt_count } => {
                            *state = UploadState::Failed {
                                reason: "suppressed_late: consent withdrawn since capture"
                                    .to_string(),
                                attempt_count: *attempt_count,
                            };
                            modified = true;
                            self.metrics
                                .conversion_upload_total()
                                .with_label_values(&[platform.as_str(), "suppressed_late"])
                                .inc();
                        }
                        UploadState::Failed {
                            attempt_count,
                            reason: prev_reason,
                        } if !prev_reason.contains("suppressed_late") => {
                            *state = UploadState::Failed {
                                reason: "suppressed_late: consent withdrawn since capture"
                                    .to_string(),
                                attempt_count: *attempt_count,
                            };
                            modified = true;
                            self.metrics
                                .conversion_upload_total()
                                .with_label_values(&[platform.as_str(), "suppressed_late"])
                                .inc();
                        }
                        _ => {}
                    }
                }

                if modified {
                    let mut scope = self
                        .tx
                        .begin_for(event.tenant_id.as_i64())
                        .await
                        .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
                    self.conversion_repo
                        .update_upload_states(
                            &mut scope,
                            event.tenant_id,
                            event.id,
                            event.upload_states.clone(),
                        )
                        .await?;
                    scope
                        .commit()
                        .await
                        .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
                }

                self.metrics
                    .conversion_suppressed_total()
                    .with_label_values(&[reason, source])
                    .inc();

                report.suppressed_late += 1;
                continue;
            }

            // 2. Otherwise process pending/failed platforms
            let connections_list = {
                let mut scope = self
                    .tx
                    .begin_for(event.tenant_id.as_i64())
                    .await
                    .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
                let list = self.connections.list(&mut scope, event.tenant_id).await?;
                scope
                    .commit()
                    .await
                    .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
                list
            };

            // Decide which platforms are due for an attempt before borrowing
            // `event` immutably in `sink.upload(&event)` below — holding a
            // `&mut` into `event.upload_states` across that `.await` is what
            // the borrow checker (rightly) refuses.
            let due: Vec<(PlatformKey, u32)> = event
                .upload_states
                .iter()
                .filter_map(|(platform, state)| {
                    let current_attempts = match state {
                        UploadState::Pending { attempt_count } => *attempt_count,
                        UploadState::Failed {
                            attempt_count,
                            reason,
                        } => {
                            if reason.contains("suppressed_late") {
                                return None;
                            }
                            *attempt_count
                        }
                        _ => return None,
                    };
                    Some((platform.clone(), current_attempts))
                })
                .collect();

            let mut event_modified = false;
            for (platform, current_attempts) in due {
                let Some(sink) = self.sinks.get(&platform) else {
                    // Sink missing in registry; skip platform without fabricating connection
                    continue;
                };

                let active_conn = connections_list
                    .iter()
                    .find(|c| c.platform() == &platform && c.status() == ConnectionStatus::Active);

                let upload_result = if let Some(conn) = active_conn {
                    let handle = ConnectionHandle {
                        id: conn.id().to_string(),
                        tenant_id: conn.tenant_id(),
                        external_account_id: conn.external_account_id().to_string(),
                    };
                    // End-to-end traceability: the outbound platform call gets
                    // its own span so capture → gate → upload is one chain.
                    // `instrument` rather than `enter()`: the entered guard is
                    // deliberately !Send and may not cross the await.
                    sink.upload(&handle, &event)
                        .instrument(tracing::info_span!(
                            "advertising.conversion_upload",
                            platform = platform.as_str(),
                            event_id = %event.event_id,
                            attempt = current_attempts,
                        ))
                        .await
                } else {
                    Err(AdvertisingError::ConnectionNotFound)
                };

                let new_state = match upload_result {
                    Ok(_) => {
                        report.uploaded += 1;
                        self.metrics
                            .conversion_upload_total()
                            .with_label_values(&[platform.as_str(), "uploaded"])
                            .inc();
                        self.metrics.observe_lag(event.occurred_at, Utc::now());
                        UploadState::Uploaded {
                            attempt_count: current_attempts,
                        }
                    }
                    Err(err) => {
                        let next_attempts = current_attempts + 1;
                        if next_attempts >= MAX_ATTEMPTS {
                            report.parked += 1;
                            self.metrics
                                .conversion_upload_total()
                                .with_label_values(&[platform.as_str(), "parked"])
                                .inc();
                            UploadState::Parked {
                                reason: err.to_string(),
                                attempts: next_attempts,
                            }
                        } else {
                            report.failed += 1;
                            self.metrics
                                .conversion_upload_total()
                                .with_label_values(&[platform.as_str(), "failed"])
                                .inc();
                            UploadState::Failed {
                                reason: err.to_string(),
                                attempt_count: next_attempts,
                            }
                        }
                    }
                };
                event.upload_states.insert(platform, new_state);
                event_modified = true;
            }

            if event_modified {
                let mut scope = self
                    .tx
                    .begin_for(event.tenant_id.as_i64())
                    .await
                    .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
                self.conversion_repo
                    .update_upload_states(
                        &mut scope,
                        event.tenant_id,
                        event.id,
                        event.upload_states.clone(),
                    )
                    .await?;
                scope
                    .commit()
                    .await
                    .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            }
        }

        let dlq_count = {
            let mut scope = self
                .tx
                .begin_unscoped("advertising_conversion_dlq_depth", ACTOR)
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            let count = self.conversion_repo.count_parked(&mut scope).await?;
            scope
                .commit()
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            count
        };
        self.metrics.conversion_dlq_depth().set(dlq_count);

        Ok(report)
    }
}
