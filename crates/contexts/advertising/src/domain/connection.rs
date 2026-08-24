//! The durable tenant-to-ad-account relationship.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use super::{AdvertisingError, ConnectionHealth, ConnectionStatus, PlatformKey, SecretRef};

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
        })
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
            PlatformKey::parse("google_ads").unwrap(),
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
            PlatformKey::parse("meta").unwrap(),
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
            PlatformKey::parse("meta").unwrap(),
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
}
