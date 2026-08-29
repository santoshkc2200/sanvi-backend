use std::sync::Arc;

use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{DirectiveResolver, DirectiveState, ProcessingPurpose, SubjectRef};

use crate::domain::{
    AdvertisingError, ConversionEvent, ConversionEventRepository, ConversionId, SubjectKeyRef,
    UploadState,
};

/// Rebuilds the exact subject the capturing gate resolved against, from the
/// non-PII key material stored on the event (`SubjectKeyRef`). A `Contact`
/// event stored nothing reconstructable — TASK-014 forbids persisting the
/// raw email here — so there is no safe subject to re-query: `None` means
/// the caller must refuse the retry rather than check the wrong subject.
fn rebuild_subject(tenant_id: TenantId, subject_key: &SubjectKeyRef) -> Option<SubjectRef> {
    match subject_key {
        SubjectKeyRef::TenantDevice { device_ref } => {
            Some(SubjectRef::tenant_device(tenant_id, device_ref))
        }
        SubjectKeyRef::EndUser { external_id } => {
            Some(SubjectRef::end_user(tenant_id, external_id))
        }
        SubjectKeyRef::TenantUser { user_id } => sanvi_kernel::UserId::try_from(*user_id)
            .ok()
            .map(SubjectRef::user),
        SubjectKeyRef::Contact => None,
    }
}

/// Manual retry use case for parked conversion events (TASK-015, slice 10.6).
///
/// Re-queues a permanently failed/exhausted event for manual retry by flipping
/// each `Parked` platform upload state back to `Pending` with an incremented
/// attempt count. Retries are strictly refused for non-parked events or if consent
/// has been withdrawn since capture.
pub struct RetryConversion {
    tx: TxManager,
    repo: Arc<dyn ConversionEventRepository>,
    directive_resolver: Arc<dyn DirectiveResolver>,
}

impl RetryConversion {
    pub fn new(
        tx: TxManager,
        repo: Arc<dyn ConversionEventRepository>,
        directive_resolver: Arc<dyn DirectiveResolver>,
    ) -> Self {
        Self {
            tx,
            repo,
            directive_resolver,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        id: ConversionId,
    ) -> Result<ConversionEvent, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let mut event = self
            .repo
            .find_by_id(&mut scope, tenant_id, id)
            .await?
            .ok_or(AdvertisingError::ConversionNotFound)?;
        drop(scope);

        let has_parked = event
            .upload_states
            .values()
            .any(|state| matches!(state, UploadState::Parked { .. }));
        let is_already_retried_pending = event.upload_states.values().any(
            |state| matches!(state, UploadState::Pending { attempt_count } if *attempt_count > 0),
        );

        if !has_parked && !is_already_retried_pending {
            return Err(AdvertisingError::InvalidInput(
                "conversion retry is only available for parked events".to_string(),
            ));
        }

        let subject = rebuild_subject(tenant_id, &event.subject_key).ok_or_else(|| {
            AdvertisingError::InvalidInput(
                "retry refused: this event was captured under an email-identified subject, \
                 which this context never persists — its directive cannot be safely re-checked, \
                 so the event cannot be retried automatically"
                    .to_string(),
            )
        })?;
        let directive = self
            .directive_resolver
            .resolve(&subject, ProcessingPurpose::AdsMeasurement)
            .await
            .map_err(|err| {
                AdvertisingError::InvalidInput(format!("privacy resolver error: {err:?}"))
            })?;

        if directive.state == DirectiveState::Denied {
            return Err(AdvertisingError::InvalidInput(
                "retry refused: consent for ads measurement was withdrawn".to_string(),
            ));
        }

        if !has_parked {
            // Idempotent double-retry on an already-pending event returns the event unchanged
            return Ok(event);
        }

        for state in event.upload_states.values_mut() {
            if let UploadState::Parked { attempts, .. } = state {
                *state = UploadState::Pending {
                    attempt_count: *attempts + 1,
                };
            }
        }

        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        self.repo
            .update_upload_states(&mut scope, tenant_id, id, event.upload_states.clone())
            .await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;

        Ok(event)
    }
}
