//! Handlers: thin actix shims over the application layer.

use std::net::IpAddr;
use std::sync::Arc;

use actix_web::{HttpMessage, HttpRequest, HttpResponse, Responder, web};
use sanvi_http::{ApiError, CurrentSubject, RequiredPermission, TenantContext};
use sanvi_kernel::TenantId;
use serde::Deserialize;
use utoipa::ToSchema;

use crate::application::{
    AddPlanPrice, AddPlanPriceCommand, ApplySubscriptionOverride, ApplySubscriptionOverrideCommand,
    CreateCheckoutSession, CreateCheckoutSessionCommand, CreatePlan, CreatePlanCommand,
    CreatePortalSession, GetSubscription, ListInvoices, ListPlans, ListPublicPlans,
    RevokeSubscriptionOverride, SetPlanEntitlements, SetPlanEntitlementsCommand, UpdatePlan,
    UpdatePlanCommand, WebhookReceiver,
};
use crate::domain::{
    BillingError, EntitlementGrant, PlanId, PlanTier, PlanVisibility, PriceId, PriceInterval,
};

use super::validation;

/// Application state visible to this context's handlers.
#[derive(Clone)]
pub struct BillingState {
    pub list_public_plans: Arc<ListPublicPlans>,
    pub create_checkout: Arc<CreateCheckoutSession>,
    pub create_portal: Arc<CreatePortalSession>,
    pub get_subscription: Arc<GetSubscription>,
    pub list_invoices: Arc<ListInvoices>,
    pub list_plans: Arc<ListPlans>,
    pub create_plan: Arc<CreatePlan>,
    pub update_plan: Arc<UpdatePlan>,
    pub add_price: Arc<AddPlanPrice>,
    pub set_entitlements: Arc<SetPlanEntitlements>,
    pub apply_override: Arc<ApplySubscriptionOverride>,
    pub revoke_override: Arc<RevokeSubscriptionOverride>,
    pub webhook: Arc<WebhookReceiver>,
}

/// The billing context's domain-error → HTTP mapping (single place, this
/// boundary).
pub fn api_error(err: BillingError) -> ApiError {
    match err {
        // TASK-027 (FR-1127): invalid-input rejections are field-addressed
        // (`validation::validation_issue` owns the field paths); webhook
        // payloads stay flat — no request field is invalid there.
        BillingError::InvalidPlanKey(_)
        | BillingError::InvalidPlanName(_)
        | BillingError::InvalidPlanTier(_)
        | BillingError::InvalidPlanVisibility(_)
        | BillingError::InvalidPrice(_)
        | BillingError::InvalidCurrency(_)
        | BillingError::InvalidInterval(_)
        | BillingError::InvalidEntitlement(_) => {
            let (detail, issues) = validation::validation_issue(&err)
                .expect("plan and price rejections are field-addressed");
            ApiError::validation(detail, issues)
        }
        BillingError::InvalidWebhookPayload(_) => ApiError::BadRequest {
            detail: err.to_string(),
        },
        BillingError::PlanNotFound(name)
        | BillingError::PriceNotFound(name)
        | BillingError::SubscriptionNotFound(name)
        | BillingError::TenantNotFound(name) => ApiError::NotFound { resource: name },
        BillingError::PriceNotActive(_) | BillingError::PlanNotPublic(_) => ApiError::Conflict {
            detail: err.to_string(),
        },
        BillingError::SubscriptionExists
        | BillingError::SubscriptionNotActive
        | BillingError::PlanKeyTaken(_)
        | BillingError::Conflict(_) => ApiError::Conflict {
            detail: err.to_string(),
        },
        BillingError::TaxRegistrationMissing => ApiError::ServiceUnavailable {
            detail: err.to_string(),
        },
        BillingError::StripeBadRequest { detail } => ApiError::BadRequest { detail },
        BillingError::StripeUnauthorized
        | BillingError::StripeRateLimited
        | BillingError::StripeService { .. }
        | BillingError::StripeConnection { .. } => {
            tracing::error!(error = %err, "stripe integration failure");
            ApiError::ServiceUnavailable {
                detail: "billing provider is temporarily unavailable".to_string(),
            }
        }
        BillingError::InvalidWebhookSignature(_) => ApiError::Unauthorized,
        BillingError::UnknownWebhookEvent(_) | BillingError::WebhookProcessing(_) => {
            ApiError::BadRequest {
                detail: err.to_string(),
            }
        }
        BillingError::Storage(detail) => {
            tracing::error!(error = %detail, "billing storage failure");
            ApiError::from_storage_detail(&detail)
        }
    }
}

fn current_tenant(req: &HttpRequest) -> Result<TenantId, ApiError> {
    req.extensions()
        .get::<TenantContext>()
        .map(|context| context.tenant_id)
        .ok_or_else(|| ApiError::NotFound {
            resource: "tenant".to_string(),
        })
}

// ---------------------------------------------------------------------------
// Public pricing page
// ---------------------------------------------------------------------------

/// `GET /api/v1/public/plans` — the pricing page catalog (localized by
/// phase 06; hidden plans excluded).///
/// Cache semantics (TASK-022, FR-1111): when `platform.ssr_cache` is
/// enabled, the anonymous response is served from the server-side SSR cache
/// keyed by (tenant, locale, theme revision, device class) with
/// single-flight on miss; a theme publish or content change invalidates the
/// affected entries within ~2 s (the entry TTL is a backstop only).
/// Requests carrying an `Authorization` header are never served from or
/// written to the cache, and responses carrying `Set-Cookie` are never
/// stored. Varies on `Accept-Language`.
#[utoipa::path(
    get,
    path = "/api/v1/public/plans",
    tag = "billing",
    operation_id = "list_public_plans",
    responses(
        (status = 200, description = "Plans with prices and entitlement grants", body = Vec<crate::application::PublicPlanView>)
    )
)]
pub async fn list_public_plans(state: web::Data<BillingState>) -> Result<impl Responder, ApiError> {
    let plans = state.list_public_plans.execute().await.map_err(api_error)?;
    Ok(HttpResponse::Ok().json(plans))
}

// ---------------------------------------------------------------------------
// Tenant billing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CheckoutSessionRequest {
    #[schema(value_type = String)]
    pub price_id: PriceId,
    pub success_url: Option<String>,
    pub cancel_url: Option<String>,
}

/// `POST /api/v1/tenant/billing/checkout-session` — start subscribing.
/// Access is granted only when Stripe webhooks arrive, never from the
/// redirect.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/billing/checkout-session",
    tag = "billing",
    operation_id = "create_checkout_session",
    request_body = CheckoutSessionRequest,
    responses(
        (status = 201, description = "Checkout URL", body = crate::application::CheckoutSessionView),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 409, description = "A live subscription already exists", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn create_checkout_session(
    state: web::Data<BillingState>,
    subject: CurrentSubject,
    body: web::Json<CheckoutSessionRequest>,
    _guard: RequiredPermission,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let tenant_id = current_tenant(&req)?;
    let session = state
        .create_checkout
        .execute(
            subject.0.user_id,
            CreateCheckoutSessionCommand {
                tenant_id,
                price_id: body.price_id,
                success_url: body.success_url.clone(),
                cancel_url: body.cancel_url.clone(),
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(session))
}

/// `POST /api/v1/tenant/billing/portal-session` — open the Customer Portal
/// (upgrades, downgrades, cancellation, payment methods).
#[utoipa::path(
    post,
    path = "/api/v1/tenant/billing/portal-session",
    tag = "billing",
    operation_id = "create_portal_session",
    responses(
        (status = 201, description = "Portal URL", body = crate::application::PortalSessionView),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 404, description = "No subscription", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn create_portal_session(
    state: web::Data<BillingState>,
    subject: CurrentSubject,
    _guard: RequiredPermission,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let tenant_id = current_tenant(&req)?;
    let session = state
        .create_portal
        .execute(subject.0.user_id, tenant_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(session))
}

/// `GET /api/v1/tenant/billing/subscription` — the tenant's subscription.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/billing/subscription",
    tag = "billing",
    operation_id = "get_subscription",
    responses(
        (status = 200, description = "The subscription, or null when none", body = Option<crate::application::SubscriptionView>),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn get_subscription(
    state: web::Data<BillingState>,
    _subject: CurrentSubject,
    _guard: RequiredPermission,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let tenant_id = current_tenant(&req)?;
    let view = state
        .get_subscription
        .execute(tenant_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(view))
}

/// Query for `GET /api/v1/tenant/billing/invoices` (TASK-021: bounded page
/// size and a keyset cursor, declared in the contract).
#[derive(Debug, Clone, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct InvoiceListQuery {
    /// Page size, 1..=100, default 50.
    #[param(schema_with = invoice_limit_schema)]
    pub limit: Option<u32>,
    /// Cursor: the id of the last invoice of the previous page. Absent for
    /// the first page; the page is ordered newest first.
    pub after: Option<String>,
}

fn invoice_limit_schema() -> utoipa::openapi::schema::Object {
    sanvi_http::openapi::page_limit_schema(100, 50).build()
}

/// `GET /api/v1/tenant/billing/invoices` — the tenant's invoices.
/// Ordered by id DESC.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/billing/invoices",
    tag = "billing",
    operation_id = "list_invoices",
    params(InvoiceListQuery),
    responses(
        (status = 200, description = "Invoices, newest first (ordered by id, which encodes creation time)", body = Vec<crate::application::InvoiceView>),
        (status = 400, description = "Malformed cursor", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 504, description = "Statement timeout — the request exceeded its budget", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_invoices(
    state: web::Data<BillingState>,
    query: web::Query<InvoiceListQuery>,
    _subject: CurrentSubject,
    _guard: RequiredPermission,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let tenant_id = current_tenant(&req)?;
    let after_id = query
        .after
        .as_deref()
        .map(|raw| raw.parse::<i64>())
        .transpose()
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let invoices = state
        .list_invoices
        .execute(tenant_id, query.limit, after_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(invoices))
}

// ---------------------------------------------------------------------------
// Platform plan management
// ---------------------------------------------------------------------------

/// `GET /api/v1/platform/plans` — the operator catalog (hidden plans too).
#[utoipa::path(
    get,
    path = "/api/v1/platform/plans",
    tag = "billing",
    operation_id = "list_plans",
    responses(
        (status = 200, description = "All plans", body = Vec<crate::application::PlanAdminView>),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_plans(
    state: web::Data<BillingState>,
    _subject: CurrentSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let plans = state.list_plans.execute().await.map_err(api_error)?;
    Ok(HttpResponse::Ok().json(plans))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreatePlanRequest {
    pub key: String,
    pub name: String,
    pub tier: PlanTier,
    pub stripe_product_id: String,
    #[serde(default)]
    pub visibility: PlanVisibility,
    #[serde(default)]
    pub sort_order: i32,
}

/// `POST /api/v1/platform/plans` — create a plan (the Stripe Product must
/// exist in the Dashboard first; its id goes into `stripe_product_id`).
#[utoipa::path(
    post,
    path = "/api/v1/platform/plans",
    tag = "billing",
    operation_id = "create_plan",
    request_body = CreatePlanRequest,
    responses(
        (status = 201, description = "Created plan", body = crate::application::PlanAdminView),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 409, description = "Plan key taken", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn create_plan(
    state: web::Data<BillingState>,
    subject: CurrentSubject,
    body: web::Json<CreatePlanRequest>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let plan = state
        .create_plan
        .execute(
            subject.0.user_id,
            CreatePlanCommand {
                key: body.key.clone(),
                name: body.name.clone(),
                tier: body.tier,
                stripe_product_id: body.stripe_product_id.clone(),
                visibility: body.visibility,
                sort_order: body.sort_order,
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(plan))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdatePlanRequest {
    pub name: String,
    pub stripe_product_id: Option<String>,
    #[serde(default)]
    pub visibility: PlanVisibility,
    #[serde(default)]
    pub sort_order: i32,
}

/// `PATCH /api/v1/platform/plans/{id}` — update a plan's mutable fields.
#[utoipa::path(
    patch,
    path = "/api/v1/platform/plans/{id}",
    tag = "billing",
    operation_id = "update_plan",
    params(("id" = String, Path, description = "Plan id")),
    request_body = UpdatePlanRequest,
    responses(
        (status = 200, description = "Updated plan", body = crate::application::PlanAdminView),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 404, description = "Plan not found", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn update_plan(
    state: web::Data<BillingState>,
    subject: CurrentSubject,
    path: web::Path<PlanId>,
    body: web::Json<UpdatePlanRequest>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let plan = state
        .update_plan
        .execute(
            subject.0.user_id,
            UpdatePlanCommand {
                plan_id: path.into_inner(),
                name: body.name.clone(),
                stripe_product_id: body.stripe_product_id.clone(),
                visibility: body.visibility,
                sort_order: body.sort_order,
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(plan))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AddPriceRequest {
    pub stripe_price_id: String,
    pub currency: String,
    pub interval: PriceInterval,
    pub unit_amount_minor: i64,
    pub trial_days: Option<u16>,
}

/// `POST /api/v1/platform/plans/{id}/prices` — attach a Stripe Price
/// (billing variant of the plan).
#[utoipa::path(
    post,
    path = "/api/v1/platform/plans/{id}/prices",
    tag = "billing",
    operation_id = "add_plan_price",
    params(("id" = String, Path, description = "Plan id")),
    request_body = AddPriceRequest,
    responses(
        (status = 201, description = "Created price", body = crate::application::PlanAdminPriceView),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 404, description = "Plan not found", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn add_plan_price(
    state: web::Data<BillingState>,
    subject: CurrentSubject,
    path: web::Path<PlanId>,
    body: web::Json<AddPriceRequest>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let price = state
        .add_price
        .execute(
            subject.0.user_id,
            AddPlanPriceCommand {
                plan_id: path.into_inner(),
                stripe_price_id: body.stripe_price_id.clone(),
                currency: body.currency.clone(),
                interval: body.interval,
                unit_amount_minor: body.unit_amount_minor,
                trial_days: body.trial_days,
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(price))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetEntitlementsRequest {
    pub grants: Vec<EntitlementGrant>,
}

/// `PUT /api/v1/platform/plans/{id}/entitlements` — replace the plan's
/// entitlement map. Pricing changes never require a deploy; tenants get the
/// new map on their next entitlement recompute.
#[utoipa::path(
    put,
    path = "/api/v1/platform/plans/{id}/entitlements",
    tag = "billing",
    operation_id = "set_plan_entitlements",
    params(("id" = String, Path, description = "Plan id")),
    request_body = SetEntitlementsRequest,
    responses(
        (status = 200, description = "The new map", body = Vec<EntitlementGrant>),
        (status = 400, description = "Unknown feature key", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 404, description = "Plan not found", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn set_plan_entitlements(
    state: web::Data<BillingState>,
    subject: CurrentSubject,
    path: web::Path<PlanId>,
    body: web::Json<SetEntitlementsRequest>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let grants = state
        .set_entitlements
        .execute(
            subject.0.user_id,
            SetPlanEntitlementsCommand {
                plan_id: path.into_inner(),
                grants: body.grants.clone(),
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(grants))
}

// ---------------------------------------------------------------------------
// Platform subscription override
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SubscriptionOverrideRequest {
    pub plan_key: String,
    pub reason: String,
}

/// `POST /api/v1/platform/tenants/{id}/subscription/override` — grant a
/// plan's entitlements without a Stripe subscription (audited operator
/// action).
#[utoipa::path(
    post,
    path = "/api/v1/platform/tenants/{id}/subscription/override",
    tag = "billing",
    operation_id = "apply_subscription_override",
    params(("id" = String, Path, description = "Tenant id")),
    request_body = SubscriptionOverrideRequest,
    responses(
        (status = 200, description = "Override applied"),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 404, description = "Plan not found", body = sanvi_http::ProblemDetail),
        (status = 409, description = "Live Stripe subscription blocks an override", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn apply_subscription_override(
    state: web::Data<BillingState>,
    subject: CurrentSubject,
    path: web::Path<TenantId>,
    body: web::Json<SubscriptionOverrideRequest>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    state
        .apply_override
        .execute(
            subject.0.user_id,
            ApplySubscriptionOverrideCommand {
                tenant_id: path.into_inner(),
                plan_key: body.plan_key.clone(),
                reason: body.reason.clone(),
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(serde_json::json!({ "status": "applied" })))
}

/// `DELETE /api/v1/platform/tenants/{id}/subscription/override` — revoke an
/// operator override.
#[utoipa::path(
    delete,
    path = "/api/v1/platform/tenants/{id}/subscription/override",
    tag = "billing",
    operation_id = "revoke_subscription_override",
    params(("id" = String, Path, description = "Tenant id")),
    responses(
        (status = 204, description = "Override revoked"),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 409, description = "Not an operator override", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn revoke_subscription_override(
    state: web::Data<BillingState>,
    subject: CurrentSubject,
    path: web::Path<TenantId>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    state
        .revoke_override
        .execute(subject.0.user_id, path.into_inner())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::NoContent())
}

// ---------------------------------------------------------------------------
// Internal webhook endpoint
// ---------------------------------------------------------------------------

/// `POST /internal/webhooks/stripe/platform` — Stripe's webhook receiver.
/// Signature verified, raw event stored, 2xx immediately; the worker
/// processes it. Redeliveries are deduplicated by event id.
pub async fn stripe_platform_webhook(
    state: web::Data<BillingState>,
    body: web::Bytes,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let signature = req
        .headers()
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok());
    let client_ip: Option<IpAddr> = req.peer_addr().map(|addr| addr.ip()).or_else(|| {
        req.connection_info()
            .realip_remote_addr()
            .and_then(|raw| raw.parse().ok())
    });
    match state.webhook.receive(&body, signature, client_ip).await {
        Ok(_) => Ok(HttpResponse::Ok().json(serde_json::json!({ "received": true }))),
        Err(BillingError::InvalidWebhookSignature(detail)) => {
            tracing::warn!(detail, "stripe webhook signature rejected");
            Err(ApiError::Unauthorized)
        }
        Err(err) => {
            tracing::error!(error = %err, "stripe webhook storage failure");
            Err(ApiError::internal("webhook storage failure"))
        }
    }
}
