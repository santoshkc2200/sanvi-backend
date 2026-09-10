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

pub struct HttpNotificationGateway {
    http: reqwest::Client,
    base_url: String,
    send_api_key: String,
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
        }
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

        let url = self.endpoint_url("/v1/notifications");
        let request_id = uuid::Uuid::now_v7().to_string();

        let req_body = EnqueueRequestBody {
            tenant_id: email.tenant_id,
            recipient: email.recipient,
            subject: email.subject,
            text_body: email.text_body,
            html_body: email.html_body,
        };

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
                return Err(NotificationError::Unavailable {
                    detail: err.to_string(),
                });
            }
        };

        let status = response.status();
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
