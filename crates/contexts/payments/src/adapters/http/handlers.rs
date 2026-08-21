//! The payments context's HTTP handlers.
//!
//! Every error leaves as RFC 9457 problem+json whose `type` carries the
//! stable `payments/*` slug from the domain error
//! (`PaymentsError::type_slug`) — the taxonomy can't drift from the
//! contract because both read the same source.

use std::net::IpAddr;

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use sanvi_http::{
    ApiError, CurrentSubject, CurrentTenant, PROBLEM_TYPE_BASE, ProblemDetail, RequiredFeature,
    RequiredPermission,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::adapters::http::PaymentsState;
use crate::application::{ProvidersView, StartConnectionCommand};
use crate::domain::connection::ProviderConnection;
use crate::domain::error::PaymentsError;
use crate::domain::money::Currency;
use crate::domain::provider::{CountryCode, ProviderKind};

/// The payments context's domain-error → HTTP status/detail mapping. The
/// status vocabulary lives in [`ApiError`]; the `payments/*` problem type
/// is attached by [`payments_problem`].
pub fn api_error(err: PaymentsError) -> ApiError {
    match err {
        PaymentsError::InvalidMoney(detail) | PaymentsError::InvalidConnection(detail) => {
            ApiError::BadRequest { detail }
        }
        PaymentsError::InvalidCountry(detail) | PaymentsError::CountryUnsupported(detail) => {
            ApiError::BadRequest { detail }
        }
        PaymentsError::NotEntitled(feature) => ApiError::Forbidden {
            detail: format!("this requires the {feature} entitlement"),
        },
        PaymentsError::ConnectionExists(detail) => ApiError::Conflict { detail },
        PaymentsError::ConnectionNotActive(detail) | PaymentsError::IdempotencyConflict(detail) => {
            ApiError::Conflict { detail }
        }
        PaymentsError::CapabilityMissing { capability, detail } => ApiError::Conflict {
            detail: format!("capability {capability} is not active: {detail}"),
        },
        PaymentsError::CurrencyUnsupported { currency, detail } => ApiError::BadRequest {
            detail: format!("currency {currency} is not supported by this connection: {detail}"),
        },
        PaymentsError::ProviderUnavailable(detail) => ApiError::BadGateway { detail },
        // The provider is still rate-limiting after the adapter's own
        // backoff: tell the client to retry, and when.
        PaymentsError::ProviderRateLimited(_) => ApiError::TooManyRequests {
            retry_after_secs: 30,
        },
        // A configuration failure, already alerted in the adapter; to the
        // tenant it is an upstream outage, not a stack trace.
        PaymentsError::ProviderAuth => ApiError::internal("payment provider authentication failed"),
        PaymentsError::WebhookRejected(detail) => {
            // The detail is logged for observability; it is never echoed
            // to a caller who just failed authentication (09.3's
            // transport keeps the same discipline).
            tracing::warn!(detail = %detail, "payments webhook rejected");
            ApiError::Unauthorized
        }
        PaymentsError::Storage(detail) => {
            tracing::error!(error = %detail, "payments storage failure");
            ApiError::internal("storage failure")
        }
    }
}

/// Domain error → actix error whose problem body carries the stable
/// `payments/*` type slug. The shared RFC 9457 layer preserves an
/// already-well-formed problem body, so the slug reaches the client.
pub fn payments_problem(err: PaymentsError) -> actix_web::Error {
    let api = api_error(err.clone());
    let mut problem = api.problem();
    problem.r#type = format!("{PROBLEM_TYPE_BASE}/{}", err.type_slug());
    actix_web::error::InternalError::from_response(
        err,
        HttpResponse::build(api.status()).json(problem),
    )
    .into()
}

/// The required `Idempotency-Key` on every mutating payments endpoint
/// (NFR-904). Missing or malformed is a 400, never a silent second
/// side effect.
fn idempotency_key(req: &HttpRequest) -> Result<Uuid, actix_web::Error> {
    let header = req
        .headers()
        .get("Idempotency-Key")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::BadRequest {
            detail: "Idempotency-Key header is required on mutating payments endpoints".to_string(),
        })?;
    Uuid::parse_str(header).map_err(|_| {
        ApiError::BadRequest {
            detail: "Idempotency-Key must be a UUID".to_string(),
        }
        .into()
    })
}

/// The caller's IP for the audit trail (best effort — a proxy that sends
fn client_ip(req: &HttpRequest) -> Option<IpAddr> {
    let connection = req.connection_info();
    let raw = connection.realip_remote_addr()?;
    if let Ok(ip) = raw.parse() {
        return Some(ip);
    }
    // `ip:port` (and `[::1]:port`) shapes.
    let (addr, _) = raw.rsplit_once(':')?;
    addr.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

// ---------------------------------------------------------------------------
// View models
// ---------------------------------------------------------------------------

/// A tenant's connection to a payment provider: identifiers and status
/// only — no PII, no credentials (the session secret is a separate,
/// never-persisted response).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConnectionView {
    pub id: Uuid,
    #[schema(example = "stripe_connect")]
    pub provider: String,
    #[schema(example = "onboarding")]
    pub status: String,
    /// The provider's capability map, verbatim from the last sync
    /// (`{"card_payments": {"status": "active"}}` for Stripe v2).
    pub capabilities: Value,
    /// The provider's outstanding-requirements summary, verbatim.
    pub requirements: Value,
    pub country: Option<String>,
    pub default_currency: Option<String>,
    #[schema(value_type = Option<String>)]
    pub connected_at: Option<DateTime<Utc>>,
    #[schema(value_type = Option<String>)]
    pub last_synced_at: Option<DateTime<Utc>>,
    /// Server-computed from the v2 `card_payments` capability — never a
    /// deprecated boolean.
    pub can_accept_payments: bool,
}

impl ConnectionView {
    pub fn of(connection: &ProviderConnection) -> Self {
        Self {
            id: connection.id,
            provider: connection.provider.as_str().to_string(),
            status: connection.status.as_str().to_string(),
            capabilities: connection.capabilities.clone(),
            requirements: connection.requirements.clone(),
            country: connection.country.clone(),
            default_currency: connection.default_currency.clone(),
            connected_at: connection.connected_at,
            last_synced_at: connection.last_synced_at,
            can_accept_payments: connection.capability_set().is_card_payments_active(),
        }
    }
}

/// `POST /api/v1/tenant/payments/connections` — start (or resume) a
/// connection. Idempotent per `Idempotency-Key`: a retry returns the
/// same connection; a crash between provider-side creation and
/// persistence adopts the same account.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateConnectionRequest {
    /// Provider kind, as listed by the catalog.
    #[schema(example = "stripe_connect")]
    pub provider: String,
    /// ISO-3166-1 alpha-2 country the tenant trades from; defaults to the
    /// tenant's region.
    #[schema(example = "JP")]
    pub country: Option<String>,
    /// ISO-4217 default currency for the connected account.
    #[schema(example = "JPY")]
    pub default_currency: Option<String>,
}

/// A short-lived Account Session for the embedded onboarding component.
/// Fetched per render; never persisted, never logged, single-render.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConnectionSessionView {
    pub connection_id: Uuid,
    #[schema(example = "stripe_connect")]
    pub provider: String,
    /// The provider's embedded-components client secret. Handle like a
    /// credential: no caching, no logging, one render.
    pub client_secret: String,
    #[schema(value_type = String)]
    pub expires_at: DateTime<Utc>,
    /// The embedded components enabled on this session (09.2:
    /// `account_onboarding` only).
    pub components: Vec<String>,
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `GET /api/v1/tenant/payments/providers`.
///
/// Gated three ways: the `payments.enabled` phase flag (route absent when
/// off), `payments.read` (permission extractor), and the
/// `payments.stripe_connect` entitlement (`RequiredFeature` — 403, never
/// 404, so the frontend can render an upgrade prompt instead of a dead
/// end).
///
/// The response is derived from the provider registry — the handler adds
/// nothing provider-specific. The tenant's country (from the tenant
/// context's region, when it parses as ISO-3166-1 alpha-2) decides
/// per-tenant availability; an unparseable region leaves the provider
/// available rather than locking the tenant out.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/payments/providers",
    tag = "payments",
    operation_id = "list_payment_providers",
    responses(
        (status = 200, description = "Connectable providers, derived from the provider registry", body = ProvidersView),
        (status = 403, description = "Missing payments.read permission or the payments.stripe_connect entitlement", body = ProblemDetail)
    )
)]
pub async fn list_payment_providers(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let tenant_country = CountryCode::parse(&tenant.0.region).ok();
    let view: ProvidersView = state
        .list_providers
        .execute(tenant_id, tenant_country)
        .await
        .map_err(payments_problem)?;
    state
        .metrics
        .catalog_requests
        .with_label_values(&["ok"])
        .inc();
    Ok(HttpResponse::Ok()
        .insert_header(("cache-control", "private, max-age=60"))
        .json(view))
}

/// `POST /api/v1/tenant/payments/connections` — connect a payment
/// provider. Creates the account at the provider (Accounts v2, keyed by
/// our connection id), persists the `Pending` connection, emits
/// `ProviderConnectionStarted` and writes the audit entry — in that
/// order, so a crashed response never orphans an account.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/payments/connections",
    tag = "payments",
    operation_id = "create_payment_connection",
    request_body = CreateConnectionRequest,
    responses(
        (status = 201, description = "Connection created (or the same one returned on an idempotent retry)", body = ConnectionView),
        (status = 400, description = "Missing/malformed Idempotency-Key, provider, country or currency", body = ProblemDetail),
        (status = 403, description = "Missing payments.manage permission or the payments.stripe_connect entitlement", body = ProblemDetail),
        (status = 409, description = "An active connection already exists (payments/connection-exists)", body = ProblemDetail),
        (status = 429, description = "Provider rate limit exhausted (payments/provider-rate-limited)", body = ProblemDetail),
        (status = 502, description = "Provider unavailable (payments/provider-unavailable)", body = ProblemDetail)
    ),
    params(
        ("Idempotency-Key" = uuid::Uuid, Header, description = "Required. The same key returns the same connection; it is also the provider-side idempotency key, so a retry after a crash adopts the created account instead of duplicating it")
    )
)]
pub async fn create_payment_connection(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    body: web::Json<CreateConnectionRequest>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let idempotency_key = idempotency_key(&req)?;
    let provider = ProviderKind::parse(body.provider.trim()).ok_or_else(|| {
        payments_problem(PaymentsError::InvalidConnection(format!(
            "unknown provider {:?}",
            body.provider
        )))
    })?;
    let country = body
        .country
        .as_deref()
        .map(CountryCode::parse)
        .transpose()
        .map_err(payments_problem)?;
    let default_currency = body
        .default_currency
        .as_deref()
        .map(Currency::parse)
        .transpose()
        .map_err(payments_problem)?;

    let connection = state
        .create_connection
        .execute(StartConnectionCommand {
            tenant_id,
            provider,
            country,
            default_currency,
            display_name: Some(tenant.0.display_name.clone()),
            idempotency_key,
            actor: subject.0.user_id,
            ip: client_ip(&req),
        })
        .await
        .map_err(payments_problem)?;

    state
        .metrics
        .connection_creates
        .with_label_values(&["ok"])
        .inc();
    Ok(HttpResponse::Created().json(ConnectionView::of(&connection)))
}

/// `POST /api/v1/tenant/payments/connections/{id}/session` — mint a
/// short-lived Account Session for the embedded onboarding component.
/// Fetched per render: never persisted, never logged, single-render;
/// refused outright for a disconnected connection.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/payments/connections/{id}/session",
    tag = "payments",
    operation_id = "create_payment_connection_session",
    params(
        ("id" = Uuid, Path, description = "The connection id"),
        ("Idempotency-Key" = uuid::Uuid, Header, description = "Required on every mutating payments endpoint; sessions are minted fresh per call")
    ),
    responses(
        (status = 201, description = "Account Session client secret + enabled components", body = ConnectionSessionView),
        (status = 400, description = "Missing/malformed Idempotency-Key", body = ProblemDetail),
        (status = 403, description = "Missing payments.manage permission or the entitlement", body = ProblemDetail),
        (status = 404, description = "No such connection for this tenant", body = ProblemDetail),
        (status = 409, description = "Connection is disconnected or rejected (payments/connection-not-active)", body = ProblemDetail),
        (status = 502, description = "Provider unavailable (payments/provider-unavailable)", body = ProblemDetail)
    )
)]
pub async fn create_payment_connection_session(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    path: web::Path<Uuid>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    idempotency_key(&req)?;
    let connection = load_connection(&state, tenant_id, path.into_inner()).await?;

    let (session, _transitioned) = state
        .account_session
        .execute(subject.0.user_id, client_ip(&req), tenant_id, &connection)
        .await
        .map_err(payments_problem)?;

    state
        .metrics
        .account_sessions
        .with_label_values(&["ok"])
        .inc();
    Ok(HttpResponse::Created()
        .insert_header(("cache-control", "no-store"))
        .json(ConnectionSessionView {
            connection_id: connection.id,
            provider: connection.provider.as_str().to_string(),
            client_secret: session.client_secret,
            expires_at: session.expires_at,
            components: vec!["account_onboarding".to_string()],
        }))
}

/// `GET /api/v1/tenant/payments/connections/{id}` — the connection's
/// status, capabilities and outstanding requirements (thin until 09.3's
/// webhooks make it truthful; the shape is contract-stable now).
#[utoipa::path(
    get,
    path = "/api/v1/tenant/payments/connections/{id}",
    tag = "payments",
    operation_id = "get_payment_connection",
    params(
        ("id" = Uuid, Path, description = "The connection id")
    ),
    responses(
        (status = 200, description = "Status, capabilities, requirements and the server-computed can_accept_payments", body = ConnectionView),
        (status = 403, description = "Missing payments.read permission or the entitlement", body = ProblemDetail),
        (status = 404, description = "No such connection for this tenant", body = ProblemDetail)
    )
)]
pub async fn get_payment_connection(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    _subject: CurrentSubject,
    path: web::Path<Uuid>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    let connection = load_connection(&state, tenant.0.tenant_id, path.into_inner()).await?;
    Ok(HttpResponse::Ok().json(ConnectionView::of(&connection)))
}

/// RLS-scoped read: a connection id another tenant sent resolves to 404,
/// not to someone else's row.
async fn load_connection(
    state: &PaymentsState,
    tenant_id: Uuid,
    connection_id: Uuid,
) -> Result<ProviderConnection, actix_web::Error> {
    let mut scope = state
        .tx
        .begin_for(tenant_id)
        .await
        .map_err(|err| payments_problem(PaymentsError::Storage(err.to_string())))?;
    state
        .connections
        .find_by_id(&mut scope, tenant_id, connection_id)
        .await
        .map_err(payments_problem)?
        .ok_or_else(|| {
            actix_web::Error::from(ApiError::NotFound {
                resource: format!("connection {connection_id}"),
            })
        })
}
