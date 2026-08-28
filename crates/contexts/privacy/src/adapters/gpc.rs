//! Universal opt-out signal (GPC/UOOM) middleware.
//!
//! A request carrying `Sec-GPC: 1` records a preference signal for the
//! device *before any authentication*, so the directive engine denies
//! `sale_or_share` and `targeted_advertising` for that device immediately.
//! When the request is authenticated, unbound signals are bound to the
//! subject — binding never unbinds, and a later "accept all" from the same
//! device does not silently override it (the UI must ask; the answer is an
//! explicit consent ledger row).

use std::future::{Ready, ready};
use std::rc::Rc;
use std::sync::Arc;

use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform, forward_ready};
use actix_web::{Error, HttpMessage};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use sanvi_http::Subject;
use sanvi_persistence::TxManager;

use crate::domain::{
    Clock, IdGenerator, PreferenceSignal, ProcessingPurpose, SignalRepository, SubjectRef,
    SubjectRepository,
};

const GPC_HEADER: &str = "Sec-GPC";
/// Re-assert the signal at most once per window per device (idempotent).
const SIGNAL_WINDOW: chrono::Duration = chrono::Duration::hours(1);

#[derive(Clone)]
pub struct GpcSignalMiddleware {
    tx_manager: TxManager,
    signals: Arc<dyn SignalRepository>,
    subjects: Arc<dyn SubjectRepository>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    cookie_name: String,
    cookie_ttl_days: i64,
}

impl GpcSignalMiddleware {
    pub fn new(
        tx_manager: TxManager,
        signals: Arc<dyn SignalRepository>,
        subjects: Arc<dyn SubjectRepository>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        cookie_name: String,
        cookie_ttl_days: i64,
    ) -> Self {
        Self {
            tx_manager,
            signals,
            subjects,
            clock,
            ids,
            cookie_name,
            cookie_ttl_days,
        }
    }

    fn device_ref_from_request(&self, req: &ServiceRequest) -> Option<String> {
        let cookie_header = req.headers().get(actix_web::http::header::COOKIE)?;
        let cookie_header = cookie_header.to_str().ok()?;
        for pair in cookie_header.split(';') {
            let mut parts = pair.splitn(2, '=');
            let name = parts.next()?.trim();
            let value = parts.next().unwrap_or("").trim();
            if name == self.cookie_name && !value.is_empty() {
                return Some(value.to_string());
            }
        }
        None
    }

    fn set_device_cookie<B>(&self, res: &mut ServiceResponse<B>, device_ref: &str) {
        let header = format!(
            "{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
            self.cookie_name,
            device_ref,
            self.cookie_ttl_days * 86_400
        );
        res.headers_mut().insert(
            actix_web::http::header::SET_COOKIE,
            header.parse().expect("valid set-cookie"),
        );
    }

    async fn record_signal(&self, device_ref: &str, subject: Option<&Subject>) -> bool {
        match self
            .tx_manager
            .begin_unscoped("privacy_gpc_signal", "privacy")
            .await
        {
            Ok(mut scope) => {
                let now = self.clock.now();
                let privacy_subject = if let Some(subject) = subject {
                    self.subjects
                        .find_by_key(
                            &mut scope,
                            SubjectRef::user(subject.user_id).canonical_key(),
                        )
                        .await
                        .ok()
                        .flatten()
                } else {
                    None
                };
                if let Some(subject) = &privacy_subject {
                    let _ = self
                        .signals
                        .bind_by_device(
                            &mut scope,
                            device_ref,
                            subject.id,
                            subject.jurisdiction.as_str(),
                        )
                        .await;
                }
                let recent = self
                    .signals
                    .latest_for_device(&mut scope, device_ref)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|latest| now - latest.observed_at < SIGNAL_WINDOW);
                if recent {
                    let _ = scope.commit().await;
                    return true;
                }
                let signal_id = match self.ids.next_preference_signal_id() {
                    Ok(id) => id,
                    Err(err) => {
                        tracing::warn!(error = %err, "failed to allocate GPC signal id");
                        return false;
                    }
                };
                let signal = PreferenceSignal {
                    id: signal_id,
                    tenant_id: subject.and_then(|s| s.tenant_id),
                    signal: "gpc".to_string(),
                    device_ref: device_ref.to_string(),
                    subject_id: privacy_subject.as_ref().map(|s| s.id),
                    jurisdiction: privacy_subject.as_ref().map(|s| s.jurisdiction.clone()),
                    purposes: vec![
                        ProcessingPurpose::SaleOrShare,
                        ProcessingPurpose::TargetedAdvertising,
                    ],
                    observed_at: now,
                    bound_at: privacy_subject.as_ref().map(|_| now),
                    evidence: serde_json::json!({
                        "identifier": subject.map(|s| s.user_id.to_string()),
                    }),
                };
                let result = self.signals.append(&mut scope, &signal).await;
                let _ = scope.commit().await;
                result.is_ok()
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to record GPC signal");
                false
            }
        }
    }

    async fn bind_device(&self, device_ref: &str, subject: &Subject) {
        let subject_ref = SubjectRef::user(subject.user_id);
        match self
            .tx_manager
            .begin_unscoped("privacy_gpc_bind", "privacy")
            .await
        {
            Ok(mut scope) => {
                if let Ok(Some(privacy_subject)) = self
                    .subjects
                    .find_by_key(&mut scope, subject_ref.canonical_key())
                    .await
                {
                    let _ = self
                        .signals
                        .bind_by_device(
                            &mut scope,
                            device_ref,
                            privacy_subject.id,
                            privacy_subject.jurisdiction.as_str(),
                        )
                        .await;
                }
                let _ = scope.commit().await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to bind GPC signals");
            }
        }
    }
}

impl<S, B> Transform<S, ServiceRequest> for GpcSignalMiddleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: actix_web::body::MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type InitError = ();
    type Transform = GpcSignalService<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(GpcSignalService {
            service: Rc::new(service),
            inner: self.clone(),
        }))
    }
}

pub struct GpcSignalService<S> {
    service: Rc<S>,
    inner: GpcSignalMiddleware,
}

impl<S, B> Service<ServiceRequest> for GpcSignalService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: actix_web::body::MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future =
        futures_util::future::LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let service = self.service.clone();
        let inner = self.inner.clone();

        let gpc_active = req
            .headers()
            .get(GPC_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim() == "1");

        let device_ref = inner.device_ref_from_request(&req);
        let subject = req.extensions().get::<Subject>().cloned();

        Box::pin(async move {
            let mut device_ref = device_ref;
            let mut recorded = false;

            if gpc_active {
                if device_ref.is_none() {
                    device_ref = Some(Uuid::now_v7().to_string());
                }
                if let Some(device) = device_ref.clone() {
                    recorded = inner.record_signal(&device, subject.as_ref()).await;
                }
            } else if let (Some(subject), Some(device)) = (&subject, &device_ref) {
                // Bind the exact pre-authentication device signal once the
                // same browser authenticates.
                inner.bind_device(device, subject).await;
            }

            let mut res = service.call(req).await?;
            if gpc_active
                && recorded
                && let Some(device) = device_ref
            {
                inner.set_device_cookie(&mut res, &device);
            }
            Ok(res)
        })
    }
}

/// "Signal observed within the dedup window" predicate, exposed for tests.
pub fn within_window(now: DateTime<Utc>, then: DateTime<Utc>) -> bool {
    now - then < SIGNAL_WINDOW
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_window_predicate() {
        let now = Utc::now();
        let then = now - chrono::Duration::minutes(30);
        assert!(within_window(now, then));
        let old = now - chrono::Duration::hours(2);
        assert!(!within_window(now, old));
    }
}
