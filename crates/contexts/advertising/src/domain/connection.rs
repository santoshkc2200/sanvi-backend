//! The durable tenant-to-ad-account relationship.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use super::{
    AdvertisingError, ConnectionHealth, ConnectionStatus, PlatformAccount, PlatformKey, SecretRef,
};

/// ISO-4217's reserved "no currency involved" code — the correct placeholder
/// for a pending connection whose account (and therefore real currency)
/// isn't chosen yet, rather than an invented sentinel.
const PENDING_CURRENCY: &str = "XXX";
const PENDING_TIMEZONE: &str = "UTC";

/// Provider failure retained for operator diagnostics. Extra provider fields
/// stay intact through database round-trips rather than being discarded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncError {
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(flatten)]
    pub context: Map<String, Value>,
}

impl SyncError {
    pub fn new(
        code: impl Into<String>,
        message: Option<String>,
        context: Map<String, Value>,
    ) -> Result<Self, AdvertisingError> {
        let code = code.into().trim().to_string();
        if code.is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "advertising sync error code is required".to_string(),
            ));
        }
        Ok(Self {
            code,
            message,
            context,
        })
    }
}

#[derive(Clone)]
pub struct AdvertisingConnection {
    id: Uuid,
    tenant_id: Uuid,
    platform: PlatformKey,
    external_account_id: String,
    account_name: Option<String>,
    currency: String,
    timezone: String,
    status: ConnectionStatus,
    granted_scopes: Vec<String>,
    token_ref: SecretRef,
    connected_by: Option<Uuid>,
    connected_at: DateTime<Utc>,
    token_expires_at: Option<DateTime<Utc>>,
    last_synced_at: Option<DateTime<Utc>>,
    last_error: Option<SyncError>,
    disconnected_notified_at: Option<DateTime<Utc>>,
    dependent_jobs_paused_at: Option<DateTime<Utc>>,
    /// Set only while `status == Pending`: the account list fetched at
    /// exchange time, cached because the authorization code that produced
    /// it is already spent and cannot be replayed.
    pending_accounts: Option<Vec<PlatformAccount>>,
}

impl AdvertisingConnection {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tenant_id: Uuid,
        platform: PlatformKey,
        external_account_id: impl Into<String>,
        currency: impl Into<String>,
        timezone: impl Into<String>,
        token_ref: SecretRef,
        connected_by: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<Self, AdvertisingError> {
        let external_account_id = normalize_external_account_id(external_account_id.into())?;
        let currency = currency.into().trim().to_ascii_uppercase();
        let timezone = timezone.into().trim().to_string();
        if token_ref.is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "advertising token reference is required".to_string(),
            ));
        }
        if currency.len() != 3
            || !currency
                .chars()
                .all(|character| character.is_ascii_alphabetic())
        {
            return Err(AdvertisingError::InvalidInput(
                "advertising account currency must be ISO-4217".to_string(),
            ));
        }
        if timezone.is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "advertising account timezone is required".to_string(),
            ));
        }
        Ok(Self {
            id: Uuid::now_v7(),
            tenant_id,
            platform,
            external_account_id,
            account_name: None,
            currency,
            timezone,
            status: ConnectionStatus::Active,
            granted_scopes: Vec::new(),
            token_ref,
            connected_by,
            connected_at: now,
            token_expires_at: None,
            last_synced_at: None,
            last_error: None,
            disconnected_notified_at: None,
            dependent_jobs_paused_at: None,
            pending_accounts: None,
        })
    }

    /// A connection between "code exchanged" and "account chosen": no real
    /// currency/timezone yet, so those fields carry the ISO/tz placeholders
    /// rather than a guess — `finalize` overwrites them once known.
    /// `id` is caller-chosen (rather than freshly generated here) so the
    /// caller can compute `token_ref` — the connection's own id, stringified
    /// — before this returns, instead of mutating it in afterward.
    #[allow(clippy::too_many_arguments)]
    pub fn new_pending(
        id: Uuid,
        tenant_id: Uuid,
        platform: PlatformKey,
        token_ref: SecretRef,
        granted_scopes: Vec<String>,
        token_expires_at: Option<DateTime<Utc>>,
        accounts: Vec<PlatformAccount>,
        connected_by: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<Self, AdvertisingError> {
        if accounts.is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "advertising authorization exchange returned no accounts".to_string(),
            ));
        }
        let mut connection = Self::new(
            tenant_id,
            platform,
            format!("pending:{id}"),
            PENDING_CURRENCY,
            PENDING_TIMEZONE,
            token_ref,
            connected_by,
            now,
        )?;
        connection.id = id;
        connection.status = ConnectionStatus::Pending;
        connection.granted_scopes = normalize_scopes(granted_scopes);
        connection.token_expires_at = token_expires_at;
        connection.pending_accounts = Some(accounts);
        Ok(connection)
    }

    /// Disconnect in Sanvi. Does not reach the platform side — the tenant's
    /// ads keep running there until they act in the platform's own UI.
    pub fn disconnect(&mut self) {
        self.status = ConnectionStatus::Disconnected;
    }

    /// The tenant chose an account: leave `Pending`, adopt its identity and
    /// real currency/timezone, and drop the cached candidate list. Fails if
    /// the chosen account was never in that list — the picker only offers
    /// accounts this connection actually fetched.
    pub fn finalize(
        &mut self,
        chosen: &PlatformAccount,
        currency: impl Into<String>,
        timezone: impl Into<String>,
    ) -> Result<(), AdvertisingError> {
        if self.status != ConnectionStatus::Pending {
            return Err(AdvertisingError::InvalidInput(
                "advertising connection is not pending selection".to_string(),
            ));
        }
        let offered = self
            .pending_accounts
            .as_ref()
            .is_some_and(|accounts| accounts.contains(chosen));
        if !offered {
            return Err(AdvertisingError::InvalidInput(
                "chosen advertising account was not among the fetched accounts".to_string(),
            ));
        }
        let currency = currency.into().trim().to_ascii_uppercase();
        let timezone = timezone.into().trim().to_string();
        if currency.len() != 3 || !currency.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(AdvertisingError::InvalidInput(
                "advertising account currency must be ISO-4217".to_string(),
            ));
        }
        if timezone.is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "advertising account timezone is required".to_string(),
            ));
        }
        self.external_account_id = normalize_external_account_id(chosen.external_id.clone())?;
        self.account_name = (!chosen.display_name.trim().is_empty())
            .then(|| chosen.display_name.trim().to_string());
        self.currency = currency;
        self.timezone = timezone;
        self.status = ConnectionStatus::Active;
        self.pending_accounts = None;
        Ok(())
    }

    pub fn pending_accounts(&self) -> Option<&[PlatformAccount]> {
        self.pending_accounts.as_deref()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_persisted(
        id: Uuid,
        tenant_id: Uuid,
        platform: PlatformKey,
        external_account_id: String,
        account_name: Option<String>,
        currency: String,
        timezone: String,
        status: ConnectionStatus,
        granted_scopes: Vec<String>,
        token_ref: SecretRef,
        connected_by: Option<Uuid>,
        connected_at: DateTime<Utc>,
        token_expires_at: Option<DateTime<Utc>>,
        last_synced_at: Option<DateTime<Utc>>,
        last_error: Option<SyncError>,
        disconnected_notified_at: Option<DateTime<Utc>>,
        dependent_jobs_paused_at: Option<DateTime<Utc>>,
        pending_accounts: Option<Vec<PlatformAccount>>,
    ) -> Result<Self, AdvertisingError> {
        let mut connection = Self::new(
            tenant_id,
            platform,
            external_account_id,
            currency,
            timezone,
            token_ref,
            connected_by,
            connected_at,
        )?;
        connection.id = id;
        connection.account_name = account_name;
        connection.status = status;
        connection.granted_scopes = normalize_scopes(granted_scopes);
        connection.token_expires_at = token_expires_at;
        connection.last_synced_at = last_synced_at;
        connection.last_error = last_error;
        connection.disconnected_notified_at = disconnected_notified_at;
        connection.dependent_jobs_paused_at = dependent_jobs_paused_at;
        connection.pending_accounts = pending_accounts;
        Ok(connection)
    }

    pub fn apply_credentials(
        &mut self,
        account_name: Option<String>,
        granted_scopes: Vec<String>,
        token_expires_at: Option<DateTime<Utc>>,
    ) {
        self.account_name = account_name.and_then(|name| {
            let name = name.trim().to_string();
            (!name.is_empty()).then_some(name)
        });
        self.granted_scopes = normalize_scopes(granted_scopes);
        self.token_expires_at = token_expires_at;
        self.status = ConnectionStatus::Active;
        self.last_error = None;
        self.disconnected_notified_at = None;
        self.dependent_jobs_paused_at = None;
    }

    /// Persist the successful observation together with the renewed access
    /// token expiry. A recovered connection is eligible for dependent work
    /// again only after a provider-confirmed refresh.
    pub fn record_refresh_success(
        &mut self,
        token_expires_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) {
        self.status = ConnectionStatus::Active;
        self.token_expires_at = token_expires_at;
        self.last_synced_at = Some(now);
        self.last_error = None;
        self.disconnected_notified_at = None;
        self.dependent_jobs_paused_at = None;
    }

    /// A permanent credential failure stops further refresh attempts. Side
    /// effects are marked separately after their respective ports succeed.
    pub fn record_permanent_refresh_failure(&mut self, error: SyncError) {
        self.status = ConnectionStatus::Disconnected;
        self.last_error = Some(error);
    }

    /// Mark a successfully delivered reconnect notification. Keeping this
    /// separate from failure observation prevents a transient notifier error
    /// from being mistaken for a completed notification.
    pub fn mark_disconnected_notified(&mut self, now: DateTime<Utc>) {
        self.disconnected_notified_at.get_or_insert(now);
    }

    /// Mark dependent jobs paused only after the scheduler has committed the
    /// pause. Repeated workers retain the first successful marker.
    pub fn mark_dependent_jobs_paused(&mut self, now: DateTime<Utc>) {
        self.dependent_jobs_paused_at.get_or_insert(now);
    }

    pub fn record_sync_error(&mut self, error: SyncError) {
        self.last_error = Some(error);
    }

    pub fn health(&self, required_scopes: &[String], now: DateTime<Utc>) -> ConnectionHealth {
        ConnectionHealth::from_connection(
            self.status,
            self.token_expires_at,
            &self.granted_scopes,
            required_scopes,
            self.last_error.as_ref().map(|error| error.code.clone()),
            self.last_synced_at,
            now,
        )
    }

    pub fn granted_scopes(&self) -> &[String] {
        &self.granted_scopes
    }

    pub fn id(&self) -> Uuid {
        self.id
    }
    pub(crate) fn tenant_id(&self) -> Uuid {
        self.tenant_id
    }
    pub(crate) fn platform(&self) -> &PlatformKey {
        &self.platform
    }
    pub(crate) fn external_account_id(&self) -> &str {
        &self.external_account_id
    }
    pub(crate) fn account_name(&self) -> Option<&str> {
        self.account_name.as_deref()
    }
    pub(crate) fn currency(&self) -> &str {
        &self.currency
    }
    pub(crate) fn timezone(&self) -> &str {
        &self.timezone
    }
    pub(crate) fn status(&self) -> ConnectionStatus {
        self.status
    }
    pub(crate) fn token_ref(&self) -> &SecretRef {
        &self.token_ref
    }
    pub(crate) fn connected_by(&self) -> Option<Uuid> {
        self.connected_by
    }
    pub(crate) fn connected_at(&self) -> DateTime<Utc> {
        self.connected_at
    }
    pub(crate) fn token_expires_at(&self) -> Option<DateTime<Utc>> {
        self.token_expires_at
    }
    pub(crate) fn last_synced_at(&self) -> Option<DateTime<Utc>> {
        self.last_synced_at
    }
    pub(crate) fn last_error(&self) -> Option<&SyncError> {
        self.last_error.as_ref()
    }
    pub(crate) fn disconnected_notified_at(&self) -> Option<DateTime<Utc>> {
        self.disconnected_notified_at
    }
    pub(crate) fn dependent_jobs_paused_at(&self) -> Option<DateTime<Utc>> {
        self.dependent_jobs_paused_at
    }
}

fn normalize_external_account_id(value: String) -> Result<String, AdvertisingError> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(AdvertisingError::InvalidInput(
            "external advertising account id is required".to_string(),
        ));
    }
    Ok(value)
}

fn normalize_scopes(scopes: Vec<String>) -> Vec<String> {
    let mut scopes = scopes
        .into_iter()
        .map(|scope| scope.trim().to_string())
        .filter(|scope| !scope.is_empty())
        .collect::<Vec<_>>();
    scopes.sort();
    scopes.dedup();
    scopes
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use serde_json::json;

    use super::*;

    fn connection() -> AdvertisingConnection {
        AdvertisingConnection::new(
            Uuid::now_v7(),
            PlatformKey::parse("network_a").unwrap(),
            "123-456-7890",
            "jpy",
            "Asia/Tokyo",
            SecretRef::new("opaque-vault-reference"),
            Some(Uuid::now_v7()),
            Utc::now(),
        )
        .unwrap()
    }

    #[test]
    fn connection_health_uses_granted_not_requested_scopes() {
        let now = Utc::now();
        let mut connection = connection();
        connection.apply_credentials(
            None,
            vec!["ads.read".to_string()],
            Some(now + Duration::hours(1)),
        );
        let health = connection.health(&["ads.read".to_string(), "ads.manage".to_string()], now);
        assert!(!health.can_sync);
        assert_eq!(health.scopes_missing, ["ads.manage"]);
        assert_eq!(connection.currency(), "JPY");
    }

    #[test]
    fn account_id_is_canonical_and_empty_secret_reference_is_rejected() {
        let connection = AdvertisingConnection::new(
            Uuid::now_v7(),
            PlatformKey::parse("network_b").unwrap(),
            "  123-456-7890  ",
            "JPY",
            "Asia/Tokyo",
            SecretRef::new("opaque-vault-reference"),
            None,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(connection.external_account_id(), "123-456-7890");
        let result = AdvertisingConnection::new(
            Uuid::now_v7(),
            PlatformKey::parse("network_b").unwrap(),
            "123-456-7890",
            "JPY",
            "Asia/Tokyo",
            SecretRef::new(""),
            None,
            Utc::now(),
        );
        assert!(matches!(result, Err(AdvertisingError::InvalidInput(_))));
    }

    #[test]
    fn sync_error_keeps_provider_context() {
        let error = SyncError::new(
            "invalid_grant",
            Some("token revoked".to_string()),
            serde_json::from_value(json!({"provider_reason": "password_changed"})).unwrap(),
        )
        .unwrap();
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(value["provider_reason"], "password_changed");
        assert_eq!(serde_json::from_value::<SyncError>(value).unwrap(), error);
    }

    #[test]
    fn permanent_refresh_failure_records_only_completed_side_effects() {
        let now = Utc::now();
        let mut connection = connection();
        let revoked = SyncError::new("invalid_grant", None, Map::new()).unwrap();

        connection.record_permanent_refresh_failure(revoked.clone());
        connection.record_permanent_refresh_failure(revoked);
        assert_eq!(connection.status(), ConnectionStatus::Disconnected);
        assert!(connection.disconnected_notified_at().is_none());
        assert!(connection.dependent_jobs_paused_at().is_none());

        connection.mark_disconnected_notified(now + Duration::minutes(2));
        connection.mark_dependent_jobs_paused(now + Duration::minutes(2));
        connection.mark_disconnected_notified(now + Duration::minutes(3));
        connection.mark_dependent_jobs_paused(now + Duration::minutes(3));
        assert_eq!(
            connection.disconnected_notified_at(),
            Some(now + Duration::minutes(2))
        );
        assert_eq!(
            connection.dependent_jobs_paused_at(),
            Some(now + Duration::minutes(2))
        );

        connection
            .record_refresh_success(Some(now + Duration::hours(1)), now + Duration::minutes(2));
        assert_eq!(connection.status(), ConnectionStatus::Active);
        assert!(connection.disconnected_notified_at().is_none());
        assert!(connection.dependent_jobs_paused_at().is_none());
    }
}
