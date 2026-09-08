//! `hitox-media-service` REST adapter: the outbound `reqwest` client behind
//! `MediaGatewayPort`.

use async_trait::async_trait;
use sanvi_kernel::TenantId;
use serde::Deserialize;
use std::time::Duration;

use crate::config::MediaConfig;
use crate::domain::{
    AssetDeliveryView, AssetUploadView, AssetView, CompletedPartInput, CreateAssetInput,
    MediaError, MediaGatewayPort, UploadPartView,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

pub struct HitoxMediaAdapter {
    http: reqwest::Client,
    base_url: String,
    service_api_key: String,
}

impl std::fmt::Debug for HitoxMediaAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HitoxMediaAdapter")
            .field("base_url", &self.base_url)
            .field("service_api_key", &"[REDACTED]")
            .finish()
    }
}

fn encode_path_segment(segment: &str) -> String {
    let mut encoded = String::with_capacity(segment.len());
    for b in segment.as_bytes() {
        match *b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*b as char);
            }
            byte => {
                use std::fmt::Write;
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

impl HitoxMediaAdapter {
    /// Infallible by design: `base_url` is checked once at boot by
    /// `AppConfig::validate`, which reports every configuration problem
    /// together rather than panicking on the first one.
    pub fn new(config: MediaConfig) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client builds with static config");
        Self {
            http,
            base_url: config.base_url,
            service_api_key: config.service_api_key,
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

    fn absolutize_url(&self, raw: &str) -> String {
        if raw.starts_with("http://") || raw.starts_with("https://") {
            raw.to_string()
        } else {
            let base = self.base_url.trim_end_matches('/');
            if raw.starts_with('/') {
                format!("{base}{raw}")
            } else {
                format!("{base}/{raw}")
            }
        }
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        tenant_id: TenantId,
    ) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.endpoint_url(path))
            .bearer_auth(&self.service_api_key)
            .header("X-Namespace-ID", tenant_id.to_string())
    }
}

#[derive(Deserialize)]
struct HitoxProblem {
    #[serde(default = "default_code")]
    code: String,
    #[serde(default = "default_detail")]
    detail: String,
}
fn default_code() -> String {
    "unknown_error".to_string()
}
fn default_detail() -> String {
    "hitox-media-service returned an unreadable error body".to_string()
}

async fn map_error(response: reqwest::Response) -> MediaError {
    let status = response.status();
    let problem: HitoxProblem = response.json().await.unwrap_or(HitoxProblem {
        code: default_code(),
        detail: default_detail(),
    });
    // 400/404/409 are `hitox-media-service`'s own tenant-facing validation
    // problems: their detail is written for the caller, so it passes
    // through as it always has. Everything else — auth failures above all —
    // is service-internal and is logged here but never echoed to the tenant.
    match status.as_u16() {
        400 => MediaError::BadRequest {
            code: problem.code,
            detail: problem.detail,
        },
        404 => MediaError::NotFound {
            code: problem.code,
            detail: problem.detail,
        },
        409 => MediaError::Conflict {
            code: problem.code,
            detail: problem.detail,
        },
        413 => {
            tracing::warn!(status = 413, code = %problem.code, detail = %problem.detail, "upstream payload too large");
            MediaError::BadRequest {
                code: problem.code,
                detail: "upload payload too large".to_string(),
            }
        }
        status_code if (400..500).contains(&status_code) => {
            if status_code == 401 || status_code == 403 {
                tracing::error!(status = status_code, code = %problem.code, detail = %problem.detail, "upstream service authentication failure");
                MediaError::Unavailable {
                    detail: "media service unavailable".to_string(),
                }
            } else {
                tracing::warn!(status = status_code, code = %problem.code, detail = %problem.detail, "upstream client error");
                MediaError::BadRequest {
                    code: problem.code,
                    detail: "invalid request".to_string(),
                }
            }
        }
        status_code => {
            tracing::error!(status = status_code, code = %problem.code, detail = %problem.detail, "upstream service error");
            MediaError::Unavailable {
                detail: "media service unavailable".to_string(),
            }
        }
    }
}

fn connection_failure(error: reqwest::Error) -> MediaError {
    tracing::error!(error = %error, "hitox-media-service connection error");
    MediaError::Unavailable {
        detail: "media service unavailable".to_string(),
    }
}

#[async_trait]
impl MediaGatewayPort for HitoxMediaAdapter {
    async fn create_asset(
        &self,
        tenant_id: TenantId,
        idempotency_key: &str,
        input: CreateAssetInput,
    ) -> Result<AssetUploadView, MediaError> {
        let response = self
            .request(reqwest::Method::POST, "/v1/assets", tenant_id)
            .header("Idempotency-Key", idempotency_key)
            .json(&input)
            .send()
            .await
            .map_err(connection_failure)?;
        if !response.status().is_success() {
            return Err(map_error(response).await);
        }
        response.json().await.map_err(connection_failure)
    }

    async fn presign_parts(
        &self,
        tenant_id: TenantId,
        asset_id: &str,
        part_numbers: Vec<i64>,
    ) -> Result<Vec<UploadPartView>, MediaError> {
        #[derive(serde::Serialize)]
        struct Req {
            part_numbers: Vec<i64>,
        }
        #[derive(Deserialize)]
        struct Resp {
            part_urls: Vec<UploadPartView>,
        }
        let encoded_id = encode_path_segment(asset_id);
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/v1/assets/{encoded_id}/parts"),
                tenant_id,
            )
            .json(&Req { part_numbers })
            .send()
            .await
            .map_err(connection_failure)?;
        if !response.status().is_success() {
            return Err(map_error(response).await);
        }
        Ok(response
            .json::<Resp>()
            .await
            .map_err(connection_failure)?
            .part_urls)
    }

    async fn complete_asset(
        &self,
        tenant_id: TenantId,
        asset_id: &str,
        parts: Vec<CompletedPartInput>,
    ) -> Result<AssetView, MediaError> {
        #[derive(serde::Serialize)]
        struct Req {
            parts: Vec<CompletedPartInput>,
        }
        let encoded_id = encode_path_segment(asset_id);
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/v1/assets/{encoded_id}/complete"),
                tenant_id,
            )
            .json(&Req { parts })
            .send()
            .await
            .map_err(connection_failure)?;
        if !response.status().is_success() {
            return Err(map_error(response).await);
        }
        response.json().await.map_err(connection_failure)
    }

    async fn get_asset(
        &self,
        tenant_id: TenantId,
        asset_id: &str,
    ) -> Result<AssetView, MediaError> {
        let encoded_id = encode_path_segment(asset_id);
        let response = self
            .request(
                reqwest::Method::GET,
                &format!("/v1/assets/{encoded_id}"),
                tenant_id,
            )
            .send()
            .await
            .map_err(connection_failure)?;
        if !response.status().is_success() {
            return Err(map_error(response).await);
        }
        response.json().await.map_err(connection_failure)
    }

    async fn delete_asset(&self, tenant_id: TenantId, asset_id: &str) -> Result<(), MediaError> {
        let encoded_id = encode_path_segment(asset_id);
        let response = self
            .request(
                reqwest::Method::DELETE,
                &format!("/v1/assets/{encoded_id}"),
                tenant_id,
            )
            .send()
            .await
            .map_err(connection_failure)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(map_error(response).await)
        }
    }

    async fn create_delivery(
        &self,
        tenant_id: TenantId,
        asset_id: &str,
    ) -> Result<AssetDeliveryView, MediaError> {
        let encoded_id = encode_path_segment(asset_id);
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/v1/assets/{encoded_id}/delivery"),
                tenant_id,
            )
            .send()
            .await
            .map_err(connection_failure)?;
        if !response.status().is_success() {
            return Err(map_error(response).await);
        }
        let mut view: AssetDeliveryView = response.json().await.map_err(connection_failure)?;
        if let Some(ref url) = view.url {
            view.url = Some(self.absolutize_url(url));
        }
        for rendition in &mut view.renditions {
            if let Some(ref url) = rendition.url {
                rendition.url = Some(self.absolutize_url(url));
            }
        }
        Ok(view)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use sanvi_kernel::TenantId;

    use crate::domain::{CreateAssetInput, MediaGatewayPort};

    #[derive(Clone)]
    struct RecordedRequest {
        method: String,
        path: String,
        headers: BTreeMap<String, String>,
        #[allow(dead_code)]
        body: Vec<u8>,
    }

    impl RecordedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .get(&name.to_ascii_lowercase())
                .map(String::as_str)
        }
    }

    struct MockServer {
        pub addr: std::net::SocketAddr,
        recorded: Arc<Mutex<Vec<RecordedRequest>>>,
        shutdown: tokio::sync::oneshot::Sender<()>,
    }

    impl MockServer {
        async fn start(responses: Vec<(u16, serde_json::Value)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let recorded = Arc::new(Mutex::new(Vec::new()));
            let recorded_cloned = recorded.clone();
            let (shutdown, mut rx) = tokio::sync::oneshot::channel::<()>();

            tokio::spawn(async move {
                let mut response_iter = responses.into_iter();
                loop {
                    tokio::select! {
                        _ = &mut rx => break,
                        accept_res = listener.accept() => {
                            let Ok((mut socket, _)) = accept_res else {
                                break;
                            };
                            let (status, resp_json) = response_iter.next().unwrap_or((404, serde_json::json!({})));
                            let mut buf = vec![0u8; 8192];
                            let n = socket.read(&mut buf).await.unwrap_or(0);
                            if n == 0 {
                                continue;
                            }
                            let head = String::from_utf8_lossy(&buf[..n]).to_string();
                            let mut lines = head.split("\r\n");
                            let req_line = lines.next().unwrap_or("");
                            let parts: Vec<&str> = req_line.split_whitespace().collect();
                            let method = parts.first().unwrap_or(&"").to_string();
                            let path = parts.get(1).unwrap_or(&"").to_string();

                            let mut headers = BTreeMap::new();
                            let mut content_length = 0usize;
                            for line in lines {
                                if line.is_empty() {
                                    break;
                                }
                                if let Some((k, v)) = line.split_once(':') {
                                    let k = k.trim().to_ascii_lowercase();
                                    let v = v.trim().to_string();
                                    if k == "content-length" {
                                        content_length = v.parse().unwrap_or(0);
                                    }
                                    headers.insert(k, v);
                                }
                            }

                            let body_start = head.find("\r\n\r\n").map(|p| p + 4).unwrap_or(n);
                            let body = buf[body_start..n.min(body_start + content_length)].to_vec();

                            recorded_cloned.lock().unwrap().push(RecordedRequest {
                                method,
                                path,
                                headers,
                                body,
                            });

                            let resp_body = serde_json::to_string(&resp_json).unwrap();
                            let http_response = format!(
                                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                                resp_body.len()
                            );
                            let _ = socket.write_all(http_response.as_bytes()).await;
                        }
                    }
                }
            });

            Self {
                addr,
                recorded,
                shutdown,
            }
        }

        fn recorded(&self) -> Vec<RecordedRequest> {
            self.recorded.lock().unwrap().clone()
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            let (shutdown, _) = tokio::sync::oneshot::channel();
            let old = std::mem::replace(&mut self.shutdown, shutdown);
            let _ = old.send(());
        }
    }

    fn test_tenant() -> TenantId {
        TenantId::try_from(1_i64).unwrap()
    }

    #[tokio::test]
    async fn create_asset_sends_bearer_key_and_namespace_header_and_returns_parsed_view() {
        let mock = MockServer::start(vec![(
            201,
            serde_json::json!({
                "asset_id": "a1", "expires_at": "2026-09-08T00:00:00Z",
                "upload_url": "https://s3.example/put"
            }),
        )])
        .await;
        let adapter = super::HitoxMediaAdapter::new(crate::config::MediaConfig {
            base_url: format!("http://{}", mock.addr),
            service_api_key: "test-key".to_string(),
        });

        let view = adapter
            .create_asset(
                test_tenant(),
                "idem-1",
                CreateAssetInput {
                    kind: "image".to_string(),
                    content_type: "image/png".to_string(),
                    filename: "a.png".to_string(),
                    size_bytes: 100,
                    visibility: crate::domain::AssetVisibility::Private,
                    owner_id: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(view.asset_id, "a1");
        let recorded = mock.recorded();
        assert_eq!(recorded[0].method, "POST");
        assert_eq!(recorded[0].path, "/v1/assets");
        assert_eq!(recorded[0].header("authorization"), Some("Bearer test-key"));
        assert_eq!(
            recorded[0].header("x-namespace-id"),
            Some(test_tenant().to_string().as_str())
        );
        assert_eq!(recorded[0].header("idempotency-key"), Some("idem-1"));
        let body: serde_json::Value = serde_json::from_slice(&recorded[0].body).unwrap();
        assert_eq!(body["visibility"], "private");
        assert_eq!(body["kind"], "image");
    }

    #[tokio::test]
    async fn presign_parts_posts_part_numbers_and_returns_parsed_urls() {
        let mock = MockServer::start(vec![(
            200,
            serde_json::json!({
                "part_urls": [
                    {"part_number": 1, "url": "https://s3.example/part1"},
                    {"part_number": 2, "url": "https://s3.example/part2"}
                ]
            }),
        )])
        .await;
        let adapter = super::HitoxMediaAdapter::new(crate::config::MediaConfig {
            base_url: format!("http://{}", mock.addr),
            service_api_key: "test-key".to_string(),
        });

        let parts = adapter
            .presign_parts(test_tenant(), "asset-1", vec![1, 2])
            .await
            .unwrap();

        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].part_number, 1);
        assert_eq!(parts[0].url, "https://s3.example/part1");
        assert_eq!(parts[1].part_number, 2);
        let recorded = mock.recorded();
        assert_eq!(recorded[0].method, "POST");
        assert_eq!(recorded[0].path, "/v1/assets/asset-1/parts");
        let body: serde_json::Value = serde_json::from_slice(&recorded[0].body).unwrap();
        assert_eq!(body, serde_json::json!({"part_numbers": [1, 2]}));
    }

    #[tokio::test]
    async fn complete_asset_posts_parts_and_returns_parsed_asset_view() {
        let mock = MockServer::start(vec![(
            200,
            serde_json::json!({
                "id": "asset-1",
                "kind": "image",
                "status": "ready",
                "progress_percent": 100,
                "version": 3
            }),
        )])
        .await;
        let adapter = super::HitoxMediaAdapter::new(crate::config::MediaConfig {
            base_url: format!("http://{}", mock.addr),
            service_api_key: "test-key".to_string(),
        });

        let view = adapter
            .complete_asset(
                test_tenant(),
                "asset-1",
                vec![crate::domain::CompletedPartInput {
                    part_number: 1,
                    etag: "etag-1".to_string(),
                }],
            )
            .await
            .unwrap();

        assert_eq!(view.id, "asset-1");
        assert_eq!(view.status, "ready");
        assert_eq!(view.version, 3);
        let recorded = mock.recorded();
        assert_eq!(recorded[0].method, "POST");
        assert_eq!(recorded[0].path, "/v1/assets/asset-1/complete");
        let body: serde_json::Value = serde_json::from_slice(&recorded[0].body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"parts": [{"part_number": 1, "etag": "etag-1"}]})
        );
    }

    #[tokio::test]
    async fn get_asset_sends_get_and_returns_parsed_asset_view() {
        let mock = MockServer::start(vec![(
            200,
            serde_json::json!({
                "id": "asset-1",
                "kind": "video",
                "status": "processing",
                "progress_percent": 42,
                "version": 1
            }),
        )])
        .await;
        let adapter = super::HitoxMediaAdapter::new(crate::config::MediaConfig {
            base_url: format!("http://{}", mock.addr),
            service_api_key: "test-key".to_string(),
        });

        let view = adapter.get_asset(test_tenant(), "asset-1").await.unwrap();

        assert_eq!(view.id, "asset-1");
        assert_eq!(view.status, "processing");
        assert_eq!(view.progress_percent, 42);
        let recorded = mock.recorded();
        assert_eq!(recorded[0].method, "GET");
        assert_eq!(recorded[0].path, "/v1/assets/asset-1");
    }

    #[tokio::test]
    async fn delete_asset_sends_delete_and_returns_ok_on_success() {
        let mock = MockServer::start(vec![(204, serde_json::json!({}))]).await;
        let adapter = super::HitoxMediaAdapter::new(crate::config::MediaConfig {
            base_url: format!("http://{}", mock.addr),
            service_api_key: "test-key".to_string(),
        });

        adapter
            .delete_asset(test_tenant(), "asset-1")
            .await
            .unwrap();

        let recorded = mock.recorded();
        assert_eq!(recorded[0].method, "DELETE");
        assert_eq!(recorded[0].path, "/v1/assets/asset-1");
    }

    #[tokio::test]
    async fn create_delivery_posts_and_returns_parsed_hls_delivery_view_with_absolutized_url() {
        let mock = MockServer::start(vec![(
            200,
            serde_json::json!({
                "visibility": "private",
                "url": "/v1/delivery/asset-1/master.m3u8?token=xyz",
                "expires_at": "2026-09-08T00:00:00Z"
            }),
        )])
        .await;
        let adapter = super::HitoxMediaAdapter::new(crate::config::MediaConfig {
            base_url: format!("http://{}", mock.addr),
            service_api_key: "test-key".to_string(),
        });

        let view = adapter
            .create_delivery(test_tenant(), "asset-1")
            .await
            .unwrap();

        assert_eq!(
            view.url.as_deref(),
            Some(
                format!(
                    "http://{}/v1/delivery/asset-1/master.m3u8?token=xyz",
                    mock.addr
                )
                .as_str()
            )
        );
        let recorded = mock.recorded();
        assert_eq!(recorded[0].method, "POST");
        assert_eq!(recorded[0].path, "/v1/assets/asset-1/delivery");
    }

    #[tokio::test]
    async fn create_delivery_image_returns_renditions_with_absolutized_and_untouched_urls() {
        let mock = MockServer::start(vec![(
            200,
            serde_json::json!({
                "visibility": "private",
                "expires_at": "2026-09-08T00:00:00Z",
                "renditions": [
                    {
                        "name": "thumb",
                        "content_type": "image/jpeg",
                        "url": "/v1/public/ns-1/asset-1/thumb",
                        "width": 320,
                        "height": 240
                    },
                    {
                        "name": "full",
                        "content_type": "image/png",
                        "url": "https://s3.example.com/bucket/full.png?token=presigned"
                    }
                ]
            }),
        )])
        .await;
        let adapter = super::HitoxMediaAdapter::new(crate::config::MediaConfig {
            base_url: format!("http://{}", mock.addr),
            service_api_key: "test-key".to_string(),
        });

        let view = adapter
            .create_delivery(test_tenant(), "asset-1")
            .await
            .unwrap();

        assert!(view.url.is_none());
        assert_eq!(view.renditions.len(), 2);
        assert_eq!(
            view.renditions[0].url.as_deref(),
            Some(format!("http://{}/v1/public/ns-1/asset-1/thumb", mock.addr).as_str())
        );
        assert_eq!(view.renditions[0].width, Some(320));
        assert_eq!(view.renditions[0].height, Some(240));
        assert_eq!(
            view.renditions[1].url.as_deref(),
            Some("https://s3.example.com/bucket/full.png?token=presigned")
        );
    }

    #[tokio::test]
    async fn path_traversal_encoding_protects_upstream_routes() {
        let mock = MockServer::start(vec![
            (
                200,
                serde_json::json!({
                    "id": "1/../../healthz",
                    "kind": "video",
                    "status": "ready",
                    "progress_percent": 100,
                    "version": 1
                }),
            ),
            (204, serde_json::json!({})),
        ])
        .await;
        let adapter = super::HitoxMediaAdapter::new(crate::config::MediaConfig {
            base_url: format!("http://{}", mock.addr),
            service_api_key: "test-key".to_string(),
        });

        let _ = adapter
            .get_asset(test_tenant(), "1/../../healthz")
            .await
            .unwrap();
        adapter
            .delete_asset(test_tenant(), "asset?evil=true#sec")
            .await
            .unwrap();

        let recorded = mock.recorded();
        assert_eq!(recorded[0].path, "/v1/assets/1%2F..%2F..%2Fhealthz");
        assert_eq!(recorded[1].path, "/v1/assets/asset%3Fevil%3Dtrue%23sec");
    }

    #[tokio::test]
    async fn base_url_trailing_slash_is_joined_robustly() {
        let mock = MockServer::start(vec![(
            200,
            serde_json::json!({
                "id": "asset-1",
                "kind": "video",
                "status": "ready",
                "progress_percent": 100,
                "version": 1
            }),
        )])
        .await;
        // base_url with trailing slash
        let adapter = super::HitoxMediaAdapter::new(crate::config::MediaConfig {
            base_url: format!("http://{}/", mock.addr),
            service_api_key: "test-key".to_string(),
        });

        let _ = adapter.get_asset(test_tenant(), "asset-1").await.unwrap();

        let recorded = mock.recorded();
        assert_eq!(recorded[0].path, "/v1/assets/asset-1");
    }

    #[tokio::test]
    async fn an_error_status_maps_to_the_matching_media_error_variant_with_code_preserved() {
        let mock = MockServer::start(vec![(
            404,
            serde_json::json!({"code": "not_found", "detail": "asset gone"}),
        )])
        .await;
        let adapter = super::HitoxMediaAdapter::new(crate::config::MediaConfig {
            base_url: format!("http://{}", mock.addr),
            service_api_key: "test-key".to_string(),
        });

        let err = adapter
            .get_asset(test_tenant(), "asset-1")
            .await
            .unwrap_err();

        assert!(
            matches!(&err, crate::domain::MediaError::NotFound { code, detail } if code == "not_found" && detail == "asset gone"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn error_detail_redaction_hides_service_secret_and_maps_413() {
        let mock = MockServer::start(vec![
            (
                401,
                serde_json::json!({
                    "code": "unauthenticated",
                    "detail": "valid service API key required"
                }),
            ),
            (
                413,
                serde_json::json!({
                    "code": "upload_too_large",
                    "detail": "upload size 1000 exceeds maximum 500"
                }),
            ),
            (
                500,
                serde_json::json!({
                    "code": "internal_error",
                    "detail": "fatal postgres connection failure"
                }),
            ),
            (
                400,
                serde_json::json!({
                    "code": "invalid_input",
                    "detail": "kind must be one of image, video, audio"
                }),
            ),
        ])
        .await;
        let adapter = super::HitoxMediaAdapter::new(crate::config::MediaConfig {
            base_url: format!("http://{}", mock.addr),
            service_api_key: "test-key".to_string(),
        });

        // 1. 401 unauthenticated
        let err401 = adapter
            .get_asset(test_tenant(), "asset-1")
            .await
            .unwrap_err();
        assert!(matches!(
            err401,
            crate::domain::MediaError::Unavailable { .. }
        ));
        assert!(
            !err401
                .to_string()
                .contains("valid service API key required")
        );
        assert!(err401.to_string().contains("media service unavailable"));

        // 2. 413 upload too large
        let err413 = adapter
            .get_asset(test_tenant(), "asset-1")
            .await
            .unwrap_err();
        assert!(
            matches!(&err413, crate::domain::MediaError::BadRequest { code, detail } if code == "upload_too_large" && detail == "upload payload too large"),
            "unexpected error: {err413:?}"
        );
        assert!(!err413.to_string().contains("1000 exceeds"));

        // 3. 500 internal error
        let err500 = adapter
            .get_asset(test_tenant(), "asset-1")
            .await
            .unwrap_err();
        assert!(matches!(
            err500,
            crate::domain::MediaError::Unavailable { .. }
        ));
        assert!(!err500.to_string().contains("fatal postgres"));

        // 4. 400 is the media service's own tenant-facing validation
        // message: it must survive, or the caller loses the only
        // explanation of what was wrong with the request.
        let err400 = adapter
            .get_asset(test_tenant(), "asset-1")
            .await
            .unwrap_err();
        assert!(
            matches!(&err400, crate::domain::MediaError::BadRequest { code, detail } if code == "invalid_input" && detail == "kind must be one of image, video, audio"),
            "unexpected error: {err400:?}"
        );
    }

    #[tokio::test]
    async fn a_connection_failure_maps_to_unavailable() {
        // Bind then immediately drop a listener to obtain a free port with
        // nothing listening on it.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let adapter = super::HitoxMediaAdapter::new(crate::config::MediaConfig {
            base_url: format!("http://{addr}"),
            service_api_key: "test-key".to_string(),
        });

        let err = adapter
            .get_asset(test_tenant(), "asset-1")
            .await
            .unwrap_err();

        assert!(
            matches!(err, crate::domain::MediaError::Unavailable { .. }),
            "unexpected error: {err:?}"
        );
    }
}
