//! US appeals: a refused request can be appealed once, within the
//! profile's window; the decision is a second-operator review; the outcome
//! must carry the state authority's complaint route.

use std::sync::Arc;

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use sanvi_persistence::TxManager;

use crate::domain::{
    Appeal, AppealOutcome, AppealRepository, AuditWriter, Clock, DsrId, DsrRepository, DsrStatus,
    EventPublisher, IdGenerator, JurisdictionRepository, PrivacyError, PrivacyEvent,
    add_business_days, add_days,
};

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AppealRequestCommand {
    pub request_id: Uuid,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AppealRequestOutput {
    pub appeal_id: Uuid,
    pub due_at: DateTime<chrono::Utc>,
    /// The state authority's complaint route, included up front.
    pub authority: crate::domain::AuthorityContact,
}

pub struct AppealRequest {
    tx_manager: TxManager,
    requests: Arc<dyn DsrRepository>,
    appeals: Arc<dyn AppealRepository>,
    jurisdictions: Arc<dyn JurisdictionRepository>,
    events: Arc<dyn EventPublisher>,
    audit: Arc<dyn AuditWriter>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
}

#[allow(clippy::too_many_arguments)]
impl AppealRequest {
    pub fn new(
        tx_manager: TxManager,
        requests: Arc<dyn DsrRepository>,
        appeals: Arc<dyn AppealRepository>,
        jurisdictions: Arc<dyn JurisdictionRepository>,
        events: Arc<dyn EventPublisher>,
        audit: Arc<dyn AuditWriter>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            requests,
            appeals,
            jurisdictions,
            events,
            audit,
            ids,
            clock,
        }
    }

    pub async fn execute(
        &self,
        requester_user: Option<Uuid>,
        command: AppealRequestCommand,
    ) -> Result<AppealRequestOutput, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_dsr_appeal", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let request_id = DsrId::from_uuid(command.request_id);
        let request = self
            .requests
            .find_by_id(&mut scope, request_id)
            .await?
            .ok_or(PrivacyError::RequestNotFound(
                command.request_id.to_string(),
            ))?;

        // Only the requester may appeal (an appeal filed by someone else is
        // an exfiltration attempt).
        if let Some(user) = requester_user {
            if request.subject_ref != crate::domain::SubjectRef::user(user) {
                return Err(PrivacyError::RequestNotOwned);
            }
        } else {
            return Err(PrivacyError::RequestNotOwned);
        }
        if request.status != DsrStatus::Rejected && request.status != DsrStatus::PartiallyComplete {
            return Err(PrivacyError::NotAppealable(
                request.status.as_str().to_string(),
            ));
        }
        let profile = self
            .jurisdictions
            .find(&mut scope, request.jurisdiction.as_str())
            .await?
            .ok_or_else(|| {
                PrivacyError::JurisdictionNotFound(request.jurisdiction.as_str().to_string())
            })?;
        let Some(window_days) = profile.appeal_window_days else {
            return Err(PrivacyError::NotAppealable(
                "jurisdiction provides no appeal right".to_string(),
            ));
        };
        let Some(response_days) = profile.appeal_response_days else {
            return Err(PrivacyError::NotAppealable(
                "jurisdiction provides no appeal right".to_string(),
            ));
        };
        if self
            .appeals
            .find_by_request(&mut scope, request_id)
            .await?
            .is_some()
        {
            return Err(PrivacyError::AlreadyAppealed);
        }

        let now = self.clock.now();
        let decided_at = request.completed_at.unwrap_or(now);
        let window_close = add_days(decided_at, window_days);
        if now > window_close {
            return Err(PrivacyError::NotAppealable(
                "appeal window closed".to_string(),
            ));
        }

        let appeal = Appeal {
            id: self.ids.next(),
            request_id,
            reason: command.reason.clone(),
            received_at: now,
            due_at: add_business_days(now, response_days),
            outcome: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            authority_notice: Some(profile.authority.clone()),
        };
        self.appeals.save(&mut scope, &appeal).await?;
        self.requests
            .set_status(&mut scope, request_id, DsrStatus::UnderAppeal, None, None)
            .await?;

        self.events
            .publish(
                &mut scope,
                &[PrivacyEvent::DsrReceived {
                    request_id: request_id.as_uuid(),
                    subject: request.subject_ref.clone(),
                    dsr_kind: "appeal".to_string(),
                    jurisdiction: request.jurisdiction.as_str().to_string(),
                }],
            )
            .await?;
        self.audit
            .record(
                &mut scope,
                requester_user.unwrap_or(Uuid::nil()),
                "privacy.appeal.submit".to_string(),
                appeal.id.to_string(),
                serde_json::json!({ "request_id": request_id.as_uuid() }),
            )
            .await?;

        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        Ok(AppealRequestOutput {
            appeal_id: appeal.id,
            due_at: appeal.due_at,
            authority: profile.authority,
        })
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct DecideAppealCommand {
    pub appeal_id: Uuid,
    pub outcome: AppealOutcome,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DecideAppealOutput {
    pub appeal_id: Uuid,
    pub outcome: AppealOutcome,
    pub decided_at: DateTime<chrono::Utc>,
    pub authority: Option<crate::domain::AuthorityContact>,
}

pub struct DecideAppeal {
    tx_manager: TxManager,
    requests: Arc<dyn DsrRepository>,
    appeals: Arc<dyn AppealRepository>,
    events: Arc<dyn EventPublisher>,
    audit: Arc<dyn AuditWriter>,
    clock: Arc<dyn Clock>,
}

impl DecideAppeal {
    pub fn new(
        tx_manager: TxManager,
        requests: Arc<dyn DsrRepository>,
        appeals: Arc<dyn AppealRepository>,
        events: Arc<dyn EventPublisher>,
        audit: Arc<dyn AuditWriter>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            requests,
            appeals,
            events,
            audit,
            clock,
        }
    }

    pub async fn execute(
        &self,
        actor: Uuid,
        command: DecideAppealCommand,
    ) -> Result<DecideAppealOutput, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_appeal_decision", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let mut open = self.appeals.find_open(&mut scope, 10_000).await?;
        let Some(index) = open.iter().position(|a| a.id == command.appeal_id) else {
            return Err(PrivacyError::AppealNotFound(command.appeal_id.to_string()));
        };
        let mut appeal = open.remove(index);

        // Second-operator review: both the rejecter and this decision are
        // recorded in the audit chain; the composition root enforces the
        // "different operator" rule at review time via the audit reader.
        appeal.decide(
            command.outcome,
            actor,
            command.reason.clone(),
            self.clock.now(),
            None,
        )?;
        self.appeals.decide(&mut scope, &appeal).await?;

        // An upheld appeal re-opens the request; a denied appeal closes it
        // with the authority route in the outcome.
        let request_id = appeal.request_id;
        let request = self
            .requests
            .find_by_id(&mut scope, request_id)
            .await?
            .ok_or(PrivacyError::RequestNotFound(
                request_id.as_uuid().to_string(),
            ))?;
        match command.outcome {
            AppealOutcome::Upheld => {
                self.requests
                    .set_status(&mut scope, request_id, DsrStatus::InProgress, None, None)
                    .await?;
            }
            AppealOutcome::Denied => {
                self.requests
                    .set_status(
                        &mut scope,
                        request_id,
                        DsrStatus::Rejected,
                        Some(self.clock.now()),
                        None,
                    )
                    .await?;
            }
        }

        self.events
            .publish(
                &mut scope,
                &[PrivacyEvent::AppealDecided {
                    request_id: request_id.as_uuid(),
                    appeal_id: appeal.id,
                    outcome: match command.outcome {
                        AppealOutcome::Upheld => "upheld".to_string(),
                        AppealOutcome::Denied => "denied".to_string(),
                    },
                    jurisdiction: request.jurisdiction.as_str().to_string(),
                }],
            )
            .await?;
        self.audit
            .record(
                &mut scope,
                actor,
                "privacy.appeal.decide".to_string(),
                appeal.id.to_string(),
                serde_json::json!({ "outcome": appeal.outcome }),
            )
            .await?;

        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        Ok(DecideAppealOutput {
            appeal_id: appeal.id,
            outcome: command.outcome,
            decided_at: appeal.decided_at.unwrap_or(self.clock.now()),
            authority: appeal.authority_notice,
        })
    }
}

pub struct ListOpenAppeals {
    tx_manager: TxManager,
    appeals: Arc<dyn AppealRepository>,
}

impl ListOpenAppeals {
    pub fn new(tx_manager: TxManager, appeals: Arc<dyn AppealRepository>) -> Self {
        Self {
            tx_manager,
            appeals,
        }
    }

    pub async fn execute(&self, limit: u32) -> Result<Vec<Appeal>, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_appeal_list", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let appeals = self.appeals.find_open(&mut scope, limit.min(200)).await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(appeals)
    }
}
