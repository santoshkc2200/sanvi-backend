//! Provider consent, token refresh and credential-backed verification.
//!
//! Consent is bound by signed, single-use state to the tenant, user, book and
//! connection that started it, and is accepted only for the organization
//! already registered on that connection. Refreshes run under the credential
//! row lock because freee refresh tokens are single-use: two concurrent
//! refreshes would burn the token and disconnect the tenant.

use std::sync::Arc;

use async_trait::async_trait;
use sanvi_kernel::{Clock, TenantId};
use uuid::Uuid;

use super::{
    Actor, Permission, ProviderVerifier, Services,
    ports::{CredentialStore, ProviderDirectory, ProviderOAuth},
};
use crate::domain::{
    AccountMappings, AccountingError, BookId, Connection, ConnectionId, Credential,
    CredentialStatus, OAUTH_STATE_TTL, OAuthStateClaims, Provider, ProviderCatalog, Secret,
};

fn find<T: ?Sized>(
    clients: &[Arc<T>],
    provider: Provider,
    of: impl Fn(&T) -> Provider,
) -> Result<Arc<T>, AccountingError> {
    clients
        .iter()
        .find(|client| of(client) == provider)
        .cloned()
        .ok_or(AccountingError::VerificationUnavailable)
}

pub struct Authorization {
    pub services: Arc<Services>,
    pub credentials: Arc<dyn CredentialStore>,
    pub providers: Vec<Arc<dyn ProviderOAuth>>,
    /// HMAC key for OAuth state; from a deployed secret, never config files.
    pub state_key: Arc<[u8]>,
}

impl Authorization {
    fn oauth(&self, provider: Provider) -> Result<Arc<dyn ProviderOAuth>, AccountingError> {
        find(&self.providers, provider, |client| client.provider())
    }

    async fn user(&self, actor: Actor) -> Result<sanvi_kernel::UserId, AccountingError> {
        self.services.authorize(actor, Permission::Manage).await?;
        match actor {
            Actor::User { user_id, .. } => Ok(user_id),
            Actor::System { .. } => Err(AccountingError::Forbidden),
        }
    }
}

/// Returns the provider consent URL for one registered connection.
pub struct StartAuthorization(pub Arc<Authorization>);
impl StartAuthorization {
    pub async fn execute(
        &self,
        actor: Actor,
        book_id: BookId,
        connection_id: ConnectionId,
    ) -> Result<String, AccountingError> {
        let auth = &self.0;
        let user_id = auth.user(actor).await?;
        let book = auth.services.store.get(actor.tenant_id(), book_id).await?;
        let connection = book.connection(connection_id)?;
        let oauth = auth.oauth(connection.provider())?;
        let claims = OAuthStateClaims {
            tenant_id: actor.tenant_id(),
            user_id,
            book_id,
            connection_id,
            provider: connection.provider(),
            nonce: Uuid::now_v7(),
            expires_at: auth.services.clock.now() + OAUTH_STATE_TTL,
        };
        auth.credentials.issue_state(&claims).await?;
        Ok(oauth.authorization_url(&sanvi_crypto::sign(&auth.state_key, &claims)))
    }
}

/// Handles the provider callback. The state is burned before the code is
/// exchanged, so a replayed redirect never reaches the provider.
pub struct CompleteAuthorization(pub Arc<Authorization>);
impl CompleteAuthorization {
    pub async fn execute(
        &self,
        actor: Actor,
        state: &str,
        code: &str,
    ) -> Result<(), AccountingError> {
        let auth = &self.0;
        let user_id = auth.user(actor).await?;
        if code.is_empty() || code.len() > 1024 {
            return Err(AccountingError::Invalid(
                "authorization code must contain 1–1024 bytes",
            ));
        }
        let claims: OAuthStateClaims = sanvi_crypto::verify(&auth.state_key, state)
            .map_err(|_| AccountingError::AuthorizationRejected("invalid authorization state"))?;
        if claims.tenant_id != actor.tenant_id() || claims.user_id != user_id {
            return Err(AccountingError::AuthorizationRejected(
                "authorization was started by another user",
            ));
        }
        let now = auth.services.clock.now();
        if claims.expires_at <= now {
            return Err(AccountingError::AuthorizationRejected(
                "authorization state expired",
            ));
        }
        if !auth.credentials.consume_state(&claims).await? {
            return Err(AccountingError::AuthorizationRejected(
                "authorization state was already used",
            ));
        }
        let book = auth
            .services
            .store
            .get(actor.tenant_id(), claims.book_id)
            .await?;
        let connection = book.connection(claims.connection_id)?;
        if connection.provider() != claims.provider {
            return Err(AccountingError::AuthorizationRejected(
                "authorization state does not match the connection",
            ));
        }
        let oauth = auth.oauth(connection.provider())?;
        let grant = oauth.exchange_code(code).await?;
        if grant.organization_id.as_deref() != Some(connection.organization_id()) {
            // Do not keep access to an organization nobody registered.
            if let Err(error) = oauth.revoke(&grant.refresh_token).await {
                tracing::warn!(%error, "accounting: revoking a mismatched grant failed");
            }
            return Err(AccountingError::AuthorizationRejected(
                "consent was granted for a different organization",
            ));
        }
        auth.credentials
            .authorize(
                actor.tenant_id(),
                Credential::authorized(claims.book_id, claims.connection_id, grant, now),
            )
            .await
    }
}

/// Disconnects a connection that no longer routes postings. Local revocation
/// is authoritative; the provider call is best-effort.
pub struct RevokeAuthorization(pub Arc<Authorization>);
impl RevokeAuthorization {
    pub async fn execute(
        &self,
        actor: Actor,
        book_id: BookId,
        connection_id: ConnectionId,
    ) -> Result<(), AccountingError> {
        let auth = &self.0;
        auth.user(actor).await?;
        let book = auth.services.store.get(actor.tenant_id(), book_id).await?;
        let connection = book.connection(connection_id)?;
        let now = auth.services.clock.now();
        if book.active_connection(now) == Some(connection_id)
            || book.draining_connections(now).contains(&connection_id)
            || book
                .scheduled_switch(now)
                .is_some_and(|period| period.connection_id == connection_id)
        {
            return Err(AccountingError::Conflict(
                "connection still routes postings; switch away and wait for the grace window",
            ));
        }
        let oauth = auth.oauth(connection.provider()).ok();
        auth.credentials
            .update(
                actor.tenant_id(),
                connection_id,
                Box::new(move |credential| {
                    Box::pin(async move {
                        if let (Some(oauth), Some(tokens)) = (oauth, &credential.tokens)
                            && let Err(error) = oauth.revoke(&tokens.refresh_token).await
                        {
                            tracing::warn!(%error, "accounting: provider token revocation failed");
                        }
                        Ok(credential.revoked())
                    })
                }),
            )
            .await
            .map(|_| ())
    }
}

/// Hands out a valid access token, refreshing at most once across processes.
pub struct AccessTokens {
    pub credentials: Arc<dyn CredentialStore>,
    pub providers: Vec<Arc<dyn ProviderOAuth>>,
    pub clock: Arc<dyn Clock>,
}

impl AccessTokens {
    pub async fn access_token(
        &self,
        tenant_id: TenantId,
        connection: &Connection,
    ) -> Result<Secret, AccountingError> {
        let oauth = find(&self.providers, connection.provider(), |client| {
            client.provider()
        })?;
        let now = self.clock.now();
        let credential = self
            .credentials
            .update(
                tenant_id,
                connection.id(),
                Box::new(move |credential| {
                    Box::pin(async move {
                        if credential.usable_access_token(now).is_some()
                            || credential.status != CredentialStatus::Active
                        {
                            return Ok(credential);
                        }
                        let Some(refresh_token) = credential.refreshable_token(now).cloned() else {
                            return Ok(credential.requiring_reauthorization());
                        };
                        match oauth.refresh(&refresh_token).await {
                            Ok(grant) => Ok(credential.rotated(grant, now)),
                            Err(AccountingError::ReauthorizationRequired) => {
                                Ok(credential.requiring_reauthorization())
                            }
                            // Unknown outcome: keep the row; the next attempt asks again.
                            Err(error) => Err(error),
                        }
                    })
                }),
            )
            .await?;
        credential
            .usable_access_token(now)
            .cloned()
            .ok_or(AccountingError::ReauthorizationRequired)
    }
}

/// `ProviderVerifier` backed by real provider responses: an authorized
/// credential, a user allowed to post, and every mapped account and tax code.
pub struct CredentialVerifier {
    pub tokens: Arc<AccessTokens>,
    pub directories: Vec<Arc<dyn ProviderDirectory>>,
}

#[async_trait]
impl ProviderVerifier for CredentialVerifier {
    async fn verify(
        &self,
        tenant_id: TenantId,
        connection: &Connection,
    ) -> Result<(), AccountingError> {
        let directory = find(&self.directories, connection.provider(), |directory| {
            directory.provider()
        })?;
        let token = self
            .tokens
            .access_token(tenant_id, connection)
            .await
            .map_err(|error| match error {
                AccountingError::NotFound => {
                    AccountingError::VerificationFailed("connection has not been authorized")
                }
                other => other,
            })?;
        let catalog = directory
            .catalog(&token, connection.organization_id())
            .await?;
        check_catalog(connection.mappings(), &catalog)
    }
}

fn check_catalog(
    mappings: &AccountMappings,
    catalog: &ProviderCatalog,
) -> Result<(), AccountingError> {
    if !catalog.can_post {
        return Err(AccountingError::VerificationFailed(
            "authorized user cannot post to this organization",
        ));
    }
    if [
        &mappings.revenue_account,
        &mappings.refund_account,
        &mappings.clearing_account,
    ]
    .into_iter()
    .any(|account| !catalog.account_ids.contains(account))
    {
        return Err(AccountingError::VerificationFailed(
            "a mapped account does not exist or is disabled in the provider",
        ));
    }
    if mappings
        .tax_codes
        .values()
        .any(|code| !catalog.tax_codes.contains(code))
    {
        return Err(AccountingError::VerificationFailed(
            "a mapped tax code does not exist or is disabled in the provider",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mappings() -> AccountMappings {
        AccountMappings {
            revenue_account: "101".into(),
            refund_account: "102".into(),
            clearing_account: "103".into(),
            tax_codes: [("standard".into(), "129".into())].into(),
        }
    }

    fn catalog() -> ProviderCatalog {
        ProviderCatalog {
            can_post: true,
            account_ids: ["101", "102", "103"].map(String::from).into(),
            tax_codes: ["129"].map(String::from).into(),
        }
    }

    #[test]
    fn catalog_must_allow_posting_and_contain_every_mapping() {
        assert_eq!(check_catalog(&mappings(), &catalog()), Ok(()));

        let read_only = ProviderCatalog {
            can_post: false,
            ..catalog()
        };
        let mut missing_account = catalog();
        missing_account.account_ids.remove("102");
        let mut missing_tax = catalog();
        missing_tax.tax_codes.clear();
        for broken in [read_only, missing_account, missing_tax] {
            assert!(matches!(
                check_catalog(&mappings(), &broken),
                Err(AccountingError::VerificationFailed(_))
            ));
        }
    }
}
