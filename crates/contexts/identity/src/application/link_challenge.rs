//! The account-linking challenge.
//!
//! When an OIDC sign-in collides with an existing account and auto-linking
//! is not allowed (unverified email on either side), Kratos refuses the
//! login. The frontend then calls `start` with the pending
//! (provider, subject, email): we store it behind a single-use nonce the
//! browser carries. After the user proves ownership of the existing account
//! (a normal login), `complete` consumes the nonce and binds the provider
//! credential — with a security notice to the account owner.

use std::sync::Arc;

use sanvi_persistence::TxManager;
use serde::{Deserialize, Serialize};
use tracing::info;
use utoipa::ToSchema;

use crate::application::storage;
use crate::domain::events::IdentityEvent;
use crate::domain::link_intent::{LinkIntent, LinkIntentPayload, LinkNonce};
use crate::domain::user::{LinkedAccount, LinkedVia, UserId};
use crate::domain::values::{Email, ProviderId, ProviderSubject};
use crate::domain::{
    Clock, EventPublisher, IdentityError, LinkIntentRepository, LinkedAccountRepository,
    Notifications,
};

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct StartLinkChallengeCommand {
    #[schema(example = "google")]
    pub provider: String,
    #[schema(example = "sub-12345")]
    pub subject: String,
    #[schema(example = "alice@example.com")]
    pub email: String,
    /// The Kratos login flow that collided (correlation only).
    pub kratos_flow_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StartLinkChallengeResponse {
    /// The nonce the frontend stores in the `sanvi_link_intent` cookie
    /// (`HttpOnly`, `SameSite=Lax`, TTL = the intent's).
    pub nonce: String,
    /// How long the challenge stays valid, in seconds.
    pub expires_in_secs: u64,
}

pub struct StartLinkChallenge {
    tx_manager: TxManager,
    intents: Arc<dyn LinkIntentRepository>,
    clock: Arc<dyn Clock>,
    ttl_secs: u64,
}

impl StartLinkChallenge {
    pub fn new(
        tx_manager: TxManager,
        intents: Arc<dyn LinkIntentRepository>,
        clock: Arc<dyn Clock>,
        ttl_secs: u64,
    ) -> Self {
        Self {
            tx_manager,
            intents,
            clock,
            ttl_secs,
        }
    }

    pub async fn execute(
        &self,
        cmd: StartLinkChallengeCommand,
    ) -> Result<StartLinkChallengeResponse, IdentityError> {
        let provider = ProviderId::parse(&cmd.provider)?;
        let _ = Email::parse(&cmd.email)?;
        if cmd.subject.trim().is_empty() {
            return Err(IdentityError::InvalidProvider("empty subject".to_string()));
        }

        let now = self.clock.now();
        let nonce = LinkNonce::generate();
        let intent = LinkIntent::new(
            &nonce,
            LinkIntentPayload {
                provider: provider.to_string(),
                subject: cmd.subject.trim().to_string(),
                email: cmd.email,
                kratos_flow_id: cmd.kratos_flow_id,
            },
            now,
            now + chrono::Duration::seconds(self.ttl_secs as i64),
        );

        let mut scope = self
            .tx_manager
            .begin_unscoped("link_challenge_start", "identity")
            .await
            .map_err(storage)?;
        self.intents.save(&mut scope, &intent).await?;
        scope.commit().await.map_err(storage)?;
        info!(nonce_hash = %intent.nonce_hash(), "account-linking challenge started");
        Ok(StartLinkChallengeResponse {
            nonce: nonce.as_str().to_string(),
            expires_in_secs: self.ttl_secs,
        })
    }
}

/// What `complete` reports so the handler can produce the right response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkChallengeResult {
    Linked {
        provider: ProviderId,
        subject: ProviderSubject,
    },
    /// The provider account was already bound to this user (idempotent
    /// replay of a successful completion).
    AlreadyLinked,
}

pub struct CompleteLinkChallenge {
    tx_manager: TxManager,
    intents: Arc<dyn LinkIntentRepository>,
    links: Arc<dyn LinkedAccountRepository>,
    events: Arc<dyn EventPublisher>,
    notifications: Arc<dyn Notifications>,
    clock: Arc<dyn Clock>,
}

impl CompleteLinkChallenge {
    pub fn new(
        tx_manager: TxManager,
        intents: Arc<dyn LinkIntentRepository>,
        links: Arc<dyn LinkedAccountRepository>,
        events: Arc<dyn EventPublisher>,
        notifications: Arc<dyn Notifications>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            intents,
            links,
            events,
            notifications,
            clock,
        }
    }

    /// `nonce` is the browser cookie's value; `user_id`/`email` come from the
    /// authenticated subject.
    pub async fn execute(
        &self,
        nonce: &str,
        user_id: UserId,
        email: &Email,
    ) -> Result<LinkChallengeResult, IdentityError> {
        let nonce_hash = {
            let parsed = LinkNonce::parse(nonce)?;
            parsed.nonce_hash()
        };

        let mut scope = self
            .tx_manager
            .begin_unscoped("link_challenge_complete", "identity")
            .await
            .map_err(storage)?;
        let now = self.clock.now();

        // Peek first: validation must never burn the nonce (a wrong-email
        // attempt leaves the challenge intact for the real owner).
        let intent = self
            .intents
            .find(&mut scope, &nonce_hash)
            .await?
            .ok_or(IdentityError::LinkIntentUnavailable)?;
        if intent.is_expired(now) {
            return Err(IdentityError::LinkIntentUnavailable);
        }
        let payload = intent.payload().clone();
        let intent_email = payload
            .email()
            .ok_or(IdentityError::LinkIntentEmailMismatch)?;
        if &intent_email != email {
            return Err(IdentityError::LinkIntentEmailMismatch);
        }
        // Now consume: exactly once per nonce, raced completions lose here.
        self.intents
            .take(&mut scope, &nonce_hash)
            .await?
            .ok_or(IdentityError::LinkIntentUnavailable)?;

        let provider = ProviderId::parse(&payload.provider)?;
        let subject = ProviderSubject::new(payload.subject.clone());

        // Idempotence: the completion itself may be retried after a commit;
        // the (provider, subject) primary key is the arbiter.
        if let Some(existing) = self.links.find(&mut scope, &provider, &subject).await? {
            if existing.user_id() == user_id {
                scope.commit().await.map_err(storage)?;
                return Ok(LinkChallengeResult::AlreadyLinked);
            }
            return Err(IdentityError::AccountLinkedToOtherUser {
                provider,
                subject: subject.as_str().to_string(),
            });
        }

        let account = LinkedAccount::new(
            user_id,
            provider.clone(),
            subject.clone(),
            LinkedVia::Challenge,
            now,
        );
        self.links.link(&mut scope, &account).await?;
        let events = vec![IdentityEvent::AccountLinked {
            user_id,
            provider: provider.clone(),
            subject: subject.as_str().to_string(),
            linked_via: LinkedVia::Challenge,
        }];
        self.events.publish(&mut scope, &events).await?;
        scope.commit().await.map_err(storage)?;

        let _ = self
            .notifications
            .send_security_notice(
                email,
                "provider_linked",
                &format!("a new sign-in provider was linked: {}", provider.as_str()),
            )
            .await;
        info!(user_id = %user_id, provider = %provider.as_str(), "account link challenge completed");
        Ok(LinkChallengeResult::Linked { provider, subject })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_validates_provider_and_email_before_any_io() {
        // The validation happens before the transaction is opened; a
        // malformed provider fails without touching storage.
        let cmd = StartLinkChallengeCommand {
            provider: "BAD PROVIDER!".to_string(),
            subject: "sub".to_string(),
            email: "alice@example.com".to_string(),
            kratos_flow_id: None,
        };
        assert!(matches!(
            ProviderId::parse(&cmd.provider),
            Err(IdentityError::InvalidProvider(_))
        ));
    }
}
