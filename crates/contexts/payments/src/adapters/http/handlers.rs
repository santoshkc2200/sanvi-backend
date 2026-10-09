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
use sanvi_kernel::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::adapters::http::PaymentsState;
use crate::application::{ProvidersView, StartConnectionCommand};
use crate::config::PaymentsConfig;
use crate::domain::checkout::CheckoutId;
use crate::domain::connection::{ConnectionId, ProviderConnection};
use crate::domain::dispute::DisputeId;
use crate::domain::error::PaymentsError;
use crate::domain::money::Currency;
use crate::domain::payment::{PaymentId, RefundId};
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
        // A redirect target off the tenant's verified-domain allow-list:
        // a client mistake, stated plainly (the detail names only what
        // was rejected, never an internal host list).
        PaymentsError::DomainNotAllowed(detail) => ApiError::BadRequest { detail },
        // Checkout-creation flooding — ours to stop, per tenant / IP /
        // customer reference.
        PaymentsError::RateLimited(_) => ApiError::TooManyRequests {
            retry_after_secs: 60,
        },
        PaymentsError::RefundExceedsRemaining {
            requested_minor,
            requested_currency,
            remaining_minor,
            remaining_currency,
        } => ApiError::BadRequest {
            detail: format!(
                "refund of {requested_minor} {requested_currency} exceeds remaining refundable {remaining_minor} {remaining_currency}"
            ),
        },
        PaymentsError::CapabilityMissing { capability, detail } => ApiError::Conflict {
            detail: format!("capability {capability} is not active: {detail}"),
        },
        // The tax preflight's specific refusal: `reason` is a stable code
        // (`tax_status_not_active` / `no_active_registration`) the UI
        // renders next to the disabled toggle.
        PaymentsError::TaxPreflightFailed { reason } => ApiError::Conflict {
            detail: format!(
                "automatic tax cannot be enabled yet ({reason}); registration and remittance are your responsibility — consult your tax advisor"
            ),
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
            ApiError::from_storage_detail(&detail)
        }
        PaymentsError::IdGeneration(detail) => {
            tracing::error!(error = %detail, "payments id generation failure");
            ApiError::internal("id generation failure")
        }
        // Normally intercepted in `payments_problem` with its structured
        // blocker body; the bare mapping keeps this match exhaustive.
        PaymentsError::DisconnectBlocked { blockers } => ApiError::Conflict {
            detail: format!(
                "disconnect is blocked while: {}",
                blockers
                    .iter()
                    .map(|b| b.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    }
}

/// Domain error → actix error whose problem body carries the stable
/// `payments/*` type slug. The shared RFC 9457 layer preserves an
/// already-well-formed problem body, so the slug reaches the client.
pub fn payments_problem(err: PaymentsError) -> actix_web::Error {
    // The disconnect refusal is richer than a bare conflict: its body
    // names the blockers (`in_flight_payments`, `open_disputes`,
    // `pending_payouts`) as RFC 9457 extension members, so the settings
    // page can tell the tenant exactly what to wait for.
    if let PaymentsError::DisconnectBlocked { blockers } = &err {
        let api = ApiError::Conflict {
            detail: err.to_string(),
        };
        let mut problem = api.problem();
        problem.r#type = format!("{PROBLEM_TYPE_BASE}/{}", err.type_slug());
        problem.extensions = Some(
            [(
                "blockers".to_string(),
                serde_json::to_value(
                    blockers
                        .iter()
                        .map(|b| DisconnectBlockerView {
                            code: b.as_str().to_string(),
                            summary_key: b.summary_key().to_string(),
                        })
                        .collect::<Vec<_>>(),
                )
                .unwrap_or(serde_json::Value::Array(vec![])),
            )]
            .into_iter()
            .collect(),
        );
        return actix_web::error::InternalError::from_response(
            err,
            HttpResponse::build(api.status()).json(problem),
        )
        .into();
    }
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

/// One outstanding provider requirement, with the i18n message key the
/// frontend renders it from. The mapping is data
/// (`payments.requirement_keys`); a code the map has not learned falls
/// back to `payments.req.unmapped`, whose catalog entry shows the raw code
/// plus Stripe's requirements help link — new requirements degrade,
/// never break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RequirementEntryView {
    /// The provider's requirement code, verbatim.
    #[schema(example = "individual.verification.document")]
    pub code: String,
    /// The i18n key to render (`payments.req.*`).
    #[schema(example = "payments.req.id_document")]
    pub summary_key: String,
}

/// The connection's outstanding-requirements summary, projected with
/// render keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RequirementsView {
    pub currently_due: Vec<RequirementEntryView>,
    pub eventually_due: Vec<RequirementEntryView>,
    pub past_due: Vec<RequirementEntryView>,
    #[schema(value_type = Option<String>)]
    pub deadline: Option<DateTime<Utc>>,
}

/// One reason the connection cannot take payments yet, keyed for i18n.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct BlockerView {
    #[schema(example = "payments.blocker.card_payments_inactive")]
    pub summary_key: String,
}

/// A tenant's connection to a payment provider: identifiers and status
/// only — no PII, no credentials (the session secret is a separate,
/// never-persisted response).
#[derive(Debug, Clone, Serialize, ToSchema)]
#[schema(as = PaymentsConnectionView)]
pub struct ConnectionView {
    #[schema(value_type = String)]
    pub id: ConnectionId,
    #[schema(example = "stripe_connect")]
    pub provider: String,
    #[schema(example = "onboarding")]
    pub status: String,
    /// The capability map as flat `name → status` strings
    /// (`{"card_payments": "active"}`), normalised from the provider's
    /// stored JSON (flat or v2-nested).
    pub capabilities: Value,
    /// Outstanding requirements with their i18n render keys and deadline.
    pub requirements: RequirementsView,
    /// Why checkout is blocked right now, if it is — server-computed,
    /// ordered, empty once active with nothing past due.
    pub blockers: Vec<BlockerView>,
    pub country: Option<String>,
    pub default_currency: Option<String>,
    #[schema(value_type = Option<String>)]
    pub connected_at: Option<DateTime<Utc>>,
    #[schema(value_type = Option<String>)]
    pub last_synced_at: Option<DateTime<Utc>>,
    /// Server-computed from the v2 `card_payments` capability — never a
    /// deprecated boolean. The frontend renders this; it never recomputes
    /// readiness from capability strings (one definition, one place).
    pub can_accept_payments: bool,
}

impl ConnectionView {
    pub fn of(connection: &ProviderConnection, config: &PaymentsConfig) -> Self {
        let capability_set = connection.capability_set();
        let can_accept_payments = capability_set.is_card_payments_active();
        let summary = connection.requirement_summary();

        let entry = |code: &str| RequirementEntryView {
            code: code.to_string(),
            summary_key: config
                .requirement_keys
                .get(code)
                .cloned()
                .unwrap_or_else(|| crate::config::UNMAPPED_REQUIREMENT_KEY.to_string()),
        };
        let list = |codes: &[String]| codes.iter().map(|c| entry(c)).collect();
        let requirements = RequirementsView {
            currently_due: list(&summary.currently_due),
            eventually_due: list(&summary.eventually_due),
            past_due: list(&summary.past_due),
            deadline: summary.deadline,
        };

        // Blockers are derived here, in one place, in stable order.
        let mut blockers = Vec::new();
        match connection.status {
            crate::domain::ConnectionStatus::Pending => {
                blockers.push(BlockerView {
                    summary_key: "payments.blocker.onboarding_not_started".to_string(),
                });
            }
            crate::domain::ConnectionStatus::Onboarding
            | crate::domain::ConnectionStatus::Restricted
                if !can_accept_payments =>
            {
                blockers.push(BlockerView {
                    summary_key: "payments.blocker.card_payments_inactive".to_string(),
                });
            }
            _ => {}
        }
        if !summary.past_due.is_empty() {
            blockers.push(BlockerView {
                summary_key: "payments.blocker.requirements_past_due".to_string(),
            });
        }

        Self {
            id: connection.id,
            provider: connection.provider.as_str().to_string(),
            status: connection.status.as_str().to_string(),
            capabilities: capability_set.to_provider_json(),
            requirements,
            blockers,
            country: connection.country.clone(),
            default_currency: connection.default_currency.clone(),
            connected_at: connection.connected_at,
            last_synced_at: connection.last_synced_at,
            can_accept_payments,
        }
    }
}

/// `POST /api/v1/tenant/payments/connections` — start (or resume) a
/// connection. Idempotent per `Idempotency-Key`: a retry returns the
/// same connection; a crash between provider-side creation and
/// persistence adopts the same account.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[schema(as = PaymentsCreateConnectionRequest)]
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
#[derive(Clone, Serialize, ToSchema)]
pub struct ConnectionSessionView {
    #[schema(value_type = String)]
    pub connection_id: ConnectionId,
    #[schema(example = "stripe_connect")]
    pub provider: String,
    /// The provider's embedded-components client secret. Handle like a
    /// credential: no caching, no logging, one render.
    pub client_secret: String,
    #[schema(value_type = String)]
    pub expires_at: DateTime<Utc>,
    /// The embedded components enabled on this session:
    /// `account_onboarding`, `notification_banner`, and `account_management`.
    pub components: Vec<String>,
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `GET /api/v1/tenant/payments/providers`.
///
/// Gated two ways: the `payments.enabled` phase flag (route absent when off)
/// and `payments.read` (permission extractor). Deliberately **not** gated on
/// the `payments.stripe_connect` entitlement, unlike every other payments
/// route — the catalog is how an un-entitled tenant learns which providers
/// exist and therefore what upgrading would buy; the admin renders the cards
/// with the connect CTA replaced by an upgrade prompt. Connecting
/// (`POST /payments/connections`) keeps the entitlement.
///
/// A 403 here therefore means the caller lacks `payments.read` — a
/// permission problem, not a plan problem. The frontend must not answer it
/// with an upgrade prompt.
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
        (status = 403, description = "Missing payments.read permission (the catalog is not entitlement-gated)", body = ProblemDetail)
    )
)]
pub async fn list_payment_providers(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
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
    Ok(HttpResponse::Created().json(ConnectionView::of(&connection, &state.config)))
}

/// `POST /api/v1/tenant/payments/connections/{id}/session` — mint an
/// Account Session client secret for the embedded onboarding component.
///
/// Fetched per render: never persisted, never logged, single-render;
/// refused outright for a disconnected connection.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/payments/connections/{id}/session",
    tag = "payments",
    operation_id = "create_payment_connection_session",
    params(
        ("id" = String, Path, description = "The connection id"),
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
    path: web::Path<ConnectionId>,
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
            components: vec![
                "account_onboarding".to_string(),
                "notification_banner".to_string(),
                "account_management".to_string(),
            ],
        }))
}

/// One reason a disconnect is refused: the stable code the API contract
/// fixes plus the i18n key the settings page renders it from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct DisconnectBlockerView {
    #[schema(example = "in_flight_payments")]
    pub code: String,
    #[schema(example = "payments.blocker.in_flight_payments")]
    pub summary_key: String,
}

/// `GET /api/v1/tenant/payments/connections/current` - connection-id
/// recovery (frontend TASK-008). The SPA persists the connection id after
/// `POST /connections`, but a cleared store, a second tab, or a fresh login
/// loses it; this returns the tenant's most recent still-connected row
/// (`disconnected_at IS NULL`, most recent first) or 404 when the tenant
/// has no current connection. Registered before `/{id}` so `current` is
/// never parsed as an id.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/payments/connections/current",
    tag = "payments",
    operation_id = "get_current_payment_connection",
    responses(
        (status = 200, description = "The tenant's current connection, most recent still-connected row", body = ConnectionView),
        (status = 403, description = "Missing payments.read permission or the entitlement", body = ProblemDetail),
        (status = 404, description = "No current connection for this tenant", body = ProblemDetail)
    )
)]
pub async fn get_current_payment_connection(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    _subject: CurrentSubject,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    let mut scope = state
        .tx
        .begin_for(tenant.0.tenant_id.as_i64())
        .await
        .map_err(|err| payments_problem(PaymentsError::Storage(err.to_string())))?;
    let connections = state
        .connections
        .list(&mut scope, tenant.0.tenant_id)
        .await
        .map_err(payments_problem)?;
    let _ = scope.rollback().await;
    let current = connections
        .into_iter()
        .find(|c| c.disconnected_at.is_none());
    match current {
        Some(connection) => {
            Ok(HttpResponse::Ok().json(ConnectionView::of(&connection, &state.config)))
        }
        None => Err(actix_web::Error::from(ApiError::NotFound {
            resource: "connection".to_string(),
        })),
    }
}

/// `GET /api/v1/tenant/payments/connections/{id}` — the connection's
/// status, capabilities, outstanding requirements (with i18n render keys
/// and deadline), blockers, and the server-computed `can_accept_payments`
/// (FR-903). Truthful because 09.3's webhooks and reconciliation job keep
/// the row current.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/payments/connections/{id}",
    tag = "payments",
    operation_id = "get_payment_connection",
    params(
        ("id" = String, Path, description = "The connection id")
    ),
    responses(
        (status = 200, description = "Status, capabilities, requirements, blockers and the server-computed can_accept_payments", body = ConnectionView),
        (status = 403, description = "Missing payments.read permission or the entitlement", body = ProblemDetail),
        (status = 404, description = "No such connection for this tenant", body = ProblemDetail)
    )
)]
pub async fn get_payment_connection(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    _subject: CurrentSubject,
    path: web::Path<ConnectionId>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    let connection = load_connection(&state, tenant.0.tenant_id, path.into_inner()).await?;
    Ok(HttpResponse::Ok().json(ConnectionView::of(&connection, &state.config)))
}

/// `DELETE /api/v1/tenant/payments/connections/{id}` — disconnect the
/// tenant's payment provider (09.7). Local semantics only: the row is
/// marked `Disconnected`, new checkouts stop, every historical record
/// stays. The provider account itself is never closed — it is the
/// tenant's account to close in their own Dashboard.
///
/// Refused with `409` and a structured blocker list (`in_flight_payments`,
/// `open_disputes`, `pending_payouts` as problem-extension members) while
/// money could still move. Idempotent: a retried DELETE of an already-
/// disconnected connection answers 200 from the record. Reconnecting
/// later creates a *new* connection; history stays attached to this one.
#[utoipa::path(
    delete,
    path = "/api/v1/tenant/payments/connections/{id}",
    tag = "payments",
    operation_id = "delete_payment_connection",
    params(
        ("id" = String, Path, description = "The connection id"),
        ("Idempotency-Key" = uuid::Uuid, Header, description = "Required on every mutating payments endpoint")
    ),
    responses(
        (status = 200, description = "Disconnected (or already disconnected: idempotent replay)", body = ConnectionView),
        (status = 400, description = "Missing/malformed Idempotency-Key", body = ProblemDetail),
        (status = 403, description = "Missing payments.manage permission or the payments.stripe_connect entitlement", body = ProblemDetail),
        (status = 404, description = "No such connection for this tenant", body = ProblemDetail),
        (status = 409, description = "Money is still moving on this connection; the problem body names the blockers as extension members: blockers: [{code, summary_key}] with code in {in_flight_payments, open_disputes, pending_payouts} (payments/disconnect-blocked)", body = ProblemDetail)
    )
)]
pub async fn delete_payment_connection(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    path: web::Path<ConnectionId>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    idempotency_key(&req)?;
    let connection_id = path.into_inner();

    // 404 semantics live here (the use case re-checks inside its
    // transaction); everything else belongs to the use case.
    load_connection(&state, tenant_id, connection_id).await?;

    let connection = state
        .disconnect_connection
        .execute(crate::application::DisconnectCommand {
            tenant_id,
            connection_id,
            actor: subject.0.user_id,
            ip: client_ip(&req),
        })
        .await;

    match connection {
        Ok(connection) => {
            state
                .metrics
                .connection_disconnects
                .with_label_values(&["ok"])
                .inc();
            Ok(HttpResponse::Ok().json(ConnectionView::of(&connection, &state.config)))
        }
        Err(err) => {
            if let PaymentsError::DisconnectBlocked { blockers } = &err {
                for blocker in blockers {
                    state
                        .metrics
                        .connection_disconnects
                        .with_label_values(&[blocker.as_str()])
                        .inc();
                }
            }
            Err(payments_problem(err))
        }
    }
}

/// `POST /api/v1/tenant/checkout` — create a checkout session on the
/// tenant's connected account (09.4, direct charges). Idempotency-Key is
/// required: a double-clicked buy button must not create two sessions.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateCheckoutRequest {
    /// The merchant's order reference, echoed on every read model.
    #[schema(example = "order-2026-0821")]
    pub reference: String,
    /// Minor units — ¥2,000 is `2000` (JPY is zero-decimal).
    #[schema(example = 2000)]
    pub amount_minor: i64,
    /// ISO-4217 code; must match the connected account's settlement
    /// currency.
    #[schema(example = "JPY")]
    pub currency: String,
    /// Free-form customer/order context (provider-passthrough; no card
    /// data ever). Rate limiting buckets one key per distinct value.
    #[schema(value_type = Option<Object>)]
    pub customer_ref: Option<Value>,
    /// Presentation locale for the hosted page (`ja`, `en-US`, …).
    pub locale: Option<String>,
    /// Redirect target on a **verified** tenant domain (phase 08). A
    /// success URL on any other host is rejected outright.
    #[schema(example = "https://shop.tenant.test/thanks")]
    pub success_url: String,
    #[schema(example = "https://shop.tenant.test/cart")]
    pub cancel_url: String,
}

/// The checkout as the storefront sees it. `status` is the only source
/// the confirmation page trusts: derived from webhook-recorded truth,
/// never from the redirect.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CheckoutView {
    #[schema(value_type = String)]
    pub id: CheckoutId,
    /// `pending` | `paid` | `failed` | `expired` | `canceled`.
    #[schema(example = "pending")]
    pub status: String,
    pub reference: String,
    #[schema(example = 2000)]
    pub amount_minor: i64,
    #[schema(example = "JPY", value_type = String)]
    pub currency: String,
    #[schema(value_type = Option<Object>)]
    pub customer_ref: Option<Value>,
    /// The provider-hosted payment URL, while the checkout can still be
    /// paid. Absent once terminal.
    #[schema(example = "https://checkout.stripe.com/c/pay/cs_…", value_type = Option<String>)]
    pub url: Option<String>,
    #[schema(value_type = Option<String>)]
    pub expires_at: Option<DateTime<Utc>>,
    /// Server-issued exactly once per order when paid; identical on every
    /// refresh of the confirmation page, so phase 10's conversion
    /// pipeline cannot double-count.
    /// The provider's decline/failure code for the newest failed attempt
    /// (`card_declined`, `insufficient_funds`, …), so the storefront can
    /// tell a buyer *why*. The provider's own message is deliberately not
    /// exposed: the storefront renders its own localized copy.
    #[schema(example = "card_declined")]
    pub failure_code: Option<String>,
    #[schema(value_type = Option<String>)]
    pub conversion_event_id: Option<PaymentId>,
    #[schema(value_type = String)]
    pub created_at: DateTime<Utc>,
}

/// The public status vocabulary mapped from webhook-recorded truth:
/// lifecycle state plus the newest recorded payment attempt ("failed"
/// while the checkout can still be retried at the hosted page).
fn public_status(
    checkout: &crate::domain::Checkout,
    latest: Option<&crate::domain::PaymentRecord>,
) -> &'static str {
    use crate::domain::CheckoutStatus;
    use crate::domain::payment::PaymentStatus;
    match checkout.status {
        CheckoutStatus::Complete => "paid",
        CheckoutStatus::Expired => "expired",
        CheckoutStatus::Canceled => "canceled",
        CheckoutStatus::Pending | CheckoutStatus::Open => {
            if latest.is_some_and(|p| p.status == PaymentStatus::Failed) {
                "failed"
            } else {
                "pending"
            }
        }
    }
}

async fn load_checkout(
    state: &PaymentsState,
    tenant_id: TenantId,
    checkout_id: CheckoutId,
) -> Result<
    (
        crate::domain::Checkout,
        Option<crate::domain::PaymentRecord>,
    ),
    actix_web::Error,
> {
    let mut scope = state
        .tx
        .begin_for(tenant_id.as_i64())
        .await
        .map_err(|err| payments_problem(PaymentsError::Storage(err.to_string())))?;
    let checkout = state
        .checkouts
        .find_by_id(&mut scope, tenant_id, checkout_id)
        .await
        .map_err(payments_problem)?
        .ok_or_else(|| {
            actix_web::Error::from(ApiError::NotFound {
                resource: format!("checkout {checkout_id}"),
            })
        })?;
    let latest = state
        .checkouts
        .latest_payment_for_checkout(&mut scope, tenant_id, checkout_id)
        .await
        .map_err(payments_problem)?;
    let _ = scope.rollback().await;
    Ok((checkout, latest))
}

/// The newest payment recorded against a checkout (RLS-scoped read).
async fn load_latest_payment(
    state: &PaymentsState,
    tenant_id: TenantId,
    checkout_id: CheckoutId,
) -> Result<Option<crate::domain::PaymentRecord>, actix_web::Error> {
    let mut scope = state
        .tx
        .begin_for(tenant_id.as_i64())
        .await
        .map_err(|err| payments_problem(PaymentsError::Storage(err.to_string())))?;
    let latest = state
        .checkouts
        .latest_payment_for_checkout(&mut scope, tenant_id, checkout_id)
        .await
        .map_err(payments_problem);
    let _ = scope.rollback().await;
    latest
}

/// The confirmation payload for a checkout + its newest payment record.
fn checkout_view(
    tenant_id: TenantId,
    checkout: &crate::domain::Checkout,
    latest: Option<&crate::domain::PaymentRecord>,
    url: Option<String>,
) -> CheckoutView {
    let status = public_status(checkout, latest);
    // Issued server-side and derivable forever from (tenant, payment):
    // the refreshed-confirmation-page dedupe phase 10 leans on.
    let conversion_event_id = (status == "paid")
        .then(|| {
            latest.map(|payment| crate::application::conversion_event_id(tenant_id, payment.id))
        })
        .flatten();
    CheckoutView {
        id: checkout.id,
        status: status.to_string(),
        reference: checkout.reference.as_str().to_string(),
        amount_minor: checkout.amount.amount_minor,
        currency: checkout.amount.currency.as_str().to_string(),
        customer_ref: checkout.customer_ref.clone(),
        url: url.filter(|_| {
            matches!(
                checkout.status,
                crate::domain::CheckoutStatus::Pending | crate::domain::CheckoutStatus::Open
            )
        }),
        expires_at: checkout.expires_at,
        failure_code: (status == "failed")
            .then(|| latest.and_then(|payment| payment.failure.as_ref()))
            .flatten()
            .map(|failure| failure.code.clone()),
        conversion_event_id,
        created_at: checkout.created_at,
    }
}

/// Throttle bucket keys for one create request: per tenant, per client
/// IP, per customer reference. Customer refs hash so arbitrary JSON never
/// becomes a map key of unbounded size.
fn throttle_buckets(
    tenant_id: TenantId,
    ip: Option<IpAddr>,
    customer_ref: Option<&Value>,
) -> Vec<String> {
    use sha2::{Digest, Sha256};
    let mut buckets = vec![format!("checkout:t:{tenant_id}")];
    if let Some(ip) = ip {
        buckets.push(format!("checkout:ip:{ip}"));
    }
    if let Some(customer_ref) = customer_ref {
        let digest = Sha256::digest(customer_ref.to_string().as_bytes());
        buckets.push(format!("checkout:c:{}", hex::encode(&digest[..8])));
    }
    buckets
}

/// The flag-off answer shared by both checkout endpoints: an honest
/// 503 carrying the context's `payments/provider-unavailable` problem
/// type, so the frontend renders "temporarily unavailable" rather than a
/// dead route. Webhook handlers stay deployed while this is on the wire.
fn checkout_disabled() -> actix_web::Error {
    let status = ApiError::ServiceUnavailable {
        detail: String::new(),
    }
    .status();
    let mut problem = ApiError::ServiceUnavailable {
        detail: "checkout is temporarily unavailable".to_string(),
    }
    .problem();
    problem.r#type = format!(
        "{PROBLEM_TYPE_BASE}/{}",
        PaymentsError::ProviderUnavailable(String::new()).type_slug()
    );
    actix_web::error::InternalError::from_response(
        "checkout disabled",
        HttpResponse::build(status).json(problem),
    )
    .into()
}

/// What the storefront needs before it can render a Pay button:
/// readiness and the settlement currency. Derived from the tenant's live
/// connection by the same gate the checkout itself applies.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CheckoutConfigView {
    /// Server-computed from the v2 `card_payments` capability, exactly as
    /// `POST /tenant/checkout` computes it. The storefront renders this;
    /// it never recomputes readiness of its own.
    pub can_accept_payments: bool,
    /// The i18n key for *why* not (`payments.blocker.*`), so the
    /// storefront can show a reason next to a disabled button instead of
    /// waiting for a 409. `null` when payments are accepted.
    #[schema(example = "payments.blocker.card_payments_inactive")]
    pub cannot_accept_reason: Option<String>,
    /// The connected account's settlement currency. The storefront must
    /// price in this currency; anything else is refused at creation.
    #[schema(example = "JPY")]
    pub currency: Option<String>,
    /// Whether a checkout started now would have automatic tax applied,
    /// read from the tenant's stored tax settings by the same rule the
    /// session creation applies. The storefront renders its tax note from
    /// this; it never assumes tax is on.
    pub tax_enabled: bool,
}

/// `GET /api/v1/tenant/checkout/config` — the storefront's pre-flight.
/// Same permission and entitlement as the buy path it guards, because it
/// answers the same question the buy path would refuse on.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/checkout/config",
    tag = "payments",
    operation_id = "get_tenant_checkout_config",
    responses(
        (status = 200, description = "Whether the tenant's connected account can accept payments, why not, its settlement currency, and whether automatic tax applies", body = CheckoutConfigView),
        (status = 403, description = "Missing payments.checkout permission or the entitlement", body = ProblemDetail),
        (status = 503, description = "The payments.checkout flag is off (payments/provider-unavailable)", body = ProblemDetail)
    )
)]
pub async fn get_tenant_checkout_config(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    if !state.config.checkout_enabled {
        return Err(checkout_disabled());
    }

    let readiness = state
        .create_checkout
        .readiness(tenant.0.tenant_id)
        .await
        .map_err(payments_problem)?;

    Ok(HttpResponse::Ok()
        .insert_header(("cache-control", "no-store"))
        .json(CheckoutConfigView {
            can_accept_payments: readiness.can_accept_payments,
            cannot_accept_reason: readiness.cannot_accept_reason,
            currency: readiness.currency,
            tax_enabled: readiness.tax_enabled,
        }))
}

/// `POST /api/v1/tenant/checkout` — start a purchase on the tenant's own
/// connected account. Direct charge: money lands in the **tenant's**
/// Stripe balance; nothing transits Sanvi. The response URL is a hint —
/// the confirmation page trusts only `GET /tenant/checkout/{id}`.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/checkout",
    tag = "payments",
    operation_id = "create_tenant_checkout",
    request_body = CreateCheckoutRequest,
    responses(
        (status = 201, description = "Session created on the connected account", body = CheckoutView),
        (status = 200, description = "Idempotent replay: the same checkout the Idempotency-Key already owns", body = CheckoutView),
        (status = 400, description = "Malformed request, non-positive amount, or a redirect URL off the verified-domain allow-list (payments/domain-not-allowed)", body = ProblemDetail),
        (status = 403, description = "Missing payments.checkout permission or the payments.stripe_connect entitlement", body = ProblemDetail),
        (status = 409, description = "The connected account cannot accept payments (payments/connection-not-active)", body = ProblemDetail),
        (status = 429, description = "Rate limit exceeded per tenant, client or customer reference (payments/rate-limited)", body = ProblemDetail),
        (status = 502, description = "Provider unavailable (payments/provider-unavailable)", body = ProblemDetail),
        (status = 503, description = "The payments.checkout flag is off (payments/provider-unavailable)", body = ProblemDetail)
    ),
    params(
        ("Idempotency-Key" = uuid::Uuid, Header, description = "Required. The same key returns the same checkout — one double-clicked buy button, one session")
    )
)]
pub async fn create_tenant_checkout(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    req: HttpRequest,
    body: web::Json<CreateCheckoutRequest>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    // Rollback switch: flag off → honest 503 with the context's problem
    // type. Webhook handlers stay deployed regardless, so money taken at
    // Stripe is never without a local record.
    if !state.config.checkout_enabled {
        return Err(checkout_disabled());
    }

    let tenant_id = tenant.0.tenant_id;
    let idempotency_key = idempotency_key(&req)?;
    let ip = client_ip(&req);

    // Abuse control before any database or provider work: session-
    // creation flooding is ours to stop.
    for bucket in throttle_buckets(tenant_id, ip, body.customer_ref.as_ref()) {
        state
            .checkout_throttle
            .acquire(&bucket)
            .await
            .map_err(payments_problem)?;
    }

    let outcome = state
        .create_checkout
        .execute(crate::application::NewCheckoutCommand {
            tenant_id,
            reference: body.reference.clone(),
            amount_minor: body.amount_minor,
            currency: body.currency.clone(),
            customer_ref: body.customer_ref.clone(),
            locale: body.locale.clone(),
            success_url: body.success_url.clone(),
            cancel_url: body.cancel_url.clone(),
            idempotency_key,
        })
        .await
        .map_err(payments_problem)?;

    // A replayed key whose order already closed answers from recorded
    // truth (status + conversion id), exactly as GET would.
    let latest = if outcome.replayed {
        load_latest_payment(&state, tenant_id, outcome.checkout.id).await?
    } else {
        None
    };

    let view = checkout_view(tenant_id, &outcome.checkout, latest.as_ref(), outcome.url);
    state
        .metrics
        .checkout_creates
        .with_label_values(&[if outcome.replayed {
            "replay"
        } else {
            "created"
        }])
        .inc();
    if outcome.replayed {
        Ok(HttpResponse::Ok().json(view))
    } else {
        Ok(HttpResponse::Created().json(view))
    }
}

/// `GET /api/v1/tenant/checkout/{id}` — the confirming state. Polled by
/// the confirmation page and trusted over any redirect: a customer who
/// never returns still ends with a paid order here, a forged return URL
/// yields nothing.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/checkout/{id}",
    tag = "payments",
    operation_id = "get_tenant_checkout",
    params(
        ("id" = String, Path, description = "The checkout id returned by POST /api/v1/tenant/checkout")
    ),
    responses(
        (status = 200, description = "The webhook-truthful checkout state (the only thing the confirmation page trusts)", body = CheckoutView),
        (status = 403, description = "Missing payments.checkout permission or the entitlement", body = ProblemDetail),
        (status = 404, description = "No such checkout for this tenant", body = ProblemDetail),
        (status = 503, description = "The payments.checkout flag is off (payments/provider-unavailable)", body = ProblemDetail)
    )
)]
pub async fn get_tenant_checkout(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    path: web::Path<CheckoutId>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    if !state.config.checkout_enabled {
        return Err(checkout_disabled());
    }

    let tenant_id = tenant.0.tenant_id;
    let (checkout, latest) = load_checkout(&state, tenant_id, path.into_inner()).await?;

    // While the checkout can still be paid, hand back its hosted URL:
    // re-read from the provider so a replayed confirmation link works,
    // never minting anything. Terminal checkouts have nothing to say.
    let url = if checkout.status.is_live() {
        match checkout.external_session_id.as_deref() {
            Some(session_id) => hosted_url(&state, tenant_id, &checkout, session_id).await,
            None => None,
        }
    } else {
        None
    };

    Ok(HttpResponse::Ok()
        .insert_header(("cache-control", "no-store"))
        .json(checkout_view(tenant_id, &checkout, latest.as_ref(), url)))
}

/// Re-fetch the provider-hosted payment URL for a live checkout.
/// Best-effort: a retrieval failure degrades to `None` (the confirmation
/// page still gets truthful status), never to an invented link.
async fn hosted_url(
    state: &PaymentsState,
    tenant_id: TenantId,
    checkout: &crate::domain::Checkout,
    session_id: &str,
) -> Option<String> {
    let mut scope = state.tx.begin_for(tenant_id.as_i64()).await.ok()?;
    let connection = state
        .connections
        .find_by_id(&mut scope, tenant_id, checkout.connection_id)
        .await
        .ok()
        .flatten();
    let _ = scope.rollback().await;
    let connection = connection?;
    let adapter = state.registry.get(connection.provider)?;
    match adapter
        .retrieve_checkout(&connection.external_account_id, session_id)
        .await
    {
        Ok(session) => Some(session.url),
        Err(err) => {
            tracing::warn!(
                checkout_id = %checkout.id,
                error = %err,
                "checkout GET could not refresh its hosted URL"
            );
            None
        }
    }
}

/// RLS-scoped read: a connection id another tenant sent resolves to 404,
/// not to someone else's row.
async fn load_connection(
    state: &PaymentsState,
    tenant_id: TenantId,
    connection_id: ConnectionId,
) -> Result<ProviderConnection, actix_web::Error> {
    let mut scope = state
        .tx
        .begin_for(tenant_id.as_i64())
        .await
        .map_err(|err| payments_problem(PaymentsError::Storage(err.to_string())))?;
    let connection = state
        .connections
        .find_by_id(&mut scope, tenant_id, connection_id)
        .await
        .map_err(payments_problem)?;
    let _ = scope.rollback().await;
    connection.ok_or_else(|| {
        actix_web::Error::from(ApiError::NotFound {
            resource: format!("connection {connection_id}"),
        })
    })
}

// ---------------------------------------------------------------------------
// 09.5: payment / refund / dispute views & handlers
// ---------------------------------------------------------------------------

/// One payment as the tenant sees it in the list.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PaymentView {
    #[schema(value_type = String)]
    pub id: PaymentId,
    pub external_payment_id: String,
    #[schema(value_type = Option<String>)]
    pub checkout_id: Option<CheckoutId>,
    pub status: String,
    pub amount_minor: i64,
    pub currency: String,
    /// The provider-reported fee on this payment. When the platform's
    /// application fee is enabled for the tenant, this is where it is
    /// disclosed — a platform fee the merchant cannot see is not
    /// acceptable, whatever the contract says.
    pub fee_minor: Option<i64>,
    pub fee_currency: Option<String>,
    /// Payout status once payout reconciliation includes this payment
    /// (`pending` | `paid` | `failed`; null until then).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payout_status: Option<String>,
    pub failure_code: Option<String>,
    pub failure_message: Option<String>,
    #[schema(value_type = Option<String>)]
    pub captured_at: Option<DateTime<Utc>>,
    #[schema(value_type = Option<String>)]
    pub created_at: Option<DateTime<Utc>>,
}

impl From<crate::domain::PaymentRecord> for PaymentView {
    fn from(payment: crate::domain::PaymentRecord) -> Self {
        Self {
            id: payment.id,
            external_payment_id: payment.external_payment_id,
            checkout_id: payment.checkout_id,
            status: payment.status.as_str().to_string(),
            amount_minor: payment.amount.amount_minor,
            currency: payment.amount.currency.as_str().to_string(),
            fee_minor: payment.fee.as_ref().map(|f| f.amount_minor),
            fee_currency: payment
                .fee
                .as_ref()
                .map(|f| f.currency.as_str().to_string()),
            payout_status: payment.payout_status.map(|s| s.as_str().to_string()),
            failure_code: payment.failure.as_ref().map(|f| f.code.clone()),
            failure_message: payment.failure.as_ref().map(|f| f.message.clone()),
            captured_at: payment.captured_at,
            created_at: payment.created_at,
        }
    }
}

/// One refund record.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RefundView {
    #[schema(value_type = String)]
    pub id: RefundId,
    #[schema(value_type = String)]
    pub payment_id: PaymentId,
    pub external_refund_id: Option<String>,
    pub amount_minor: i64,
    pub currency: String,
    pub status: String,
    pub reason: Option<String>,
    #[schema(value_type = Option<String>)]
    pub created_by: Option<UserId>,
    #[schema(value_type = String)]
    pub created_at: DateTime<Utc>,
}

impl From<crate::domain::payment::RefundRecord> for RefundView {
    fn from(refund: crate::domain::payment::RefundRecord) -> Self {
        Self {
            id: refund.id,
            payment_id: refund.payment_id,
            external_refund_id: refund.external_refund_id,
            amount_minor: refund.amount.amount_minor,
            currency: refund.amount.currency.as_str().to_string(),
            status: refund.status.as_str().to_string(),
            reason: refund.reason,
            created_by: refund.created_by,
            created_at: refund.created_at,
        }
    }
}

/// One dispute plus its dashboard deep link (read-only).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DisputeView {
    #[schema(value_type = String)]
    pub id: DisputeId,
    #[schema(value_type = String)]
    pub payment_id: PaymentId,
    pub external_dispute_id: String,
    pub status: String,
    pub amount_minor: i64,
    pub currency: String,
    pub reason: Option<String>,
    #[schema(value_type = Option<String>)]
    pub due_by: Option<DateTime<Utc>>,
    pub charge_id: Option<String>,
    pub dashboard_url: Option<String>,
    #[schema(value_type = String)]
    pub created_at: DateTime<Utc>,
}

/// The payment detail the support UI renders: payment + its refunds +
/// disputes + timeline.
///
/// The timeline is a first-class projection derived from stored events
/// (created, succeeded/failed, refunded, disputed, paid out), each with
/// a timestamp and an actor where one exists.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PaymentDetailView {
    pub payment: PaymentView,
    pub refunds: Vec<RefundView>,
    pub disputes: Vec<DisputeView>,
    pub timeline: Vec<crate::domain::timeline::TimelineEntry>,
    pub checkout_reference: Option<String>,
}

/// Cursor-paginated payment list.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PaymentListView {
    pub items: Vec<PaymentView>,
    pub next_cursor: Option<String>,
}

/// Cursor-paginated dispute list.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DisputeListView {
    pub items: Vec<DisputeView>,
    pub next_cursor: Option<String>,
}

/// Query for `GET /api/v1/tenant/payments`.
#[derive(Debug, Clone, Deserialize, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PaymentListQuery {
    /// Filter by payment status.
    pub status: Option<String>,
    /// Filter by payout status.
    pub payout_status: Option<String>,
    /// Filter by currency (ISO-4217).
    pub currency: Option<String>,
    /// Filter by customer reference substring.
    pub customer: Option<String>,
    /// Filter by created_at >= (RFC3339).
    #[serde(default)]
    #[param(value_type = Option<String>)]
    pub date_from: Option<DateTime<Utc>>,
    /// Filter by created_at <= (RFC3339).
    #[serde(default)]
    #[param(value_type = Option<String>)]
    pub date_to: Option<DateTime<Utc>>,
    /// Pagination cursor (opaque).
    #[serde(rename = "after", alias = "cursor")]
    pub cursor: Option<String>,
    /// Page size, clamped to 1..=100.
    #[param(schema_with = payment_limit_schema)]
    pub limit: Option<u32>,
}

/// Query for `GET /api/v1/tenant/payments/disputes`.
#[derive(Debug, Clone, Deserialize, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DisputeListQuery {
    /// Filter by dispute status.
    pub status: Option<String>,
    /// Filter by currency (ISO-4217).
    pub currency: Option<String>,
    /// Filter by created_at >= (RFC3339).
    #[serde(default)]
    #[param(value_type = Option<String>)]
    pub date_from: Option<DateTime<Utc>>,
    /// Filter by created_at <= (RFC3339).
    #[serde(default)]
    #[param(value_type = Option<String>)]
    pub date_to: Option<DateTime<Utc>>,
    /// Pagination cursor (opaque).
    #[serde(rename = "after", alias = "cursor")]
    pub cursor: Option<String>,
    /// Page size, clamped to 1..=100.
    #[param(schema_with = payment_limit_schema)]
    pub limit: Option<u32>,
}

fn payment_limit_schema() -> utoipa::openapi::schema::Object {
    sanvi_http::openapi::page_limit_schema(100, 20).build()
}

#[cfg(test)]
mod pagination_contract_tests {
    use super::*;
    #[test]
    fn canonical_after_and_legacy_cursor_deserialize_identically() {
        for name in ["after", "cursor"] {
            let value = serde_json::json!({name: "opaque-next-page"});
            let payment: PaymentListQuery = serde_json::from_value(value.clone()).unwrap();
            let dispute: DisputeListQuery = serde_json::from_value(value).unwrap();
            assert_eq!(payment.cursor.as_deref(), Some("opaque-next-page"));
            assert_eq!(dispute.cursor.as_deref(), Some("opaque-next-page"));
        }
    }
}

/// Body for `POST /api/v1/tenant/payments/{id}/refund`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RefundRequest {
    /// Minor-unit amount or `null` for a full refund (remaining amount).
    pub amount: Option<RefundAmount>,
    /// Why the refund is being issued — required, audited and shown in
    /// the timeline. Empty is rejected.
    pub reason: String,
    /// Optional free-form note (support context, not the audit reason).
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct RefundAmount {
    #[schema(example = 500)]
    pub minor_units: i64,
    #[schema(example = "JPY", value_type = String)]
    pub currency: String,
}

/// `GET /api/v1/tenant/payments` — every documented filter combinable,
/// cursor-paginated, tenant-scoped by RLS and indexed for the filter set.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/payments",
    tag = "payments",
    operation_id = "list_tenant_payments",
    params(PaymentListQuery),
    responses(
        (status = 200, description = "One page of payments", body = PaymentListView),
        (status = 403, description = "Missing payments.read permission or the entitlement", body = ProblemDetail),
        (status = 400, description = "Malformed filter (unknown status, bad currency)", body = ProblemDetail),
        (status = 504, description = "Statement timeout — the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn list_payments(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    query: web::Query<PaymentListQuery>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    let command = crate::application::ListPaymentsCommand {
        tenant_id: tenant.0.tenant_id,
        status: query.status.clone(),
        payout_status: query.payout_status.clone(),
        currency: query.currency.clone(),
        customer: query.customer.clone(),
        date_from: query.date_from,
        date_to: query.date_to,
        cursor: query.cursor.clone(),
        limit: query.limit,
    };
    let page = state
        .list_payments
        .execute(command)
        .await
        .map_err(payments_problem)?;
    let view = PaymentListView {
        items: page.items.into_iter().map(PaymentView::from).collect(),
        next_cursor: page.next_cursor,
    };
    Ok(HttpResponse::Ok()
        .insert_header(("cache-control", "no-store"))
        .json(view))
}

/// `GET /api/v1/tenant/payments/{id}` — payment detail with timeline,
/// refunds and dispute state.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/payments/{id}",
    tag = "payments",
    operation_id = "get_tenant_payment",
    params(
        ("id" = String, Path, description = "The payment id")
    ),
    responses(
        (status = 200, description = "Payment detail with timeline, refunds and disputes", body = PaymentDetailView),
        (status = 403, description = "Missing payments.read permission or the entitlement", body = ProblemDetail),
        (status = 404, description = "No such payment for this tenant", body = ProblemDetail)
    )
)]
pub async fn get_payment(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    path: web::Path<PaymentId>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let payment_id = path.into_inner();
    let detail = state
        .get_payment
        .execute(tenant_id, payment_id)
        .await
        .map_err(|err| match err {
            PaymentsError::InvalidConnection(detail) if detail.contains("not found") => {
                actix_web::Error::from(ApiError::NotFound {
                    resource: format!("payment {payment_id}"),
                })
            }
            other => payments_problem(other),
        })?;
    // The use case resolves the connected account (the deep-link anchor);
    // a missing link is never a failed detail view.
    let disputes: Vec<DisputeView> = detail
        .disputes
        .into_iter()
        .map(|d| {
            let dashboard_url = detail
                .account_external_id
                .as_deref()
                .and_then(|acct| d.dashboard_url(acct));
            DisputeView {
                id: d.id,
                payment_id: d.payment_id,
                external_dispute_id: d.external_dispute_id,
                status: d.status.as_str().to_string(),
                amount_minor: d.amount.amount_minor,
                currency: d.amount.currency.as_str().to_string(),
                reason: d.reason,
                due_by: d.due_by,
                charge_id: d.charge_id,
                dashboard_url,
                created_at: d.created_at,
            }
        })
        .collect();
    let view = PaymentDetailView {
        payment: PaymentView::from(detail.payment),
        refunds: detail.refunds.into_iter().map(RefundView::from).collect(),
        disputes,
        timeline: detail.timeline.entries,
        checkout_reference: detail.checkout_reference,
    };
    Ok(HttpResponse::Ok()
        .insert_header(("cache-control", "no-store"))
        .json(view))
}

/// `POST /api/v1/tenant/payments/{id}/refund` — refund a payment.
///
/// `Idempotency-Key` is required; the response is the refund record,
/// never a bare 204 — a partial refund of an unexpected amount is a
/// support incident, so the UI must be able to show what actually
/// happened. Arithmetic is in minor units against the already-refunded
/// total; an over-refund is rejected with the remaining refundable
/// amount in the error, not a generic 422. Executed on the connected
/// account — the refund is the tenant's money moving, not ours.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/payments/{id}/refund",
    tag = "payments",
    operation_id = "refund_tenant_payment",
    params(
        ("id" = String, Path, description = "The payment id"),
        ("Idempotency-Key" = Uuid, Header, description = "Required (UUID). The same key with the same amount/reason replays the same refund; a different payload conflicts")
    ),
    request_body = RefundRequest,
    responses(
        (status = 200, description = "Idempotent replay: the same refund the Idempotency-Key already owns", body = RefundView),
        (status = 201, description = "Refund issued on the connected account", body = RefundView),
        (status = 400, description = "Missing reason, malformed amount, over-refund (payments/over-refund reports remaining), currency mismatch", body = ProblemDetail),
        (status = 403, description = "Missing payments.refund permission or the entitlement", body = ProblemDetail),
        (status = 404, description = "No such payment for this tenant", body = ProblemDetail),
        (status = 409, description = "Idempotency-Key reuse with a different amount/reason (payments/idempotency-conflict) or already fully refunded", body = ProblemDetail),
        (status = 502, description = "Provider unavailable (payments/provider-unavailable)", body = ProblemDetail)
    )
)]
pub async fn refund_payment(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    path: web::Path<PaymentId>,
    body: web::Json<RefundRequest>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let payment_id = path.into_inner();
    let idempotency_key = idempotency_key(&req)?;
    let amount = match &body.amount {
        None => None,
        Some(amount) => Some(
            crate::domain::Money::new(amount.minor_units, &amount.currency)
                .map_err(payments_problem)?,
        ),
    };
    let command = crate::application::RefundCommand {
        tenant_id,
        payment_id,
        amount,
        reason: body.reason.clone(),
        note: body.note.clone(),
        idempotency_key,
        actor: subject.0.user_id,
        ip: client_ip(&req),
    };
    let (refund, replayed) =
        state
            .issue_refund
            .execute(command)
            .await
            .map_err(|err| match err {
                PaymentsError::InvalidConnection(detail) if detail.contains("not found") => {
                    actix_web::Error::from(ApiError::NotFound {
                        resource: format!("payment {payment_id}"),
                    })
                }
                other => payments_problem(other),
            })?;
    // The response is the refund record, never a bare 204. 201 issued it,
    // 200 is a replay of the same refund under the same key.
    let status = if replayed {
        actix_web::http::StatusCode::OK
    } else {
        actix_web::http::StatusCode::CREATED
    };
    Ok(HttpResponse::build(status).json(RefundView::from(refund)))
}

/// `GET /api/v1/tenant/payments/disputes` — disputes for this tenant,
/// cursor-paginated.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/payments/disputes",
    tag = "payments",
    operation_id = "list_tenant_disputes",
    params(DisputeListQuery),
    responses(
        (status = 200, description = "One page of disputes", body = DisputeListView),
        (status = 403, description = "Missing payments.read permission or the entitlement", body = ProblemDetail),
        (status = 504, description = "Statement timeout — the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn list_disputes(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    query: web::Query<DisputeListQuery>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    let command = crate::application::ListDisputesCommand {
        tenant_id: tenant.0.tenant_id,
        status: query.status.clone(),
        currency: query.currency.clone(),
        date_from: query.date_from,
        date_to: query.date_to,
        cursor: query.cursor.clone(),
        limit: query.limit,
    };
    let page = state
        .list_disputes
        .execute(command)
        .await
        .map_err(payments_problem)?;
    let items = page
        .items
        .into_iter()
        .map(|item| {
            let d = item.record;
            DisputeView {
                id: d.id,
                payment_id: d.payment_id,
                external_dispute_id: d.external_dispute_id,
                status: d.status.as_str().to_string(),
                amount_minor: d.amount.amount_minor,
                currency: d.amount.currency.as_str().to_string(),
                reason: d.reason,
                due_by: d.due_by,
                charge_id: d.charge_id,
                dashboard_url: item.dashboard_url,
                created_at: d.created_at,
            }
        })
        .collect();
    Ok(HttpResponse::Ok()
        .insert_header(("cache-control", "no-store"))
        .json(DisputeListView {
            items,
            next_cursor: page.next_cursor,
        }))
}

/// `GET /api/v1/tenant/payments/export` — server-side CSV export,
/// tenant-scoped, audited.
///
/// The column set is fixed and documented: `payment_id,external_payment_id,
/// checkout_reference,amount_minor,currency,status,fee_minor,fee_currency,
/// captured_at,created_at,refunded_minor,dispute_status`.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/payments/export",
    tag = "payments",
    operation_id = "export_tenant_payments",
    params(
        ("status" = Option<String>, Query, description = "Filter by payment status"),
        ("payout_status" = Option<String>, Query, description = "Filter by payout status"),
        ("currency" = Option<String>, Query, description = "Filter by currency"),
        ("customer" = Option<String>, Query, description = "Filter by customer reference substring"),
        ("date_from" = Option<String>, Query, description = "Filter by created_at >= (RFC3339)"),
        ("date_to" = Option<String>, Query, description = "Filter by created_at <= (RFC3339)")
    ),
    responses(
        (status = 200, description = "CSV bytes (text/csv)", content_type = "text/csv"),
        (status = 403, description = "Missing payments.read permission or the entitlement", body = ProblemDetail)
    )
)]
pub async fn export_payments(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    query: web::Query<PaymentListQuery>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    let command = crate::application::ExportCommand {
        tenant_id: tenant.0.tenant_id,
        actor: subject.0.user_id,
        ip: client_ip(&req),
        status: query.status.clone(),
        payout_status: query.payout_status.clone(),
        currency: query.currency.clone(),
        customer: query.customer.clone(),
        date_from: query.date_from,
        date_to: query.date_to,
    };
    let stream = state
        .export_payments
        .execute_stream(command)
        .await
        .map_err(payments_problem)?;
    Ok(HttpResponse::Ok()
        .insert_header(("content-type", "text/csv; charset=utf-8"))
        .insert_header((
            "content-disposition",
            "attachment; filename=\"payments.csv\"".to_string(),
        ))
        .insert_header(("cache-control", "no-store"))
        .streaming(stream))
}

// ---------------------------------------------------------------------------
// 09.6: payouts (read-only) and tax settings.
// ---------------------------------------------------------------------------

/// One payout as the tenant's list renders it. Read-only: payout
/// management lives in the provider's own dashboard / embedded component.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PayoutView {
    pub external_payout_id: String,
    pub amount_minor: i64,
    pub currency: String,
    /// `pending` | `paid` | `failed`. A failed payout is alerted, not
    /// just listed — the tenant's money is stuck at the provider.
    #[schema(example = "paid")]
    pub status: String,
    #[schema(value_type = Option<String>)]
    pub arrival_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PayoutsListView {
    /// Newest first; empty before a connection exists.
    pub payouts: Vec<PayoutView>,
}

/// `GET /api/v1/tenant/payments/payouts` — the connected account's payout
/// history (schedule, next payout, recent history), read live over the
/// connected account.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/payments/payouts",
    tag = "payments",
    operation_id = "list_tenant_payouts",
    responses(
        (status = 200, description = "The connected account's payout history, newest first", body = PayoutsListView),
        (status = 403, description = "Missing payments.read permission or the entitlement", body = ProblemDetail),
        (status = 502, description = "Provider unavailable (payments/provider-unavailable)", body = ProblemDetail)
    )
)]
pub async fn list_payouts(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    let view = state
        .list_payouts
        .execute(tenant.0.tenant_id)
        .await
        .map_err(payments_problem)?;
    Ok(HttpResponse::Ok()
        .insert_header(("cache-control", "no-store"))
        .json(PayoutsListView {
            payouts: view
                .payouts
                .into_iter()
                .map(|p| PayoutView {
                    external_payout_id: p.external_payout_id,
                    amount_minor: p.amount_minor,
                    currency: p.currency,
                    status: p.status,
                    arrival_at: p.arrival_at,
                })
                .collect(),
        }))
}

/// The tenant-facing automatic-tax state plus the standing legal
/// statement: registration and remittance are the tenant's responsibility;
/// Sanvi never advises on where or whether to register.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TaxSettingsView {
    pub enabled: bool,
    /// The liable connected-account id when enabled (`{ type: 'account' }`
    /// routing) — with direct charges the connected account is the
    /// merchant of record.
    pub liability_account: Option<String>,
    pub provider_status: String,
    pub active_registrations: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<TaxWarningView>,
    #[schema(value_type = Option<String>)]
    pub last_checked_at: Option<DateTime<Utc>>,
    /// Always present, always the same: we never advise.
    pub disclaimer: &'static str,
    /// The platform's fee on this tenant's payments, when one is enabled
    /// (off by default in 0.10.0). Disclosed here and on every affected
    /// payment — a platform fee the merchant cannot see is not acceptable,
    /// whatever the contract says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_fee: Option<PlatformFeeDisclosure>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TaxWarningView {
    /// Stable i18n key (`payments.tax.warning.*`).
    pub code: String,
    pub detail: String,
    #[schema(value_type = String)]
    pub detected_at: DateTime<Utc>,
}

pub const TAX_DISCLAIMER: &str = "Tax registration and remittance are your responsibility as the merchant of record. \
     Sanvi does not advise on where or whether to register — consult your tax advisor.";

fn tax_view(
    view: crate::application::tax_settings::TaxSettingsView,
    platform_fee: Option<PlatformFeeDisclosure>,
) -> TaxSettingsView {
    TaxSettingsView {
        enabled: view.enabled,
        liability_account: view.liability_account,
        provider_status: view.provider_status,
        active_registrations: view.active_registrations,
        warning: view.warning.map(|w| TaxWarningView {
            code: w.code,
            detail: w.detail,
            detected_at: w.detected_at,
        }),
        last_checked_at: view.last_checked_at,
        disclaimer: TAX_DISCLAIMER,
        platform_fee,
    }
}

/// The platform's fee disclosure for this tenant (their payment settings).
/// A platform fee the merchant cannot see is not acceptable, whatever the
/// contract says.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct PlatformFeeDisclosure {
    /// Fees are off for this tenant unless stated here.
    pub enabled: bool,
    pub basis_points: Option<u32>,
    pub fixed_minor: Option<i64>,
    pub fixed_currency: Option<String>,
    pub minimum_minor: i64,
}

impl PlatformFeeDisclosure {
    fn of(disclosure: &crate::application::TenantFeeDisclosure) -> Self {
        Self {
            enabled: disclosure.enabled,
            basis_points: disclosure.basis_points,
            fixed_minor: disclosure.fixed_minor,
            fixed_currency: disclosure.fixed_currency.clone(),
            minimum_minor: disclosure.minimum_minor,
        }
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateTaxSettingsRequest {
    /// Enable requires the compliance preflight to pass right now; disable
    /// always succeeds (and is a tenant-facing change requiring notice:
    /// mid-period it changes what customers are charged).
    pub enabled: bool,
}

/// `GET /api/v1/tenant/payments/tax-settings`.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/payments/tax-settings",
    tag = "payments",
    operation_id = "get_tenant_tax_settings",
    responses(
        (status = 200, description = "The tenant's automatic-tax state with the legal statement", body = TaxSettingsView),
        (status = 403, description = "Missing payments.read permission or the entitlement", body = ProblemDetail)
    )
)]
pub async fn get_tax_settings(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let view = state
        .manage_tax_settings
        .view(tenant_id)
        .await
        .map_err(payments_problem)?;
    // The disclosure rides the settings response: one read for the whole
    // payment-settings page.
    let platform_fee = crate::application::disclose_fee_for(state.fee_store.as_ref(), tenant_id)
        .await
        .map_err(payments_problem)?
        .as_ref()
        .map(PlatformFeeDisclosure::of);
    Ok(HttpResponse::Ok()
        .insert_header(("cache-control", "no-store"))
        .json(tax_view(view, platform_fee)))
}

/// `PUT /api/v1/tenant/payments/tax-settings` — enable/disable automatic
/// tax. Enabling runs the preflight first: an account whose tax settings
/// are not `active`, or which holds no active registration, cannot enable
/// (409 naming the missing step) — without registrations Stripe collects
/// nothing and returns no error, and that silence must never look like a
/// working toggle.
#[utoipa::path(
    put,
    path = "/api/v1/tenant/payments/tax-settings",
    tag = "payments",
    operation_id = "update_tenant_tax_settings",
    request_body = UpdateTaxSettingsRequest,
    responses(
        (status = 200, description = "The updated tax settings", body = TaxSettingsView),
        (status = 400, description = "Malformed request", body = ProblemDetail),
        (status = 403, description = "Missing payments.manage permission or the entitlement", body = ProblemDetail),
        (status = 409, description = "Preflight failed — the response names the exact missing step (payments/tax-preflight-failed)", body = ProblemDetail),
        (status = 502, description = "Provider unavailable (payments/provider-unavailable)", body = ProblemDetail)
    )
)]
pub async fn put_tax_settings(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    body: web::Json<UpdateTaxSettingsRequest>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, actix_web::Error> {
    let view = if body.enabled {
        state.manage_tax_settings.enable(tenant.0.tenant_id).await
    } else {
        state.manage_tax_settings.disable(tenant.0.tenant_id).await
    }
    .map_err(payments_problem)?;
    // The response repeats the disclosure so the toggle's page always
    // renders the fee next to it.
    let platform_fee =
        crate::application::disclose_fee_for(state.fee_store.as_ref(), tenant.0.tenant_id)
            .await
            .map_err(payments_problem)?
            .as_ref()
            .map(PlatformFeeDisclosure::of);
    Ok(HttpResponse::Ok().json(tax_view(view, platform_fee)))
}
