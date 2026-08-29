//! Worker job that sweeps pending conversion events and uploads them to connected ad platforms (TASK-015, slice 10.6).

use std::collections::BTreeMap;
use std::sync::Arc;

use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{DirectiveResolver, DirectiveState, ProcessingPurpose};
use serde::Serialize;

use super::retry::rebuild_subject;
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
}

impl UploadPendingConversions {
    pub fn new(
        tx: TxManager,
        conversion_repo: Arc<dyn ConversionEventRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        directive_resolver: Arc<dyn DirectiveResolver>,
        sinks: BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>>,
    ) -> Self {
        Self {
            tx,
            conversion_repo,
            connections,
            directive_resolver,
            sinks,
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
            let directive_allowed = match subject {
                Some(ref sub) => match self
                    .directive_resolver
                    .resolve(sub, ProcessingPurpose::AdsMeasurement)
                    .await
                {
                    Ok(directive) => directive.state != DirectiveState::Denied,
                    Err(_) => false,
                },
                None => false, // Contact-identified subject cannot be safely re-queried
            };

            if !directive_allowed {
                let mut modified = false;
                for state in event.upload_states.values_mut() {
                    match state {
                        UploadState::Pending { attempt_count } => {
                            *state = UploadState::Failed {
                                reason: "suppressed_late: consent withdrawn since capture"
                                    .to_string(),
                                attempt_count: *attempt_count,
                            };
                            modified = true;
                        }
                        UploadState::Failed {
                            attempt_count,
                            reason,
                        } if !reason.contains("suppressed_late") => {
                            *state = UploadState::Failed {
                                reason: "suppressed_late: consent withdrawn since capture"
                                    .to_string(),
                                attempt_count: *attempt_count,
                            };
                            modified = true;
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
                    };
                    sink.upload(&handle, &event).await
                } else {
                    Err(AdvertisingError::ConnectionNotFound)
                };

                let new_state = match upload_result {
                    Ok(_) => {
                        report.uploaded += 1;
                        UploadState::Uploaded {
                            attempt_count: current_attempts,
                        }
                    }
                    Err(err) => {
                        let next_attempts = current_attempts + 1;
                        if next_attempts >= MAX_ATTEMPTS {
                            report.parked += 1;
                            UploadState::Parked {
                                reason: err.to_string(),
                                attempts: next_attempts,
                            }
                        } else {
                            report.failed += 1;
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

        Ok(report)
    }
}
