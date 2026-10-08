use async_trait::async_trait;
use sanvi_kernel::UserId;
use serde::{Deserialize, Serialize};

use crate::domain::AccessError;
use crate::domain::ports::{PermissionStore, Tuple};

#[derive(Debug, Clone)]
pub struct HttpKetoClient {
    http: reqwest::Client,
    read_url: String,
    write_url: String,
    breaker: std::sync::Arc<sanvi_resilience::CircuitBreaker>,
    degradation: Option<std::sync::Arc<sanvi_resilience::DegradationRegistry>>,
}

const KETO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl HttpKetoClient {
    pub fn new(read_url: String, write_url: String) -> Result<Self, AccessError> {
        Self::with_resilience(
            read_url,
            write_url,
            KETO_TIMEOUT,
            sanvi_resilience::BreakerConfig::default(),
            None,
        )
    }

    /// Test constructor: a shortened deadline so a slow Keto is observable
    /// in milliseconds.
    pub fn with_timeout(
        read_url: String,
        write_url: String,
        timeout: std::time::Duration,
    ) -> Result<Self, AccessError> {
        Self::with_resilience(
            read_url,
            write_url,
            timeout,
            sanvi_resilience::BreakerConfig::default(),
            None,
        )
    }

    /// The full constructor (TASK-023): the matrix's breaker (5 failures /
    /// 30 s cooldown) and the degradation registry's `keto` reason.
    pub fn with_resilience(
        read_url: String,
        write_url: String,
        timeout: std::time::Duration,
        breaker_config: sanvi_resilience::BreakerConfig,
        degradation: Option<std::sync::Arc<sanvi_resilience::DegradationRegistry>>,
    ) -> Result<Self, AccessError> {
        let http = reqwest::Client::builder()
            .connect_timeout(timeout)
            .timeout(timeout)
            .build()
            .map_err(|err| AccessError::BackendUnreachable(err.to_string()))?;
        let breaker = std::sync::Arc::new(sanvi_resilience::CircuitBreaker::new(
            "keto",
            breaker_config,
            None,
        ));
        Ok(Self {
            http,
            read_url,
            write_url,
            breaker,
            degradation,
        })
    }

    /// TASK-023: membership checks run under the dependency-matrix
    /// breaker; an open breaker fails fast as `BackendUnreachable` — the
    /// exact error shape the degraded authorizer's deny/cached-read rule
    /// recognises.
    fn note_breaker_state(&self) {
        if let Some(degradation) = &self.degradation {
            degradation.set_degraded(
                sanvi_resilience::DegradationReason::Keto,
                self.breaker.state() == sanvi_resilience::BreakerState::Open,
            );
        }
    }
}

/// The REST shapes Keto expects.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct TupleBody {
    namespace: String,
    object: String,
    relation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_set: Option<SubjectSetBody>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct SubjectSetBody {
    namespace: String,
    object: String,
    relation: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CheckResponse {
    #[serde(default)]
    allowed: bool,
}

impl TupleBody {
    fn from_tuple(tuple: &Tuple) -> Self {
        match tuple {
            Tuple::Subject {
                namespace,
                object,
                relation,
                subject_id,
            } => Self {
                namespace: namespace.clone(),
                object: object.clone(),
                relation: relation.clone(),
                subject_id: Some(subject_id.to_string()),
                subject_set: None,
            },
            Tuple::SubjectSet {
                namespace,
                object,
                relation,
                subject_set,
            } => Self {
                namespace: namespace.clone(),
                object: object.clone(),
                relation: relation.clone(),
                subject_id: None,
                subject_set: Some(SubjectSetBody {
                    namespace: subject_set.0.clone(),
                    object: subject_set.1.clone(),
                    relation: subject_set.2.clone(),
                }),
            },
        }
    }
}

#[async_trait]
impl PermissionStore for HttpKetoClient {
    async fn write(&self, tuples: &[Tuple]) -> Result<(), AccessError> {
        // Keto's admin API takes one relation tuple per request.
        for tuple in tuples {
            let body = TupleBody::from_tuple(tuple);
            let response = self
                .http
                .put(format!("{}/admin/relation-tuples", self.write_url))
                .json(&body)
                .send()
                .await
                .map_err(|err| AccessError::BackendUnreachable(err.to_string()))?;
            if !response.status().is_success() {
                return Err(AccessError::BackendRejected(format!(
                    "tuple write returned {}: {}",
                    response.status(),
                    response.text().await.unwrap_or_default()
                )));
            }
        }
        Ok(())
    }

    async fn delete(&self, tuples: &[Tuple]) -> Result<(), AccessError> {
        // Keto's admin API takes one relation tuple per request, as query
        // parameters (a JSON body is rejected).
        for tuple in tuples {
            let mut request = self
                .http
                .delete(format!("{}/admin/relation-tuples", self.write_url));
            match tuple {
                Tuple::Subject {
                    namespace,
                    object,
                    relation,
                    subject_id,
                } => {
                    request = request.query(&[
                        ("namespace", namespace.as_str()),
                        ("object", object.as_str()),
                        ("relation", relation.as_str()),
                        ("subject_id", &subject_id.to_string()),
                    ]);
                }
                Tuple::SubjectSet {
                    namespace,
                    object,
                    relation,
                    subject_set,
                } => {
                    request = request.query(&[
                        ("namespace", namespace.as_str()),
                        ("object", object.as_str()),
                        ("relation", relation.as_str()),
                        ("subject_set.namespace", subject_set.0.as_str()),
                        ("subject_set.object", subject_set.1.as_str()),
                        ("subject_set.relation", subject_set.2.as_str()),
                    ]);
                }
            }
            let response = request
                .send()
                .await
                .map_err(|err| AccessError::BackendUnreachable(err.to_string()))?;
            if !response.status().is_success() {
                return Err(AccessError::BackendRejected(format!(
                    "tuple delete returned {}: {}",
                    response.status(),
                    response.text().await.unwrap_or_default()
                )));
            }
        }
        Ok(())
    }

    async fn clear_namespace(&self, namespace: &str) -> Result<(), AccessError> {
        let response = self
            .http
            .delete(format!(
                "{}/admin/relation-tuples?namespace={namespace}",
                self.write_url
            ))
            .send()
            .await
            .map_err(|err| AccessError::BackendUnreachable(err.to_string()))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(AccessError::BackendRejected(format!(
                "namespace clear returned {}: {}",
                response.status(),
                response.text().await.unwrap_or_default()
            )))
        }
    }

    async fn check_membership(
        &self,
        user_id: UserId,
        objects: &[String],
    ) -> Result<Vec<bool>, AccessError> {
        // Keto v26 (legacy namespaces) exposes only the single check
        // endpoint; candidate-role sets are small and the decision cache
        // absorbs the cost.
        // TASK-023: one breaker report per check batch; an open breaker
        // fails fast with BackendUnreachable before any request is built.
        let mut permit = match self.breaker.acquire() {
            Ok(permit) => Some(permit),
            Err(_open) => {
                self.note_breaker_state();
                return Err(AccessError::BackendUnreachable(
                    "circuit breaker open after repeated Keto failures".to_string(),
                ));
            }
        };
        let mut results = Vec::with_capacity(objects.len());
        let mut first_error = None;
        for object in objects {
            let response = match self
                .http
                .get(format!("{}/relation-tuples/check", self.read_url))
                .query(&[
                    ("namespace", "role"),
                    ("object", object.as_str()),
                    ("relation", "member"),
                    ("subject_id", &user_id.to_string()),
                ])
                .send()
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    first_error = Some(AccessError::BackendUnreachable(err.to_string()));
                    break;
                }
            };
            if !response.status().is_success()
                && response.status() != reqwest::StatusCode::FORBIDDEN
            {
                first_error = Some(AccessError::BackendRejected(format!(
                    "check returned {}: {}",
                    response.status(),
                    response.text().await.unwrap_or_default()
                )));
                break;
            }
            // Keto's legacy check endpoint answers denials as
            // `403 {"allowed":false}` — a normal decision, not a failure.
            let parsed: CheckResponse = match response.json::<CheckResponse>().await {
                Ok(parsed) => parsed,
                Err(err) => {
                    first_error = Some(AccessError::BackendRejected(err.to_string()));
                    break;
                }
            };
            results.push(parsed.allowed);
        }
        let ok = first_error.is_none();
        if let Some(permit) = permit.take() {
            permit.report(ok);
        }
        self.note_breaker_state();
        match first_error {
            Some(err) => Err(err),
            None => Ok(results),
        }
    }
}

/// Errors from a failed batch check, keyed by index (used by the sync
/// consumer to log precisely which tuples failed).
#[derive(Debug, Clone, Default)]
pub struct KetoCheckError {
    pub index: usize,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuple_bodies_serialize_to_the_keto_shape() {
        let tuple = Tuple::role_membership(
            "acme-tenant:tenant_owner",
            UserId::try_from(101_i64).unwrap(),
        );
        let body = TupleBody::from_tuple(&tuple);
        let json = serde_json::to_value(body).unwrap();
        assert_eq!(json["namespace"], "role");
        assert_eq!(json["object"], "acme-tenant:tenant_owner");
        assert_eq!(json["relation"], "member");
        assert_eq!(json["subject_id"], "101");
        assert!(json.get("subject_set").is_none());

        let grant = Tuple::permission_grant("identity.member.read", "acme-tenant:tenant_owner");
        let json = serde_json::to_value(TupleBody::from_tuple(&grant)).unwrap();
        assert_eq!(json["subject_set"]["namespace"], "role");
        assert_eq!(json["subject_set"]["object"], "acme-tenant:tenant_owner");
        assert_eq!(json["subject_set"]["relation"], "member");
    }
}
