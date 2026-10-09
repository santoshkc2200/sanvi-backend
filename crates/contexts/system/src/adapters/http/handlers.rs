//! Handlers: thin actix shims over the application layer.
//!
//! The handlers know nothing about Postgres or Redis; they resolve their use
//! cases from application state and translate errors into [`ApiError`].

use std::{sync::Arc, time::Duration};

use actix_web::{HttpMessage, HttpRequest, HttpResponse, Responder, web};
use sanvi_http::{ApiError, ProblemDetail};

use crate::{
    application::{CheckLiveness, CheckReadiness, GetBuildDetails, GetBuildInfo, LivenessResult},
    domain::{BuildDetails, BuildInfo, HealthState, Readiness, ReadinessStates},
};

/// Application state visible to this context's handlers.
#[derive(Clone)]
pub struct SystemState {
    pub build_info: Arc<GetBuildInfo>,
    pub build_details: Arc<GetBuildDetails>,
    pub readiness: Arc<CheckReadiness>,
    pub liveness: Arc<CheckLiveness>,
}

/// Liveness: `GET /healthz`. Never depends on anything else.
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "system",
    operation_id = "liveness",
    responses(
        (status = 200, description = "Process is alive", body = LivenessResult),
        (status = 500, description = "Internal error", body = ProblemDetail)
    )
)]
pub async fn healthz(state: web::Data<SystemState>) -> impl Responder {
    HttpResponse::Ok().json(state.liveness.execute())
}

/// Readiness: `GET /readyz`. Degrades (503) when a dependency is down.
#[utoipa::path(
    get,
    path = "/readyz",
    tag = "system",
    operation_id = "readiness",
    responses(
        (status = 200, description = "All dependencies healthy", body = Readiness),
        (status = 503, description = "At least one dependency is degraded", body = ProblemDetail, content_type = "application/problem+json")
    )
)]
pub async fn readyz(state: web::Data<SystemState>, req: HttpRequest) -> impl Responder {
    let output = state.readiness.execute().await;
    if output.readiness.is_ready() {
        HttpResponse::Ok().json(output.readiness)
    } else {
        let detail = output
            .readiness
            .checks
            .iter()
            .filter(|check| check.state == crate::domain::HealthState::Degraded)
            .map(|check| {
                check
                    .detail
                    .clone()
                    .unwrap_or_else(|| "degraded".to_string())
            })
            .collect::<Vec<_>>()
            .join("; ");
        let problem = ApiError::ServiceUnavailable {
            detail: format!("one or more dependencies are degraded: {detail}"),
        }
        .with_context(
            Some(req.path().to_string()),
            req.extensions()
                .get::<sanvi_http::RequestId>()
                .map(|id| id.to_string()),
        );
        HttpResponse::ServiceUnavailable().json(problem)
    }
}

/// Liveness under the versioned API: `GET /api/v1/system/health` (TASK-025).
/// Identical semantics to `GET /healthz` — process aliveness only, never a
/// database or cache touch — so an orchestrator that can only reach the
/// API surface gets a probe that cannot turn a slow database into
/// "every instance unhealthy".
#[utoipa::path(
    get,
    path = "/api/v1/system/health",
    tag = "system",
    operation_id = "health",
    responses(
        (status = 200, description = "Process is alive", body = LivenessResult),
        (status = 500, description = "Internal error", body = ProblemDetail)
    )
)]
pub async fn health(state: web::Data<SystemState>) -> impl Responder {
    HttpResponse::Ok().json(state.liveness.execute())
}

/// Readiness as states: `GET /api/v1/system/ready` (TASK-025). The
/// dependency checks run concurrently under their own budget and this
/// handler wraps the fan-out in a response timeout of its own, so the
/// endpoint answers within `budget + grace` no matter what a dependency
/// does. Successful responses report dependency states; the assembled API
/// normalizes HTTP 503 into the shared problem-details envelope.
#[utoipa::path(
    get,
    path = "/api/v1/system/ready",
    tag = "system",
    operation_id = "ready",
    responses(
        (status = 200, description = "All dependencies healthy", body = ReadinessStates),
        (status = 503, description = "At least one dependency is degraded", body = ProblemDetail, content_type = "application/problem+json")
    )
)]
pub async fn ready(state: web::Data<SystemState>) -> impl Responder {
    // Budget plus a grace slice for scheduling: the endpoint's own ceiling.
    // On timeout the honest answer is degraded — a probe that cannot finish
    // within its budget is not a ready dependency.
    let grace = std::time::Duration::from_secs(1);
    let states =
        match tokio::time::timeout(state.readiness.budget() + grace, state.readiness.execute())
            .await
        {
            Ok(output) => output.readiness.states(),
            Err(_) => ReadinessStates {
                status: HealthState::Degraded,
                checks: state
                    .readiness
                    .dependency_names()
                    .into_iter()
                    .map(|name| crate::domain::DependencyState {
                        name,
                        state: HealthState::Degraded,
                    })
                    .collect(),
            },
        };

    if states.status == HealthState::Ok {
        HttpResponse::Ok().json(states)
    } else {
        HttpResponse::ServiceUnavailable().json(states)
    }
}

/// Build/version info: `GET /api/v1/system/version`.
#[utoipa::path(
    get,
    path = "/api/v1/system/version",
    tag = "system",
    operation_id = "version",
    responses(
        (status = 200, description = "Build information", body = BuildInfo),
        (status = 500, description = "Internal error", body = ProblemDetail)
    )
)]
pub async fn version(state: web::Data<SystemState>) -> impl Responder {
    HttpResponse::Ok().json(state.build_info.execute())
}

/// Build probe: `GET /api/v1/system/build` (FR-1102). Unauthenticated and
/// cacheable so RUM, the error tracker and load tools can stamp a
/// measurement with the exact build that produced it; carries no tenant
/// data.
#[utoipa::path(
    get,
    path = "/api/v1/system/build",
    tag = "system",
    operation_id = "build",
    responses(
        (status = 200, description = "Release stamp of the running binary", body = BuildDetails,
         headers(
             ("Cache-Control" = String, description = "public — the stamp is identical for every caller of a build")
         )),
        (status = 500, description = "Internal error", body = ProblemDetail)
    )
)]
pub async fn build(state: web::Data<SystemState>) -> impl Responder {
    HttpResponse::Ok()
        .insert_header((actix_web::http::header::CACHE_CONTROL, "public, max-age=60"))
        .json(state.build_details.execute())
}

/// Convenience for tests: state with no real dependencies.
pub fn state_for_tests(build_info: BuildInfo, budget: Duration) -> SystemState {
    SystemState {
        build_details: Arc::new(GetBuildDetails::new(&build_info, "local".to_string())),
        build_info: Arc::new(GetBuildInfo::new(build_info)),
        readiness: Arc::new(CheckReadiness::new(Vec::new(), budget)),
        liveness: Arc::new(CheckLiveness::new()),
    }
}
