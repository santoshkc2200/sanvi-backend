//! HTTP client adapter for the standalone notification service.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::config::NotificationConfig;
use crate::domain::{
    EnqueueEmail, EnqueuedNotification, NotificationError, NotificationGatewayPort,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

fn quota_notification_error(error: sanvi_http::ApiError) -> NotificationError {
    if error.status().is_server_error() {
        NotificationError::Unavailable {
            detail: error.to_string(),
        }
    } else {
        NotificationError::Conflict {
            detail: error.to_string(),
        }
    }
}

pub struct HttpNotificationGateway {
    http: reqwest::Client,
    base_url: String,
    send_api_key: String,
    /// Outbound-send abuse quota (TASK-024, FR-1117): counting only — the
    /// verdict is recorded as a `would_block` metric and the send still
    /// goes through. There is deliberately no enforcement path here:
    /// dropping a delivered invitation or DSR credential email is a
    /// policy decision this slice does not make, and the quota's job is to
    /// produce the number that decision will be made from.
    send_quota: Option<SendQuota>,
    commercial_quota: Option<sanvi_http::quota_middleware::QuotaRuntime>,
}

/// The counter half of the gateway's abuse quota, attached by the
/// composition root (which owns the Redis connection).
struct SendQuota {
    counter: std::sync::Arc<sanvi_http::abuse::AbuseCounter>,
    quota: sanvi_http::WindowQuota,
}

impl std::fmt::Debug for HttpNotificationGateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpNotificationGateway")
            .field("base_url", &self.base_url)
            .field("send_api_key", &"[REDACTED]")
            .finish()
    }
}

impl HttpNotificationGateway {
    /// Infallible by design: `base_url` is checked once at boot by
    /// `AppConfig::validate`, which reports every configuration problem
    /// together rather than panicking on the first one.
    pub fn new(config: NotificationConfig) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client builds with static config");
        Self {
            http,
            base_url: config.base_url,
            send_api_key: config.send_api_key,
            send_quota: None,
            commercial_quota: None,
        }
    }

    /// Attach the abuse counter the send quota counts against. Without it
    /// (dev machines, tests) sends are not counted — counting starts where
    /// the composition root can name a Redis connection.
    pub fn with_abuse_counter(
        mut self,
        counter: std::sync::Arc<sanvi_http::abuse::AbuseCounter>,
        limit: u64,
        window_secs: u64,
    ) -> Self {
        self.send_quota = Some(SendQuota {
            counter,
            quota: sanvi_http::WindowQuota { limit, window_secs },
        });
        self
    }

    pub fn with_commercial_quota(
        mut self,
        quota: sanvi_http::quota_middleware::QuotaRuntime,
    ) -> Self {
        self.commercial_quota = Some(quota);
        self
    }

    fn endpoint_url(&self, path: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = if path.starts_with('/') {
            path
        } else {
            &format!("/{path}")
        };
        format!("{base}{path}")
    }
}

#[derive(Serialize)]
struct EnqueueRequestBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    tenant_id: Option<&'a str>,
    recipient: &'a str,
    subject: &'a str,
    text_body: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    html_body: Option<&'a str>,
}

#[derive(Deserialize)]
struct EnqueueResponseBody {
    id: String,
    #[serde(default = "default_status")]
    status: String,
    #[serde(default)]
    idempotent_replay: bool,
}

fn default_status() -> String {
    "queued".to_string()
}

#[derive(Deserialize)]
struct ProblemDetails {
    #[serde(default = "default_problem_type")]
    r#type: String,
    #[serde(default)]
    #[allow(dead_code)]
    title: String,
    #[serde(default)]
    #[allow(dead_code)]
    status: Option<u16>,
    #[serde(default = "default_problem_detail")]
    detail: String,
    #[serde(default)]
    #[allow(dead_code)]
    request_id: Option<String>,
}

fn default_problem_type() -> String {
    "about:blank".to_string()
}

fn default_problem_detail() -> String {
    "notification-service returned an unreadable error body".to_string()
}

#[async_trait]
impl NotificationGatewayPort for HttpNotificationGateway {
    async fn enqueue_email(
        &self,
        email: EnqueueEmail<'_>,
    ) -> Result<EnqueuedNotification, NotificationError> {
        if self.base_url.is_empty() || self.send_api_key.is_empty() {
            return Err(NotificationError::Unavailable {
                detail: "notification service unconfigured (fail closed)".to_string(),
            });
        }

        let req_body = EnqueueRequestBody {
            tenant_id: email.tenant_id,
            recipient: email.recipient,
            subject: email.subject,
            text_body: email.text_body,
            html_body: email.html_body,
        };
        let reservation =
            if let (Some(runtime), Some(tenant)) = (&self.commercial_quota, email.tenant_id) {
                let tenant = tenant.parse().map_err(|_| NotificationError::Invalid {
                    code: "invalid_tenant".into(),
                    detail: "invalid tenant id".into(),
                })?;
                let payload =
                    serde_json::to_vec(&req_body).map_err(|_| NotificationError::Unavailable {
                        detail: "notification payload serialization failed".into(),
                    })?;
                runtime
                    .reserve_external_payload(
                        tenant,
                        sanvi_http::quotas::QuotaDimension::NotificationSends,
                        email.idempotency_key,
                        1,
                        &payload,
                    )
                    .await
                    .map_err(quota_notification_error)?
            } else {
                None
            };

        // Count the send against the tenant's window before it leaves the
        // process. Shadow only: the verdict is recorded, the send proceeds.
        if let Some(send_quota) = &self.send_quota {
            let key = match email.tenant_id {
                Some(tenant_id) => format!("tenant:{tenant_id}"),
                None => "tenant:none".to_string(),
            };
            let verdict = send_quota
                .counter
                .count("notifications", "tenant", &key, send_quota.quota)
                .await;
            sanvi_http::AbuseInstruments::process().record(
                "notifications",
                "tenant",
                if verdict.is_over() {
                    "would_block"
                } else {
                    "counted"
                },
            );
        }

        let url = self.endpoint_url("/v1/notifications");
        let request_id = uuid::Uuid::now_v7().to_string();

        let response = match self
            .http
            .post(&url)
            .bearer_auth(&self.send_api_key)
            .header("Idempotency-Key", email.idempotency_key)
            .header("X-Request-ID", &request_id)
            .json(&req_body)
            .send()
            .await
        {
            Ok(res) => res,
            Err(err) => {
                // NEVER log recipient address, rendered body, or api key!
                tracing::warn!(error = %err, "notification service request failed");
                if let (Some(runtime), Some(ticket)) = (&self.commercial_quota, &reservation)
                    && let Err(error) = runtime
                        .finish_external(
                            ticket,
                            sanvi_http::external_quotas::ExternalOutcome::Ambiguous,
                        )
                        .await
                {
                    tracing::warn!(%error,"notification quota settlement unavailable");
                }
                return Err(NotificationError::Unavailable {
                    detail: err.to_string(),
                });
            }
        };

        let status = response.status();
        if let (Some(runtime), Some(ticket)) = (&self.commercial_quota, &reservation) {
            use sanvi_http::external_quotas::ExternalOutcome;
            let outcome = if status.as_u16() == 202 {
                // The enqueue was accepted even if decoding its body fails.
                ExternalOutcome::Accepted { asset_id: None }
            } else if (400..500).contains(&status.as_u16())
                && !matches!(status.as_u16(), 401 | 403 | 408 | 409 | 429)
            {
                ExternalOutcome::Rejected
            } else {
                ExternalOutcome::Ambiguous
            };
            runtime
                .finish_external(ticket, outcome)
                .await
                .map_err(quota_notification_error)?;
        }
        if status.as_u16() == 202 {
            let body: EnqueueResponseBody =
                response
                    .json()
                    .await
                    .map_err(|err| NotificationError::Unavailable {
                        detail: format!("failed to parse 202 response body: {err}"),
                    })?;
            return Ok(EnqueuedNotification {
                id: body.id,
                status: body.status,
                idempotent_replay: body.idempotent_replay,
            });
        }

        let problem: ProblemDetails = response.json().await.unwrap_or(ProblemDetails {
            r#type: default_problem_type(),
            title: String::new(),
            status: Some(status.as_u16()),
            detail: default_problem_detail(),
            request_id: None,
        });

        match status.as_u16() {
            409 => {
                tracing::error!(
                    status = 409,
                    detail = %problem.detail,
                    "notification service conflict: idempotency key reused with different payload"
                );
                Err(NotificationError::Conflict {
                    detail: problem.detail,
                })
            }
            401 | 403 => {
                tracing::error!(
                    status = status.as_u16(),
                    "notification service authentication/authorization failure"
                );
                Err(NotificationError::Unavailable {
                    detail: "notification service authentication failure".to_string(),
                })
            }
            status_code if (400..500).contains(&status_code) => {
                tracing::warn!(
                    status = status_code,
                    code = %problem.r#type,
                    detail = %problem.detail,
                    "notification service rejected invalid request"
                );
                Err(NotificationError::Invalid {
                    code: problem.r#type,
                    detail: problem.detail,
                })
            }
            status_code => {
                tracing::warn!(
                    status = status_code,
                    detail = %problem.detail,
                    "notification service returned 5xx / transient error"
                );
                Err(NotificationError::Unavailable {
                    detail: problem.detail,
                })
            }
        }
    }
}

#[cfg(test)]
mod send_quota_tests {
    use super::*;
    use crate::domain::EnqueueEmail;
    use std::sync::Arc;

    #[test]
    fn quota_accounting_infrastructure_failure_remains_retryable() {
        let error = quota_notification_error(sanvi_http::ApiError::service_unavailable(
            "accounting temporarily unavailable",
        ));
        assert!(matches!(error, NotificationError::Unavailable { .. }));
        let error = quota_notification_error(sanvi_http::ApiError::Conflict {
            detail: "retired key".into(),
        });
        assert!(matches!(error, NotificationError::Conflict { .. }));
    }

    fn gateway(
        limit: u64,
        counter: Arc<sanvi_http::abuse::AbuseCounter>,
    ) -> HttpNotificationGateway {
        HttpNotificationGateway::new(NotificationConfig {
            base_url: "http://127.0.0.1:9".to_string(),
            send_api_key: "key".to_string(),
            ..Default::default()
        })
        .with_abuse_counter(counter, limit, 60)
    }

    fn email<'a>(tenant_id: Option<&'a str>, key: &'a str) -> EnqueueEmail<'a> {
        EnqueueEmail {
            tenant_id,
            recipient: "member@example.com",
            subject: "subject",
            text_body: "body",
            html_body: None,
            idempotency_key: key,
        }
    }

    /// Shadow only: sends past the quota still leave the process (they fail
    /// here only because the endpoint is dead, not because the quota
    /// refused them), and every attempt is counted per tenant.
    #[tokio::test]
    async fn sends_count_per_tenant_and_are_never_blocked_by_the_quota() {
        let counter = Arc::new(sanvi_http::abuse::AbuseCounter::new(None));
        let gateway = gateway(2, counter.clone());

        for i in 0..3 {
            let result = gateway
                .enqueue_email(email(Some("42"), &format!("k{i}")))
                .await;
            assert!(result.is_err(), "the dead endpoint fails the send itself");
        }
        assert_eq!(
            counter.local_count("notifications", "tenant", "tenant:42"),
            3
        );

        // A different tenant has its own window.
        let _ = gateway.enqueue_email(email(Some("7"), "k9")).await;
        assert_eq!(
            counter.local_count("notifications", "tenant", "tenant:7"),
            1
        );

        // Platform mail (no tenant) counts under its own key.
        let _ = gateway.enqueue_email(email(None, "k10")).await;
        assert_eq!(
            counter.local_count("notifications", "tenant", "tenant:none"),
            1
        );
    }

    /// Without a counter attached (dev machines, tests), sends are simply
    /// not counted — no gateway construction anywhere panics.
    #[tokio::test]
    async fn gateway_without_a_counter_still_sends() {
        let gateway = HttpNotificationGateway::new(NotificationConfig {
            base_url: "http://127.0.0.1:9".to_string(),
            send_api_key: "key".to_string(),
            ..Default::default()
        });
        assert!(gateway.enqueue_email(email(Some("1"), "k")).await.is_err());
    }
}
