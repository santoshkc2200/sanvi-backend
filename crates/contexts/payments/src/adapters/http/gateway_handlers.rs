//! Platform gateway handlers for the Payments bounded context.
//! Enforces entitlements (features.payments) and permission checks before delegating.
//! Embedded state is preserved for explicit regression fixtures.

use actix_web::{FromRequest, HttpRequest, HttpResponse, web};
use sanvi_http::{
    ApiError, CurrentSubject, CurrentTenant, ProblemDetail, RequiredFeature, RequiredPermission,
};

use super::{
    handlers::{
        CheckoutConfigView, CheckoutView, ConnectionSessionView, ConnectionView,
        CreateCheckoutRequest, CreateConnectionRequest, DisputeListQuery, DisputeListView,
        PaymentDetailView, PaymentListQuery, PaymentListView, PayoutsListView, ProvidersView,
        RefundRequest, RefundView, TaxSettingsView, UpdateTaxSettingsRequest,
        create_payment_connection_embedded, create_payment_connection_session_embedded,
        create_tenant_checkout_embedded, delete_payment_connection_embedded,
        export_payments_embedded, get_current_payment_connection_embedded,
        get_payment_connection_embedded, get_payment_embedded, get_tax_settings_embedded,
        get_tenant_checkout_config_embedded, get_tenant_checkout_embedded, list_disputes_embedded,
        list_payment_providers_embedded, list_payments_embedded, list_payouts_embedded,
        put_tax_settings_embedded, refund_payment_embedded,
    },
    state::PaymentsState,
};
use crate::{
    adapters::remote::{GatewayState, proxy},
    domain::{CheckoutId, ConnectionId, PaymentId},
};

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
        (status = 403, description = "Missing payments.read permission (the catalog is not entitlement-gated)", body = ProblemDetail),
        (status = 502, description = "Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Payments service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn list_payment_providers(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    _permission: RequiredPermission,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    list_payment_providers_embedded(state, tenant, _permission).await
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
        (status = 502, description = "Provider unavailable (payments/provider-unavailable), or the Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Payments service temporarily unavailable", body = ProblemDetail)
    ),
    params(
        ("Idempotency-Key" = uuid::Uuid, Header, description = "Required. The same key returns the same connection; it is also the provider-side idempotency key, so a retry after a crash adopts the created account instead of duplicating it")
    )
)]
pub async fn create_payment_connection(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    let mut raw: actix_web::dev::Payload = payload.into();
    let body = web::Json::<CreateConnectionRequest>::from_request(&req, &mut raw)
        .await
        .map_err(|error| ApiError::BadRequest {
            detail: error.to_string(),
        })?;
    create_payment_connection_embedded(state, tenant, subject, req, body, _permission, _entitlement)
        .await
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
        (status = 502, description = "Provider unavailable (payments/provider-unavailable), or the Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Payments service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn create_payment_connection_session(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    path: web::Path<ConnectionId>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    create_payment_connection_session_embedded(
        state,
        tenant,
        subject,
        req,
        path,
        _permission,
        _entitlement,
    )
    .await
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
        (status = 404, description = "No current connection for this tenant", body = ProblemDetail),
        (status = 502, description = "Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Payments service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn get_current_payment_connection(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    get_current_payment_connection_embedded(state, tenant, subject, _permission, _entitlement).await
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
        (status = 404, description = "No such connection for this tenant", body = ProblemDetail),
        (status = 502, description = "Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Payments service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn get_payment_connection(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<ConnectionId>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    get_payment_connection_embedded(state, tenant, subject, path, _permission, _entitlement).await
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
        (status = 409, description = "Money is still moving on this connection; the problem body names the blockers as extension members: blockers: [{code, summary_key}] with code in {in_flight_payments, open_disputes, pending_payouts} (payments/disconnect-blocked)", body = ProblemDetail),
        (status = 502, description = "Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Payments service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn delete_payment_connection(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    path: web::Path<ConnectionId>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    delete_payment_connection_embedded(state, tenant, subject, req, path, _permission, _entitlement)
        .await
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
        (status = 503, description = "The payments.checkout flag is off (payments/provider-unavailable), or the Payments service is temporarily unavailable", body = ProblemDetail),
        (status = 502, description = "Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail)
    )
)]
pub async fn get_tenant_checkout_config(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    get_tenant_checkout_config_embedded(state, tenant, _permission, _entitlement).await
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
        (status = 409, description = "The connected account cannot accept payments, or the Idempotency-Key owns a different request or a legacy checkout without a fingerprint", body = ProblemDetail),
        (status = 429, description = "Rate limit exceeded per tenant, client or customer reference (payments/rate-limited)", body = ProblemDetail),
        (status = 502, description = "Provider unavailable (payments/provider-unavailable), or the Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "The payments.checkout flag is off (payments/provider-unavailable), or the Payments service is temporarily unavailable", body = ProblemDetail)
    ),
    params(
        ("Idempotency-Key" = uuid::Uuid, Header, description = "Required. Replay must match the original order, amount, currency, customer context, locale and redirects; changed inputs return 409")
    )
)]
pub async fn create_tenant_checkout(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    let mut raw: actix_web::dev::Payload = payload.into();
    let body = web::Json::<CreateCheckoutRequest>::from_request(&req, &mut raw)
        .await
        .map_err(|error| ApiError::BadRequest {
            detail: error.to_string(),
        })?;
    create_tenant_checkout_embedded(state, tenant, req, body, _permission, _entitlement).await
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
        (status = 503, description = "The payments.checkout flag is off (payments/provider-unavailable), or the Payments service is temporarily unavailable", body = ProblemDetail),
        (status = 502, description = "Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail)
    )
)]
pub async fn get_tenant_checkout(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<CheckoutId>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    get_tenant_checkout_embedded(state, tenant, path, _permission, _entitlement).await
}

/// `GET /api/v1/tenant/payments` — every documented filter combinable,
/// cursor-paginated, tenant-scoped by RLS and indexed for the filter set.
/// Ordered by created_at DESC, id DESC.
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
        (status = 504, description = "Statement timeout — the request exceeded its budget", body = ProblemDetail),
        (status = 502, description = "Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Payments service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn list_payments(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    query: web::Query<PaymentListQuery>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    list_payments_embedded(state, tenant, query, _permission, _entitlement).await
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
        (status = 404, description = "No such payment for this tenant", body = ProblemDetail),
        (status = 502, description = "Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Payments service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn get_payment(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<PaymentId>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    get_payment_embedded(state, tenant, path, _permission, _entitlement).await
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
        (status = 502, description = "Provider unavailable (payments/provider-unavailable), or the Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Payments service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn refund_payment(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    path: web::Path<PaymentId>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    let mut raw: actix_web::dev::Payload = payload.into();
    let body = web::Json::<RefundRequest>::from_request(&req, &mut raw)
        .await
        .map_err(|error| ApiError::BadRequest {
            detail: error.to_string(),
        })?;
    refund_payment_embedded(
        state,
        tenant,
        subject,
        req,
        path,
        body,
        _permission,
        _entitlement,
    )
    .await
}

/// `GET /api/v1/tenant/payments/disputes` — disputes for this tenant,
/// cursor-paginated.
/// Ordered by created_at DESC, id DESC.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/payments/disputes",
    tag = "payments",
    operation_id = "list_tenant_disputes",
    params(DisputeListQuery),
    responses(
        (status = 200, description = "One page of disputes", body = DisputeListView),
        (status = 403, description = "Missing payments.read permission or the entitlement", body = ProblemDetail),
        (status = 504, description = "Statement timeout — the request exceeded its budget", body = ProblemDetail),
        (status = 502, description = "Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Payments service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn list_disputes(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    query: web::Query<DisputeListQuery>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    list_disputes_embedded(state, tenant, query, _permission, _entitlement).await
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
        (status = 403, description = "Missing payments.read permission or the entitlement", body = ProblemDetail),
        (status = 502, description = "Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Payments service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn export_payments(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    query: web::Query<PaymentListQuery>,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    export_payments_embedded(
        state,
        tenant,
        subject,
        req,
        query,
        _permission,
        _entitlement,
    )
    .await
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
        (status = 502, description = "Provider unavailable (payments/provider-unavailable), or the Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Payments service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn list_payouts(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    list_payouts_embedded(state, tenant, _permission, _entitlement).await
}

/// `GET /api/v1/tenant/payments/tax-settings`.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/payments/tax-settings",
    tag = "payments",
    operation_id = "get_tenant_tax_settings",
    responses(
        (status = 200, description = "The tenant's automatic-tax state with the legal statement", body = TaxSettingsView),
        (status = 403, description = "Missing payments.read permission or the entitlement", body = ProblemDetail),
        (status = 502, description = "Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Payments service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn get_tax_settings(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    get_tax_settings_embedded(state, tenant, _permission, _entitlement).await
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
        (status = 502, description = "Provider unavailable (payments/provider-unavailable), or the Payments service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Payments service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn put_tax_settings(
    state: Option<web::Data<PaymentsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Payments service is not configured"))?;
        return Ok(proxy(gateway, tenant, subject, _permission, req, payload).await?);
    };
    let mut raw: actix_web::dev::Payload = payload.into();
    let body = web::Json::<UpdateTaxSettingsRequest>::from_request(&req, &mut raw)
        .await
        .map_err(|error| ApiError::BadRequest {
            detail: error.to_string(),
        })?;
    put_tax_settings_embedded(state, tenant, body, _permission, _entitlement).await
}
