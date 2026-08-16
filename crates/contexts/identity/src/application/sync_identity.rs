//! The Kratos webhook projection sync: registration, login and settings
//! events keep the local `identity.users` row faithful to the identity
//! Kratos owns. Replay-safe and transactional.

use std::sync::Arc;

use sanvi_persistence::TxManager;
use tracing::{info, warn};

use crate::application::storage;
use crate::domain::events::IdentityEvent;
use crate::domain::user::{LinkedAccount, LinkedVia, User, UserId};
use crate::domain::values::{Email, Locale, ProviderId, ProviderSubject};
use crate::domain::{
    Clock, EventPublisher, IdGenerator, IdentityError, KratosClient, LinkedAccountRepository,
    Notifications, UserRepository,
};

/// A parsed, validated snapshot of what Kratos told us.
#[derive(Debug, Clone)]
pub struct SyncIdentityInput {
    pub flow: Flow,
    pub kratos_identity_id: uuid::Uuid,
    pub email: Email,
    pub email_verified: bool,
    pub locale: Option<Locale>,
    /// `provider:subject` strings from the identity's OIDC credentials.
    pub oidc_identifiers: Vec<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Registration,
    Login,
    Settings,
}

impl Flow {
    pub fn parse(raw: &str) -> Result<Self, IdentityError> {
        match raw {
            "registration" => Ok(Self::Registration),
            "login" => Ok(Self::Login),
            "settings" => Ok(Self::Settings),
            other => Err(IdentityError::MalformedWebhook(format!(
                "unknown flow {other:?}"
            ))),
        }
    }
}

pub struct SyncIdentityFromKratos {
    tx_manager: TxManager,
    users: Arc<dyn UserRepository>,
    links: Arc<dyn LinkedAccountRepository>,
    events: Arc<dyn EventPublisher>,
    notifications: Arc<dyn Notifications>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
    kratos: Arc<dyn KratosClient>,
    trusted_providers: Vec<String>,
}

impl SyncIdentityFromKratos {
    // One port per dependency; the composition root wires them explicitly
    // (same shape as every other use case in this codebase).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_manager: TxManager,
        users: Arc<dyn UserRepository>,
        links: Arc<dyn LinkedAccountRepository>,
        events: Arc<dyn EventPublisher>,
        notifications: Arc<dyn Notifications>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
        kratos: Arc<dyn KratosClient>,
        trusted_providers: Vec<String>,
    ) -> Self {
        Self {
            tx_manager,
            users,
            links,
            events,
            notifications,
            ids,
            clock,
            kratos,
            trusted_providers,
        }
    }

    /// Idempotent by construction: refresh is an upsert, linking is guarded
    /// by the `(provider, subject)` primary key, and registration only
    /// creates when the Kratos identity is unknown.
    pub async fn execute(&self, mut input: SyncIdentityInput) -> Result<(), IdentityError> {
        // Kratos strips `credentials` from webhook payloads: resolve OIDC
        // identifiers through the admin API (best effort — a reachable
        // Kratos is already proven by the flow that triggered us).
        if let Ok(Some(identity)) = self.kratos.get_identity(input.kratos_identity_id).await {
            input.oidc_identifiers = identity
                .oidc_identifiers
                .into_iter()
                .map(|(provider, subject)| format!("{}:{}", provider.as_str(), subject.as_str()))
                .collect();
        }
        let mut scope = self
            .tx_manager
            .begin_unscoped("kratos_webhook", "kratos")
            .await
            .map_err(storage)?;
        let now = self.clock.now();

        let Some(mut user) = self
            .users
            .find_by_kratos(&mut scope, input.kratos_identity_id)
            .await?
        else {
            // Unknown Kratos identity: registration for a fresh user, or —
            // defensively — an identity whose email collides with an
            // existing user. The latter must never silently merge: Kratos
            // already refused the flow; we refuse to bind anything.
            match self.users.find_by_email(&mut scope, &input.email).await? {
                Some(existing) => {
                    warn!(
                        user_id = %existing.id(),
                        kratos_identity_id = %input.kratos_identity_id,
                        "kratos identity for an unknown id carries a known email; refusing to bind"
                    );
                    let events = self.rejected_link_events(None, &input);
                    self.events.publish(&mut scope, &events).await?;
                    scope.commit().await.map_err(storage)?;
                    return Ok(());
                }
                None => {
                    let user = User::project_new(
                        self.ids.next_user_id(),
                        input.kratos_identity_id,
                        input.email.clone(),
                        input.email_verified,
                        input.locale.clone(),
                        now,
                    );
                    let mut events = vec![IdentityEvent::UserRegistered {
                        user_id: user.id(),
                        email: user.primary_email().clone(),
                    }];
                    self.users.save(&mut scope, &user).await?;
                    events.extend(
                        self.link_identifiers(&mut scope, &user, &input, true)
                            .await?,
                    );
                    if input.flow == Flow::Login
                        && let Some(session_id) = &input.session_id
                    {
                        events.push(IdentityEvent::SessionCreated {
                            user_id: user.id(),
                            session_id: session_id.clone(),
                        });
                    }
                    self.events.publish(&mut scope, &events).await?;
                    scope.commit().await.map_err(storage)?;
                    info!(
                        user_id = %user.id(),
                        kratos_identity_id = %input.kratos_identity_id,
                        "user projection created from kratos registration"
                    );
                    return Ok(());
                }
            }
        };

        // Known identity: refresh + link any new OIDC identifiers.
        let email_changed = user.apply_snapshot(
            Some(input.email.clone()),
            input.email_verified,
            input.locale.clone(),
        );
        let mut events = Vec::new();
        if email_changed {
            self.users.save(&mut scope, &user).await?;
            if input.email_verified {
                events.push(IdentityEvent::UserEmailVerified {
                    user_id: user.id(),
                    email: user.primary_email().clone(),
                });
            }
        }
        events.extend(
            self.link_identifiers(&mut scope, &user, &input, false)
                .await?,
        );
        if input.flow == Flow::Login
            && let Some(session_id) = &input.session_id
        {
            events.push(IdentityEvent::SessionCreated {
                user_id: user.id(),
                session_id: session_id.clone(),
            });
        }
        if !events.is_empty() {
            self.events.publish(&mut scope, &events).await?;
        }
        scope.commit().await.map_err(storage)?;

        // Security notices leave the transaction (best effort): the owner
        // must be told when a provider auto-linked to their account.
        for event in &events {
            if let IdentityEvent::AccountLinked {
                user_id,
                provider,
                linked_via: LinkedVia::AutoVerified,
                ..
            } = event
            {
                let _ = self
                    .notifications
                    .send_security_notice(
                        user.primary_email(),
                        "provider_linked",
                        &format!(
                            "a new sign-in provider was linked to the account (user {user_id}): {}",
                            provider.as_str()
                        ),
                    )
                    .await;
            }
        }
        Ok(())
    }

    /// Apply the account-linking algorithm to every OIDC identifier on the
    /// identity. `new_registration` = the identity itself was just created
    /// (link freely as `Registration`).
    async fn link_identifiers(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        user: &User,
        input: &SyncIdentityInput,
        new_registration: bool,
    ) -> Result<Vec<IdentityEvent>, IdentityError> {
        let mut events = Vec::new();
        let now = self.clock.now();
        for identifier in &input.oidc_identifiers {
            let Some((provider, subject)) = split_identifier(identifier) else {
                continue;
            };
            let provider = match ProviderId::parse(&provider) {
                Ok(provider) => provider,
                Err(_) => continue,
            };
            let subject = ProviderSubject::new(subject);

            if self.links.find(scope, &provider, &subject).await?.is_some() {
                continue; // already linked somewhere; idempotent.
            }

            if new_registration {
                let account = LinkedAccount::new(
                    user.id(),
                    provider.clone(),
                    subject.clone(),
                    LinkedVia::Registration,
                    now,
                );
                self.links.link(scope, &account).await?;
                events.push(IdentityEvent::AccountLinked {
                    user_id: user.id(),
                    provider: provider.clone(),
                    subject: subject.as_str().to_string(),
                    linked_via: LinkedVia::Registration,
                });
                continue;
            }

            // Existing user: auto-link ONLY when the provider is on the
            // trust allow-list, the provider verified the email, and the
            // local account's email is verified for the same address.
            // Everything else: must-not-link → challenge path later.
            let trusted = self
                .trusted_providers
                .iter()
                .any(|p| p.eq_ignore_ascii_case(provider.as_str()));
            if trusted && input.email_verified && user.email_verified() {
                let account = LinkedAccount::new(
                    user.id(),
                    provider.clone(),
                    subject.clone(),
                    LinkedVia::AutoVerified,
                    now,
                );
                self.links.link(scope, &account).await?;
                events.push(IdentityEvent::AccountLinked {
                    user_id: user.id(),
                    provider: provider.clone(),
                    subject: subject.as_str().to_string(),
                    linked_via: LinkedVia::AutoVerified,
                });
            } else {
                events.push(IdentityEvent::AccountLinkRejected {
                    user_id: Some(user.id()),
                    provider: provider.clone(),
                    subject: subject.as_str().to_string(),
                    email: user.primary_email().clone(),
                });
            }
        }
        Ok(events)
    }

    /// AccountLinkRejected events for every OIDC identifier on a colliding
    /// identity (the defensive registration path).
    fn rejected_link_events(
        &self,
        user_id: Option<UserId>,
        input: &SyncIdentityInput,
    ) -> Vec<IdentityEvent> {
        input
            .oidc_identifiers
            .iter()
            .filter_map(|identifier| {
                let (provider, subject) = split_identifier(identifier)?;
                let provider = ProviderId::parse(&provider).ok()?;
                Some(IdentityEvent::AccountLinkRejected {
                    user_id,
                    provider,
                    subject: subject.to_string(),
                    email: input.email.clone(),
                })
            })
            .collect()
    }
}

/// Kratos stores OIDC identifiers as `provider:subject`.
fn split_identifier(identifier: &str) -> Option<(String, String)> {
    let (provider, subject) = identifier.split_once(':')?;
    if provider.is_empty() || subject.is_empty() {
        return None;
    }
    Some((provider.to_string(), subject.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::IdentityEventName;

    #[test]
    fn flow_parse_roundtrips() {
        assert_eq!(Flow::parse("registration").unwrap(), Flow::Registration);
        assert_eq!(Flow::parse("login").unwrap(), Flow::Login);
        assert_eq!(Flow::parse("settings").unwrap(), Flow::Settings);
        assert!(Flow::parse("bogus").is_err());
    }

    #[test]
    fn identifier_splitting_is_defensive() {
        assert_eq!(
            split_identifier("google:subject-1"),
            Some(("google".to_string(), "subject-1".to_string()))
        );
        assert_eq!(split_identifier("no-colon"), None);
        assert_eq!(split_identifier(":empty-provider"), None);
    }

    #[test]
    fn event_names_are_stable() {
        assert_eq!(
            IdentityEventName::UserRegistered.as_str(),
            "identity.user_registered"
        );
        assert_eq!(
            IdentityEventName::AccountLinkRejected.as_str(),
            "identity.account_link_rejected"
        );
    }
}
