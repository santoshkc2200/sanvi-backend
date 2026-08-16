//! The Keto REST adapter: relation-tuple writes/deletes on the admin API,
//! batch membership checks on the read API.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::AccessError;
use crate::domain::ports::{PermissionStore, Tuple};

#[derive(Debug, Clone)]
pub struct HttpKetoClient {
    http: reqwest::Client,
    read_url: String,
    write_url: String,
}

impl HttpKetoClient {
    pub fn new(read_url: String, write_url: String) -> Result<Self, AccessError> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|err| AccessError::BackendUnreachable(err.to_string()))?;
        Ok(Self {
            http,
            read_url,
            write_url,
        })
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
        user_id: Uuid,
        objects: &[String],
    ) -> Result<Vec<bool>, AccessError> {
        // Keto v26 (legacy namespaces) exposes only the single check
        // endpoint; candidate-role sets are small and the decision cache
        // absorbs the cost.
        let mut results = Vec::with_capacity(objects.len());
        for object in objects {
            let response = self
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
                .map_err(|err| AccessError::BackendUnreachable(err.to_string()))?;
            if !response.status().is_success()
                && response.status() != reqwest::StatusCode::FORBIDDEN
            {
                return Err(AccessError::BackendRejected(format!(
                    "check returned {}: {}",
                    response.status(),
                    response.text().await.unwrap_or_default()
                )));
            }
            // Keto's legacy check endpoint answers denials as
            // `403 {"allowed":false}` — a normal decision, not a failure.
            let parsed: CheckResponse = response
                .json()
                .await
                .map_err(|err| AccessError::BackendRejected(err.to_string()))?;
            results.push(parsed.allowed);
        }
        Ok(results)
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
            Uuid::parse_str("0190f0d0-0000-7000-8000-000000000001").unwrap(),
        );
        let body = TupleBody::from_tuple(&tuple);
        let json = serde_json::to_value(body).unwrap();
        assert_eq!(json["namespace"], "role");
        assert_eq!(json["object"], "acme-tenant:tenant_owner");
        assert_eq!(json["relation"], "member");
        assert_eq!(json["subject_id"], "0190f0d0-0000-7000-8000-000000000001");
        assert!(json.get("subject_set").is_none());

        let grant = Tuple::permission_grant("identity.member.read", "acme-tenant:tenant_owner");
        let json = serde_json::to_value(TupleBody::from_tuple(&grant)).unwrap();
        assert_eq!(json["subject_set"]["namespace"], "role");
        assert_eq!(json["subject_set"]["object"], "acme-tenant:tenant_owner");
        assert_eq!(json["subject_set"]["relation"], "member");
    }
}
