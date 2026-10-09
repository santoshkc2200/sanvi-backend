//! Collection ordering (TASK-027, FR-1127).
//!
//! Two layers, one contract: **every collection endpoint declares a
//! deterministic ordering in its spec, ending in the row's unique id**, and
//! the enforcement is real — rows with identical sort keys come back in the
//! declared order, stable across repeated fetches and across page
//! boundaries.
//!
//! *The spec gate* walks the merged OpenAPI document — the same walker the
//! TASK-021 bounded-collections gate uses, so the two gates see the same
//! surface — and fails on any bounded collection whose `summary` or
//! `description` does not carry a machine-parsable clause of the form
//! `Ordered by <key> <ASC|DESC>[, <key> <ASC|DESC>].` whose final key is
//! the unique tie-breaker (`id`; `seq` for the audit log, whose chain
//! position is the row's identity). A handler that drops the clause, or
//! declares a tie-break-less ordering, fails the build.
//!
//! *The live probe* serves `GET /api/v1/tenant/payments` through the real
//! payments stack against a real Postgres, seeded with rows that share one
//! `created_at`: the exact reshuffle the clause exists to prevent. Two
//! independent page walks must produce byte-identical order, ids descending
//! inside the tie.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use actix_web::{
    App, HttpMessage,
    dev::{Service, ServiceFactory, ServiceRequest, ServiceResponse},
    http::StatusCode,
    test as aw_test, web,
};
use serde_json::Value;
use utoipa::OpenApi as _;

use sanvi_http::{ApiError, Decision, FeatureGate, PermissionGate, ResourceRef, Subject};

// ---------------------------------------------------------------------------
// Spec gate
// ---------------------------------------------------------------------------

/// `<key> <ASC|DESC>` pairs, comma-separated, period-terminated.
fn parse_ordering_clause(text: &str) -> Option<Vec<(String, bool)>> {
    let start = text.find("Ordered by ")?;
    let clause = &text[start + "Ordered by ".len()..];
    let end = clause.find('.')?;
    let clause = &clause[..end];
    let mut keys = Vec::new();
    for part in clause.split(", ") {
        let (key, direction) = part.rsplit_once(' ')?;
        if !key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            || key.is_empty()
        {
            return None;
        }
        match direction {
            "ASC" => keys.push((key.to_string(), true)),
            "DESC" => keys.push((key.to_string(), false)),
            _ => return None,
        }
    }
    (!keys.is_empty()).then_some(keys)
}

fn operation_text(operation: &Value) -> String {
    [
        operation
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or(""),
        operation
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or(""),
    ]
    .join("\n")
}

#[test]
fn every_bounded_collection_declares_an_ordering_tie_broken_by_id() {
    let doc = serde_json::to_value(sanvi_api::ApiDoc::openapi()).expect("merged doc serializes");
    let collections = bounded_collections(&doc);
    assert!(
        !collections.is_empty(),
        "the gate enumerated zero bounded collections — the walker is broken, \
         not the contract vacuously clean"
    );

    let mut failures = Vec::new();
    for (operation_id, path, operation) in &collections {
        let declared = parse_ordering_clause(&operation_text(operation));
        let Some(keys) = declared else {
            failures.push(format!(
                "{operation_id} ({path}): no parseable `Ordered by … .` clause in the summary or \
                 description — declare the deterministic ordering the handler enforces"
            ));
            continue;
        };
        let (last_key, _) = keys.last().expect("non-empty by construction");
        let unique_key = unique_tie_breaker(operation_id, last_key);
        match unique_key {
            Ok(()) => {}
            Err(reason) => failures.push(format!(
                "{operation_id} ({path}): ordering `{last_key}` is not tie-broken by a unique key — \
                 {reason}"
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "collection endpoints without a specified deterministic ordering:\n  - {}",
        failures.join("\n  - ")
    );
}

/// The unique key a collection's ordering must end in. `id` everywhere,
/// with two documented exceptions where the row's identity is named
/// differently: the audit log's chain position `seq` *is* the entry id,
/// and a membership row is keyed by `(tenant_id, user_id)` — it has no
/// other id.
const NON_ID_UNIQUE_KEYS: &[(&str, &str)] = &[("list_audit", "seq"), ("list_members", "user_id")];

fn unique_tie_breaker(operation_id: &str, last_key: &str) -> Result<(), String> {
    if last_key == "id" {
        return Ok(());
    }
    match NON_ID_UNIQUE_KEYS
        .iter()
        .find(|(op, key)| *op == operation_id && *key == last_key)
    {
        Some(_) => Ok(()),
        None => Err(format!(
            "end the clause at `id` (exceptions: {})",
            NON_ID_UNIQUE_KEYS
                .iter()
                .map(|(op, key)| format!("{op} → {key}"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Collection GETs that declare a bounded `limit` — the TASK-021 non-exempt
/// set, re-derived the same way so the two gates cannot disagree.
fn bounded_collections(doc: &Value) -> Vec<(String, String, Value)> {
    let mut rows = Vec::new();
    let Some(paths) = doc.get("paths").and_then(Value::as_object) else {
        return rows;
    };
    for (path, item) in paths {
        let Some(operations) = item.as_object() else {
            continue;
        };
        for (method, operation) in operations {
            if method != "get" || !operation.is_object() {
                continue;
            }
            let bounded_limit = operation
                .get("parameters")
                .and_then(Value::as_array)
                .map(|params| {
                    params.iter().any(|parameter| {
                        parameter.get("name").and_then(Value::as_str) == Some("limit")
                            && parameter
                                .pointer("/schema/maximum")
                                .and_then(Value::as_i64)
                                .is_some_and(|max| max >= 1)
                            && parameter
                                .pointer("/schema/default")
                                .and_then(Value::as_i64)
                                .is_some()
                    })
                })
                .unwrap_or(false);
            if !bounded_limit {
                continue;
            }
            let operation_id = operation
                .get("operationId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("get {path}"));
            rows.push((operation_id, path.clone(), operation.clone()));
        }
    }
    rows
}

// ---------------------------------------------------------------------------
// Spec-gate self-tests
// ---------------------------------------------------------------------------

#[cfg(test)]
fn gate_on(operation: serde_json::Value) -> Result<(), String> {
    let keys = parse_ordering_clause(&operation_text(&operation));
    let operation_id = operation
        .get("operationId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match keys {
        None => Err("no clause".to_string()),
        Some(keys) => {
            let (last, _) = keys.last().expect("non-empty");
            unique_tie_breaker(operation_id, last)
        }
    }
}

#[test]
fn the_spec_gate_fails_on_a_missing_or_unanchored_clause() {
    let doc = serde_json::json!({
        "summary": "List things.",
        "description": "A fine list."
    });
    assert!(gate_on(doc).is_err(), "no clause must fail");

    let unanchored = serde_json::json!({
        "summary": "List things. Ordered by created_at DESC."
    });
    assert!(
        gate_on(unanchored).is_err(),
        "a tie-break-less ordering must fail"
    );

    let good = serde_json::json!({
        "summary": "List things.",
        "description": "Ordered by created_at DESC, id DESC."
    });
    assert!(gate_on(good).is_ok());

    let audit = serde_json::json!({
        "operationId": "list_audit",
        "summary": "The audit trail.",
        "description": "Ordered by seq DESC."
    });
    assert!(gate_on(audit).is_ok(), "seq is the audit row's id");
    let memberships = serde_json::json!({
        "operationId": "list_members",
        "summary": "The tenant's members.",
        "description": "Ordered by created_at ASC, user_id ASC."
    });
    assert!(
        gate_on(memberships).is_ok(),
        "user_id is a membership row's identity"
    );
}

// ---------------------------------------------------------------------------
// Live stability probe
// ---------------------------------------------------------------------------

/// Fixed ids owned by this suite.
const TENANT: i64 = 910_970_001;
const CONNECTION: i64 = 910_970_100;
const FIRST_PAYMENT: i64 = 910_970_101;
const PAYMENT_COUNT: i64 = 5;

struct AllowPaymentsRead;
impl PermissionGate for AllowPaymentsRead {
    fn check<'a>(
        &'a self,
        _subject: &'a Subject,
        permission: &'a str,
        _resource: &'a ResourceRef,
        _write: bool,
    ) -> Pin<Box<dyn Future<Output = Result<Decision, ApiError>> + Send + 'a>> {
        assert_eq!(permission, "payments.read");
        Box::pin(async { Ok(Decision::Allowed) })
    }
}

struct AllowFeatures;
impl FeatureGate for AllowFeatures {
    fn check<'a>(
        &'a self,
        _tenant_id: sanvi_kernel::TenantId,
        _feature: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, ApiError>> + Send + 'a>> {
        Box::pin(async { Ok(true) })
    }
}

/// The assembled payments routes over the real stack: tenant middleware is
/// substituted by a fixed `TenantContext` (the resolution middleware has
/// its own suites), permission and entitlement by allowing fakes — the
/// contract under test is the *ordering*, not the gates.
fn payments_app(
    pool: sanvi_persistence::PgPool,
    tenant: sanvi_kernel::TenantId,
) -> App<
    impl ServiceFactory<
        ServiceRequest,
        Config = (),
        Response = ServiceResponse<actix_web::body::BoxBody>,
        Error = actix_web::Error,
        InitError = (),
    > + 'static,
> {
    let config = sanvi_api::AppConfig::default();
    let ids = sanvi_testsupport::test_ids();
    let audit = sanvi_api::build_audit_state(pool.clone(), ids.clone(), &config.audit);
    let metrics = sanvi_telemetry::MetricsRegistry::prometheus_only();
    let state = sanvi_api::build_payments_state(
        pool,
        ids,
        &config.payments,
        metrics.prometheus_registry(),
        "test",
        audit.writer,
    );
    let permission: Arc<dyn PermissionGate> = Arc::new(AllowPaymentsRead);
    let features: Arc<dyn FeatureGate> = Arc::new(AllowFeatures);
    App::new()
        .app_data(web::Data::new(state))
        .app_data(web::Data::new(permission))
        .app_data(web::Data::new(features))
        .service(
            web::scope("/api/v1/tenant")
                .configure(sanvi_payments::adapters::http::routes::payments_tenant_scope_routes),
        )
        .wrap(sanvi_http::error::ErrorHandlersLayer)
        .wrap_fn(move |request, service| {
            request.extensions_mut().insert(sanvi_http::TenantContext {
                tenant_id: tenant,
                slug: "ordering".into(),
                display_name: "Ordering probe".into(),
                status: sanvi_http::TenantRuntimeStatus::Active,
                region: "jp".into(),
                default_locale: "en".into(),
                resolution_source: sanvi_http::ResolutionSource::SessionClaim,
            });
            request.extensions_mut().insert(Subject {
                user_id: sanvi_kernel::UserId::try_from(910_970_900_i64).expect("fixed id"),
                session_id: "ordering-probe-session".into(),
                aal: "aal1".into(),
                methods: vec!["password".into()],
                tenant_id: Some(tenant),
                role_ids: Vec::new(),
                authenticated_at: None,
            });
            service.call(request)
        })
}

/// One full page walk: (ids in order, per page), following `next_cursor`.
async fn walk_payments<S, B>(app: &S) -> Vec<Vec<String>>
where
    // `test::init_service` hands back a service over the raw
    // `actix_http::Request`; `TestRequest::to_request()` produces it.
    S: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: actix_web::body::MessageBody + 'static,
    B::Error: std::fmt::Debug,
{
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut uri = String::from("/api/v1/tenant/payments?limit=2");
        if let Some(cursor) = &cursor {
            uri.push_str(&format!("&cursor={cursor}"));
        }
        let response =
            aw_test::call_service(app, aw_test::TestRequest::get().uri(&uri).to_request()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = aw_test::read_body_json(response).await;
        let items: Vec<String> = body["items"]
            .as_array()
            .expect("items array")
            .iter()
            .map(|item| item["id"].as_str().expect("string id").to_string())
            .collect();
        cursor = body["next_cursor"].as_str().map(str::to_string);
        let exhausted = cursor.is_none();
        pages.push(items);
        if exhausted {
            break;
        }
    }
    pages
}

#[actix_web::test]
async fn tied_sort_keys_hold_one_order_across_fetches_and_page_boundaries() {
    let db = sanvi_testsupport::test_db().await;
    let admin = db.admin_pool().await;
    let tenant = sanvi_kernel::TenantId::try_from(TENANT).expect("fixed id");

    sqlx::query(
        "INSERT INTO tenancy.tenants(id, slug, display_name, status, region)
         VALUES ($1, 'ordering-probe', 'Ordering probe', 'active', 'jp')",
    )
    .bind(TENANT)
    .execute(&admin)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO payments.provider_connections(id, tenant_id, provider, external_account_id, status)
         VALUES ($1, $2, 'stripe_connect', 'acct_ordering', 'active')",
    )
    .bind(CONNECTION)
    .bind(TENANT)
    .execute(&admin)
    .await
    .unwrap();
    // Five payments, one shared created_at: every row ties on the declared
    // sort key, so the id tie-break alone decides the order.
    for index in 0..PAYMENT_COUNT {
        sqlx::query(
            "INSERT INTO payments.payments(id, tenant_id, external_payment_id,
                    status, amount_minor, currency, created_at, updated_at)
             VALUES ($1, $2, $3, 'succeeded', 1000, 'JPY',
                     '2030-01-01T00:00:00Z', '2030-01-01T00:00:00Z')",
        )
        .bind(FIRST_PAYMENT + index)
        .bind(TENANT)
        .bind(format!("seed_ordering_{index}"))
        .execute(&admin)
        .await
        .unwrap();
    }

    let app = aw_test::init_service(payments_app(db.app_pool(), tenant)).await;

    let first = walk_payments(&app).await;
    let second = walk_payments(&app).await;
    assert_eq!(
        first, second,
        "two fetches of the same collection must not reshuffle tied rows"
    );

    let flat: Vec<String> = first.concat();
    assert_eq!(
        flat.len(),
        PAYMENT_COUNT as usize,
        "the walk must stitch every page: {first:?}"
    );
    let ids: Vec<i64> = flat
        .iter()
        .map(|id| id.parse::<i64>().expect("decimal string id"))
        .collect();
    let mut sorted_desc = ids.clone();
    sorted_desc.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(
        ids, sorted_desc,
        "tied created_at values must fall through to the declared id DESC tie-break"
    );
    // The tie held: a single created_at means page boundaries cut straight
    // through it, so at least one boundary separates equal keys.
    assert!(
        first.len() >= 3,
        "limit=2 over {} rows must span page boundaries: {first:?}",
        PAYMENT_COUNT
    );
}
