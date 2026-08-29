//! Worker job that sweeps uploaded conversion events whose consent has been
//! withdrawn since upload and issues deletion/suppression requests to connected
//! ad platforms (TASK-015, slice 10.6).

use std::collections::BTreeMap;
use std::sync::Arc;

use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{DirectiveResolver, DirectiveState, ProcessingPurpose};
use serde::Serialize;

use super::retry::rebuild_subject;
use crate::adapters::AdvertisingMetrics;
use crate::domain::{
    AdvertisingConnectionRepository, AdvertisingError, ConnectionHandle, ConnectionStatus,
    ConversionEventRepository, ConversionSinkPort, PlatformKey, UploadState,
};

const ACTOR: &str = "advertising-conversion-retraction";

/// One sweep pass outcome, for the job log line and metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RetractionSweepReport {
    pub checked: u64,
    pub retracted: u64,
    pub unpropagated: u64,
    pub failed: u64,
}

pub struct RetractWithdrawnConversions {
    tx: TxManager,
    conversion_repo: Arc<dyn ConversionEventRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    directive_resolver: Arc<dyn DirectiveResolver>,
    sinks: BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>>,
    metrics: Arc<AdvertisingMetrics>,
}

impl RetractWithdrawnConversions {
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

    pub async fn execute(&self, limit: i64) -> Result<RetractionSweepReport, AdvertisingError> {
        let mut report = RetractionSweepReport::default();

        let batch = {
            let mut scope = self
                .tx
                .begin_unscoped("advertising_conversion_retraction_sweep", ACTOR)
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            let batch = self
                .conversion_repo
                .list_uploaded(&mut scope, limit)
                .await?;
            scope
                .commit()
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            batch
        };

        for mut event in batch {
            let has_uploaded = event
                .upload_states
                .values()
                .any(|state| matches!(state, UploadState::Uploaded { .. }));
            if !has_uploaded {
                continue;
            }

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
                None => false, // Contact-identified subject cannot be safely re-queried or resolver failed -> treat as no longer permitted
            };

            // If consent is still allowed, retraction is not needed
            if directive_allowed {
                continue;
            }

            // 2. Fetch connections for tenant
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

            // Collect platforms that are in Uploaded state
            let due: Vec<PlatformKey> = event
                .upload_states
                .iter()
                .filter_map(|(platform, state)| match state {
                    UploadState::Uploaded { .. } => Some(platform.clone()),
                    _ => None,
                })
                .collect();

            let mut event_modified = false;
            for platform in due {
                let Some(sink) = self.sinks.get(&platform) else {
                    continue;
                };

                let active_conn = connections_list
                    .iter()
                    .find(|c| c.platform() == &platform && c.status() == ConnectionStatus::Active);

                let retract_result = if let Some(conn) = active_conn {
                    let handle = ConnectionHandle {
                        id: conn.id().to_string(),
                    };
                    sink.retract(&handle, &event).await
                } else {
                    Err(AdvertisingError::ConnectionNotFound)
                };

                match retract_result {
                    Ok(()) => {
                        report.retracted += 1;
                        self.metrics
                            .conversion_retraction_total()
                            .with_label_values(&[platform.as_str(), "retracted"])
                            .inc();
                        event.upload_states.insert(platform, UploadState::Retracted);
                        event_modified = true;
                    }
                    Err(err) => {
                        if is_retraction_unsupported(&err) {
                            report.unpropagated += 1;
                            self.metrics
                                .conversion_retraction_total()
                                .with_label_values(&[platform.as_str(), "unpropagated"])
                                .inc();
                            let reason = extract_unsupported_reason(&err);
                            event
                                .upload_states
                                .insert(platform, UploadState::Unpropagated { reason });
                            event_modified = true;
                        } else {
                            report.failed += 1;
                            self.metrics
                                .conversion_retraction_total()
                                .with_label_values(&[platform.as_str(), "failed"])
                                .inc();
                            // Leave as Uploaded so it can be retried on the next pass
                        }
                    }
                }
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

fn is_retraction_unsupported(err: &AdvertisingError) -> bool {
    match err {
        AdvertisingError::Validation(violations) => violations
            .iter()
            .any(|v| v.code == "capability_unsupported"),
        AdvertisingError::PlatformUnavailable(msg) | AdvertisingError::InvalidInput(msg) => {
            let lower = msg.to_lowercase();
            lower.contains("unsupported")
                || lower.contains("not supported")
                || lower.contains("does not support")
                || lower.contains("capability")
                || lower.contains("not implemented")
                || lower.contains("cannot be retracted")
        }
        _ => false,
    }
}

fn extract_unsupported_reason(err: &AdvertisingError) -> String {
    match err {
        AdvertisingError::Validation(violations) => {
            let msgs: Vec<&str> = violations.iter().map(|v| v.message.as_str()).collect();
            if msgs.is_empty() {
                err.to_string()
            } else {
                msgs.join("; ")
            }
        }
        _ => err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ValidationViolation;

    #[test]
    fn test_is_retraction_unsupported_detection() {
        let validation_unsupported =
            AdvertisingError::Validation(vec![ValidationViolation::unsupported(
                "retract",
                "retraction unsupported on this platform",
            )]);
        assert!(is_retraction_unsupported(&validation_unsupported));
        assert_eq!(
            extract_unsupported_reason(&validation_unsupported),
            "retraction unsupported on this platform"
        );

        let platform_unsupported = AdvertisingError::PlatformUnavailable(
            "Platform does not support conversion retraction".to_string(),
        );
        assert!(is_retraction_unsupported(&platform_unsupported));
        assert_eq!(
            extract_unsupported_reason(&platform_unsupported),
            "platform unavailable: Platform does not support conversion retraction"
        );

        let transient_err =
            AdvertisingError::PlatformUnavailable("503 Service Unavailable".to_string());
        assert!(!is_retraction_unsupported(&transient_err));

        let conn_err = AdvertisingError::ConnectionNotFound;
        assert!(!is_retraction_unsupported(&conn_err));
    }

    #[test]
    fn test_upload_state_retraction_serialization() {
        let retracted = UploadState::Retracted;
        let json = serde_json::to_value(&retracted).unwrap();
        assert_eq!(json, serde_json::json!({ "status": "retracted" }));
        let deserialized: UploadState = serde_json::from_value(json).unwrap();
        assert_eq!(deserialized, UploadState::Retracted);

        let unpropagated = UploadState::Unpropagated {
            reason: "retraction unsupported".to_string(),
        };
        let json_unprop = serde_json::to_value(&unpropagated).unwrap();
        assert_eq!(
            json_unprop,
            serde_json::json!({ "status": "unpropagated", "reason": "retraction unsupported" })
        );
        let deserialized_unprop: UploadState = serde_json::from_value(json_unprop).unwrap();
        assert_eq!(
            deserialized_unprop,
            UploadState::Unpropagated {
                reason: "retraction unsupported".to_string()
            }
        );
    }
}
