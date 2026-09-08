# Media Asset Gateway Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the admin SPA's Kratos-cookie session a way to authenticate to `hitox-media-service`, by adding a session-authenticated, tenant-scoped proxy in `sanvi-backend` that mirrors the 6 `/v1/assets` endpoints `course-media`'s `api/assets.ts` already calls, injecting the service's `Bearer` key and `X-Namespace-ID` itself.

**Architecture:** New bounded context `crates/contexts/media` (`sanvi-media`), same shape as `advertising`'s outbound adapters: `domain` (port trait + error/DTO types, no `reqwest`/`actix`), `application` (one thin use-case struct per operation), `adapters/hitox.rs` (the `reqwest` client implementing the port), `adapters/http` (actix handlers/routes/state). Routes mount under `/api/v1/tenant/media` so `course-media`'s hardcoded `/v1/assets...` paths resolve correctly with `VITE_MEDIA_ORIGIN` set to `<backend origin>/api/v1/tenant/media` — no `sanvi-frontend` change. `hitox-media-service` is untouched.

**Tech Stack:** Rust, actix-web, `reqwest` (outbound), `sanvi-http` (ApiError/CurrentTenant/RequiredPermission/PermissionMap), `utoipa` (OpenAPI), `thiserror`, `async-trait`.

**Spec:** `docs/tasks/phase-10/TASK-032-10-4a-media-asset-gateway.md`

## Global Constraints

- CONTRIBUTING.md rule #1 (layering): `domain/` imports nothing but `sanvi-kernel` + std/serde — no `reqwest`/`actix` there. `just layering` enforces this.
- CONTRIBUTING.md rule #2 (errors): `sanvi_http::ApiError`/`ProblemDetail` (RFC 9457) is the only error body this API produces. No raw upstream JSON relay.
- CONTRIBUTING.md rule #3 (contracts): handler changes require `api/media.yaml` regenerated via `cargo run -p sanvi-cli -- openapi --write` and committed.
- CONTRIBUTING.md rule #7 (IDs): no new first-party ID type needed — every id this context handles is either `sanvi_kernel::TenantId` (existing) or an opaque `hitox-media-service` asset id string we never parse.
- No transactions/persistence: this context is a stateless relay — CONTRIBUTING rule #4 (Tx/outbox) does not apply, nothing here writes to Postgres.
- The gateway never trusts a client-supplied `X-Tenant-ID`/`X-Namespace-ID` header (both are sent today by `courseApiRequest`, a holdover from a different backend) — the tenant always comes from the session-resolved `CurrentTenant`.
- `service_api_key` must never appear in a log line, trace attribute, or response body — redact it in `HitoxMediaAdapter`'s `Debug` impl, the same pattern `MetaAdsAdapter` uses for `client_secret` (`crates/contexts/advertising/src/adapters/meta.rs:44-53`).

---

### Task 1: Scaffold the `sanvi-media` crate and its domain types

**Files:**
- Create: `crates/contexts/media/Cargo.toml`
- Create: `crates/contexts/media/src/lib.rs`
- Create: `crates/contexts/media/src/domain/mod.rs`
- Create: `crates/contexts/media/src/domain/error.rs`
- Create: `crates/contexts/media/src/domain/port.rs`
- Create: `crates/contexts/media/src/domain/dto.rs`
- Modify: `Cargo.toml:3-27` (workspace `members`) and `Cargo.toml:61-64`-area (`[workspace.dependencies]` path alias)
- Test: `crates/contexts/media/src/domain/error.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Produces: `MediaError` enum, `MediaGatewayPort` trait, and every DTO type later tasks depend on (exact field lists below — these mirror `course-media`'s `api/assets.ts` wire shapes verbatim, not `hitox-media-service`'s full internal contract, since that's the only shape any caller actually uses today).

- [ ] **Step 1: Add the crate to the workspace**

In `Cargo.toml`, add `"crates/contexts/media",` to the `members` array (next to `"crates/contexts/advertising",`), and add to `[workspace.dependencies]`:

```toml
sanvi-media = { path = "crates/contexts/media" }
```

- [ ] **Step 2: Write `crates/contexts/media/Cargo.toml`**

```toml
[package]
name = "sanvi-media"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
publish.workspace = true

[dependencies]
actix-web.workspace = true
async-trait.workspace = true
reqwest.workspace = true
sanvi-http.workspace = true
sanvi-kernel.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tokio.workspace = true
tracing.workspace = true
utoipa.workspace = true

[dev-dependencies]
tokio.workspace = true
```

- [ ] **Step 3: Write the failing test — error → wire-code mapping stays lossless**

`crates/contexts/media/src/domain/error.rs`:

```rust
use thiserror::Error;

/// Every failure this context's port can return. `code` carries
/// `hitox-media-service`'s own machine-readable error code through
/// unmodified — `course-media`'s `CourseApiError` already reads it, and
/// this context does not get to invent a competing vocabulary.
#[derive(Debug, Clone, Error)]
pub enum MediaError {
    #[error("{detail}")]
    BadRequest { code: String, detail: String },
    #[error("{detail}")]
    NotFound { code: String, detail: String },
    #[error("{detail}")]
    Conflict { code: String, detail: String },
    #[error("hitox-media-service unavailable: {detail}")]
    Unavailable { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_variant_keeps_its_own_message_in_display() {
        let err = MediaError::NotFound {
            code: "not_found".to_string(),
            detail: "asset abc123 does not exist".to_string(),
        };
        assert_eq!(err.to_string(), "asset abc123 does not exist");
    }
}
```

- [ ] **Step 4: Run it**

Run: `cargo test -p sanvi-media` (from `crates/contexts/media/`, or with `-p sanvi-media` from repo root)
Expected: compiles once `Cargo.toml`/`lib.rs`/`domain/mod.rs` exist (Step 6), then PASSES.

- [ ] **Step 5: Write the DTOs** (`crates/contexts/media/src/domain/dto.rs`) — field names match `course-media/src/api/assets.ts`'s wire types and `hitox-media-service/internal/media/application/dto.go` exactly:

```rust
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateAssetInput {
    pub kind: String,
    pub content_type: String,
    pub filename: String,
    pub size_bytes: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UploadPartView {
    pub part_number: i64,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AssetUploadView {
    pub asset_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_urls: Option<Vec<UploadPartView>>,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CompletedPartInput {
    pub part_number: i64,
    pub etag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AssetView {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,
    pub kind: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    pub progress_percent: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub version: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AssetDeliveryView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}
```

- [ ] **Step 6: Write the port trait** (`crates/contexts/media/src/domain/port.rs`) — pure, no `reqwest`:

```rust
use async_trait::async_trait;
use sanvi_kernel::TenantId;

use super::dto::{AssetDeliveryView, AssetUploadView, AssetView, CompletedPartInput, CreateAssetInput, UploadPartView};
use super::error::MediaError;

/// Driven port to the generic media service. `tenant_id` is converted to
/// `hitox-media-service`'s opaque `X-Namespace-ID` by the implementation,
/// never by a caller — see Global Constraints.
#[async_trait]
pub trait MediaGatewayPort: Send + Sync {
    async fn create_asset(
        &self,
        tenant_id: TenantId,
        idempotency_key: &str,
        input: CreateAssetInput,
    ) -> Result<AssetUploadView, MediaError>;

    async fn presign_parts(
        &self,
        tenant_id: TenantId,
        asset_id: &str,
        part_numbers: Vec<i64>,
    ) -> Result<Vec<UploadPartView>, MediaError>;

    async fn complete_asset(
        &self,
        tenant_id: TenantId,
        asset_id: &str,
        parts: Vec<CompletedPartInput>,
    ) -> Result<AssetView, MediaError>;

    async fn get_asset(&self, tenant_id: TenantId, asset_id: &str) -> Result<AssetView, MediaError>;

    async fn delete_asset(&self, tenant_id: TenantId, asset_id: &str) -> Result<(), MediaError>;

    async fn create_delivery(
        &self,
        tenant_id: TenantId,
        asset_id: &str,
    ) -> Result<AssetDeliveryView, MediaError>;
}
```

- [ ] **Step 7: `domain/mod.rs` and `lib.rs`**

`crates/contexts/media/src/domain/mod.rs`:
```rust
pub mod dto;
pub mod error;
pub mod port;

pub use dto::*;
pub use error::MediaError;
pub use port::MediaGatewayPort;
```

`crates/contexts/media/src/lib.rs`:
```rust
pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use config::MediaConfig;

pub mod routes {
    pub use crate::adapters::http::routes::media_tenant_scope_routes;
}
```

(This references `adapters`, `application`, `config` modules that don't exist yet — the crate won't compile until Tasks 2–4 add them. That's expected; `cargo test -p sanvi-media` in Step 4 only needs to pass once `domain` alone compiles, so temporarily comment out the `adapters`/`application`/`config`/`routes` lines in this step and uncomment each as its task lands, or do Step 4's test run after Task 2 instead — either is fine, note which you chose.)

- [ ] **Step 8: Run the full crate test again, then commit**

```bash
cargo test -p sanvi-media
git add Cargo.toml crates/contexts/media
git commit -m "feat(media): scaffold sanvi-media crate with domain types"
```

---

### Task 2: `HitoxMediaAdapter` — the outbound `reqwest` client

**Files:**
- Create: `crates/contexts/media/src/adapters/mod.rs`
- Create: `crates/contexts/media/src/adapters/hitox.rs`
- Create: `crates/contexts/media/src/config.rs`

**Interfaces:**
- Consumes: `MediaGatewayPort`, every DTO/`MediaError` from Task 1.
- Produces: `HitoxMediaAdapter::new(config: MediaConfig) -> Self`, implementing `MediaGatewayPort`. `MediaConfig { base_url: String, service_api_key: String }`.

- [ ] **Step 1: `config.rs`**

```rust
use serde::{Deserialize, Serialize};

/// `hitox-media-service` connection. Both fields are deployment secrets/
/// endpoints — set from env (`SANVI__MEDIA__BASE_URL`,
/// `SANVI__MEDIA__SERVICE_API_KEY`), never committed. An empty
/// `service_api_key` is a valid (if useless) default so the process still
/// boots in dev without the service configured — every call then fails
/// closed with `MediaError::Unavailable`, never silently unauthenticated.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MediaConfig {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub service_api_key: String,
}
```

- [ ] **Step 2: Write the failing adapter test** — adapt the hand-rolled `MockServer` from `crates/contexts/advertising/src/adapters/meta.rs:1349-1430` (bind `127.0.0.1:0`, record method/path/authorization/body, reply from a canned `(status, json)` queue) into `crates/contexts/media/src/adapters/hitox.rs`'s own `#[cfg(test)] mod tests`. First test:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use sanvi_kernel::TenantId;

    // ... MockServer struct/impl adapted from meta.rs, `RecordedRequest { method, path, authorization, body }` ...

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
        let adapter = HitoxMediaAdapter::new(MediaConfig {
            base_url: format!("http://{}", mock.addr),
            service_api_key: "test-key".to_string(),
        });

        let view = adapter
            .create_asset(
                TenantId::from(42u64), // exact constructor: match whatever Task 1's dto tests/other contexts use for TenantId in tests — grep `TenantId::` usage in advertising's tests for the real one before writing this line
                "idem-1",
                CreateAssetInput {
                    kind: "image".to_string(),
                    content_type: "image/png".to_string(),
                    filename: "a.png".to_string(),
                    size_bytes: 100,
                    owner_id: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(view.asset_id, "a1");
        let recorded = mock.recorded();
        assert_eq!(recorded[0].method, "POST");
        assert_eq!(recorded[0].path, "/v1/assets");
        assert_eq!(recorded[0].authorization.as_deref(), Some("Bearer test-key"));
        // namespace header assertion needs the mock harness extended to also
        // capture X-Namespace-ID and Idempotency-Key, not just Authorization —
        // extend `RecordedRequest`/the header-parsing loop copied from meta.rs
        // to record every header into a `BTreeMap<String, String>` instead of
        // only `authorization`, then assert on that map here.
    }
}
```

Before writing the `TenantId::from(42u64)` line for real, grep `crates/contexts/advertising/src -e "TenantId::"` for how existing tests construct a `TenantId` in-process and use that exact constructor — do not guess it.

- [ ] **Step 3: Run it, confirm it fails** (adapter doesn't exist yet)

Run: `cargo test -p sanvi-media create_asset_sends_bearer_key -- --nocapture`
Expected: FAIL — `HitoxMediaAdapter` not found.

- [ ] **Step 4: Implement `HitoxMediaAdapter`**

```rust
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

impl HitoxMediaAdapter {
    pub fn new(config: MediaConfig) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client builds with static config");
        Self { http, base_url: config.base_url, service_api_key: config.service_api_key }
    }

    fn request(&self, method: reqwest::Method, path: &str, tenant_id: TenantId) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base_url, path))
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
fn default_code() -> String { "unknown_error".to_string() }
fn default_detail() -> String { "hitox-media-service returned an unreadable error body".to_string() }

async fn map_error(response: reqwest::Response) -> MediaError {
    let status = response.status();
    let problem: HitoxProblem = response.json().await.unwrap_or(HitoxProblem {
        code: default_code(),
        detail: default_detail(),
    });
    match status.as_u16() {
        400 => MediaError::BadRequest { code: problem.code, detail: problem.detail },
        404 => MediaError::NotFound { code: problem.code, detail: problem.detail },
        409 => MediaError::Conflict { code: problem.code, detail: problem.detail },
        other => MediaError::Unavailable {
            detail: format!("hitox-media-service returned {other}: {}", problem.detail),
        },
    }
}

fn connection_failure(error: reqwest::Error) -> MediaError {
    MediaError::Unavailable { detail: error.to_string() }
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
        struct Req { part_numbers: Vec<i64> }
        #[derive(Deserialize)]
        struct Resp { part_urls: Vec<UploadPartView> }
        let response = self
            .request(reqwest::Method::POST, &format!("/v1/assets/{asset_id}/parts"), tenant_id)
            .json(&Req { part_numbers })
            .send()
            .await
            .map_err(connection_failure)?;
        if !response.status().is_success() {
            return Err(map_error(response).await);
        }
        Ok(response.json::<Resp>().await.map_err(connection_failure)?.part_urls)
    }

    async fn complete_asset(
        &self,
        tenant_id: TenantId,
        asset_id: &str,
        parts: Vec<CompletedPartInput>,
    ) -> Result<AssetView, MediaError> {
        #[derive(serde::Serialize)]
        struct Req { parts: Vec<CompletedPartInput> }
        let response = self
            .request(reqwest::Method::POST, &format!("/v1/assets/{asset_id}/complete"), tenant_id)
            .json(&Req { parts })
            .send()
            .await
            .map_err(connection_failure)?;
        if !response.status().is_success() {
            return Err(map_error(response).await);
        }
        response.json().await.map_err(connection_failure)
    }

    async fn get_asset(&self, tenant_id: TenantId, asset_id: &str) -> Result<AssetView, MediaError> {
        let response = self
            .request(reqwest::Method::GET, &format!("/v1/assets/{asset_id}"), tenant_id)
            .send()
            .await
            .map_err(connection_failure)?;
        if !response.status().is_success() {
            return Err(map_error(response).await);
        }
        response.json().await.map_err(connection_failure)
    }

    async fn delete_asset(&self, tenant_id: TenantId, asset_id: &str) -> Result<(), MediaError> {
        let response = self
            .request(reqwest::Method::DELETE, &format!("/v1/assets/{asset_id}"), tenant_id)
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
        let response = self
            .request(reqwest::Method::POST, &format!("/v1/assets/{asset_id}/delivery"), tenant_id)
            .send()
            .await
            .map_err(connection_failure)?;
        if !response.status().is_success() {
            return Err(map_error(response).await);
        }
        response.json().await.map_err(connection_failure)
    }
}
```

`crates/contexts/media/src/adapters/mod.rs`:
```rust
pub mod hitox;
pub mod http;

pub use hitox::HitoxMediaAdapter;
```
(`pub mod http;` is created in Task 4 — add it here now, its contents land later.)

- [ ] **Step 5: Run the test, confirm it passes**

Run: `cargo test -p sanvi-media create_asset_sends_bearer_key -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Add one test per remaining method** (`presign_parts`, `complete_asset`, `get_asset`, `delete_asset`, `create_delivery`) following the same `MockServer` pattern — one canned response, assert the request path/method and the parsed return value. Also add:
  - `an_error_status_maps_to_the_matching_media_error_variant_with_code_preserved` — mock returns `(404, {"code": "not_found", "detail": "asset gone"})`, assert `matches!(err, MediaError::NotFound { code, detail } if code == "not_found" && detail == "asset gone")`.
  - `a_connection_failure_maps_to_unavailable` — point `base_url` at a closed port (bind and immediately drop a `TcpListener` to get a free port nothing is listening on), assert `matches!(err, MediaError::Unavailable { .. })`.

- [ ] **Step 7: Run the full adapter test module, then commit**

```bash
cargo test -p sanvi-media adapters::hitox
git add crates/contexts/media/src/adapters crates/contexts/media/src/config.rs
git commit -m "feat(media): HitoxMediaAdapter reqwest client with error mapping"
```

---

### Task 3: Application use cases

**Files:**
- Create: `crates/contexts/media/src/application/mod.rs`

**Interfaces:**
- Consumes: `MediaGatewayPort` (Task 1), `HitoxMediaAdapter` only in tests via a fake.
- Produces: `CreateAssetUpload`, `PresignAssetParts`, `CompleteAssetUpload`, `GetMediaAsset`, `DeleteMediaAsset`, `CreateAssetDelivery` — each `new(gateway: Arc<dyn MediaGatewayPort>) -> Self` + `execute(...)` matching the port method it wraps one-to-one. Task 4's `MediaState` holds `Arc<CreateAssetUpload>` etc., not the port directly — same shape as `AdvertisingState` holding `Arc<CreateCampaign>` rather than a raw adapter.

- [ ] **Step 1: Write the failing test** — a fake port, one use case exercised:

```rust
use std::sync::Arc;
use async_trait::async_trait;
use sanvi_kernel::TenantId;

use crate::domain::{
    AssetDeliveryView, AssetUploadView, AssetView, CompletedPartInput, CreateAssetInput,
    MediaError, MediaGatewayPort, UploadPartView,
};

pub struct CreateAssetUpload {
    gateway: Arc<dyn MediaGatewayPort>,
}

impl CreateAssetUpload {
    pub fn new(gateway: Arc<dyn MediaGatewayPort>) -> Self {
        Self { gateway }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        idempotency_key: String,
        input: CreateAssetInput,
    ) -> Result<AssetUploadView, MediaError> {
        self.gateway.create_asset(tenant_id, &idempotency_key, input).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeGateway;

    #[async_trait]
    impl MediaGatewayPort for FakeGateway {
        async fn create_asset(&self, _tenant_id: TenantId, idempotency_key: &str, input: CreateAssetInput) -> Result<AssetUploadView, MediaError> {
            assert_eq!(idempotency_key, "idem-1");
            assert_eq!(input.filename, "a.png");
            Ok(AssetUploadView { asset_id: "a1".to_string(), upload_id: None, upload_url: Some("https://s3.example/put".to_string()), part_size: None, part_urls: None, expires_at: "2026-09-08T00:00:00Z".to_string() })
        }
        async fn presign_parts(&self, _: TenantId, _: &str, _: Vec<i64>) -> Result<Vec<UploadPartView>, MediaError> { unimplemented!() }
        async fn complete_asset(&self, _: TenantId, _: &str, _: Vec<CompletedPartInput>) -> Result<AssetView, MediaError> { unimplemented!() }
        async fn get_asset(&self, _: TenantId, _: &str) -> Result<AssetView, MediaError> { unimplemented!() }
        async fn delete_asset(&self, _: TenantId, _: &str) -> Result<(), MediaError> { unimplemented!() }
        async fn create_delivery(&self, _: TenantId, _: &str) -> Result<AssetDeliveryView, MediaError> { unimplemented!() }
    }

    #[tokio::test]
    async fn create_asset_upload_delegates_to_the_gateway_with_the_same_arguments() {
        let use_case = CreateAssetUpload::new(Arc::new(FakeGateway));
        let view = use_case
            .execute(
                TenantId::from(1u64), // same caveat as Task 2 Step 2 — confirm the real constructor before writing
                "idem-1".to_string(),
                CreateAssetInput { kind: "image".to_string(), content_type: "image/png".to_string(), filename: "a.png".to_string(), size_bytes: 10, owner_id: None },
            )
            .await
            .unwrap();
        assert_eq!(view.asset_id, "a1");
    }
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p sanvi-media application::create_asset_upload_delegates`
Expected: PASS once the struct above compiles (this task writes the real implementation directly, not a stub-then-fill, since there is no meaningful "minimal wrong version" for a one-line delegation — TDD's red step here is "doesn't compile yet," which Step 1 already demonstrated by writing test+impl together; run it to confirm green).

- [ ] **Step 3: Add the remaining five use cases** in the same file, each following the identical shape (`new`, `execute` delegating to the matching port method, a `FakeGateway`-based test asserting the arguments reach the port and the return value passes through):
  - `PresignAssetParts::execute(tenant_id, asset_id: String, part_numbers: Vec<i64>) -> Result<Vec<UploadPartView>, MediaError>`
  - `CompleteAssetUpload::execute(tenant_id, asset_id: String, parts: Vec<CompletedPartInput>) -> Result<AssetView, MediaError>`
  - `GetMediaAsset::execute(tenant_id, asset_id: String) -> Result<AssetView, MediaError>`
  - `DeleteMediaAsset::execute(tenant_id, asset_id: String) -> Result<(), MediaError>`
  - `CreateAssetDelivery::execute(tenant_id, asset_id: String) -> Result<AssetDeliveryView, MediaError>`

  Reuse one shared `FakeGateway` in the test module (make each method's fake behavior configurable via a field or closure if more than one test needs different canned responses — do not duplicate the six-method `impl MediaGatewayPort for FakeGateway` block per test).

- [ ] **Step 4: `application/mod.rs`** re-exports all six: `pub use self::{create_asset_upload::CreateAssetUpload, ...};` (adjust to whatever file/module layout Step 1–3 actually used — a single `application/mod.rs` file with all six structs inline is fine and matches this context's size; splitting into six files is unnecessary for six ~10-line structs).

- [ ] **Step 5: Run the whole application test module, then commit**

```bash
cargo test -p sanvi-media application::
git add crates/contexts/media/src/application
git commit -m "feat(media): application use cases wrapping MediaGatewayPort"
```

---

### Task 4: HTTP layer — state, handlers, routes, error mapping

**Files:**
- Create: `crates/contexts/media/src/adapters/http/mod.rs`
- Create: `crates/contexts/media/src/adapters/http/state.rs`
- Create: `crates/contexts/media/src/adapters/http/handlers.rs`
- Create: `crates/contexts/media/src/adapters/http/routes.rs`
- Modify: `crates/contexts/media/src/lib.rs` (uncomment `adapters`/`application`/`config`/`routes`, per Task 1 Step 7's note)

**Interfaces:**
- Consumes: the six application use cases (Task 3), `sanvi_http::{ApiError, ProblemDetail, CurrentTenant, RequiredPermission, PermissionMap, require_permission}`.
- Produces: `MediaState`, `media_tenant_scope_routes(cfg: &mut web::ServiceConfig)`, and the six `pub async fn` handlers `create_media_asset`, `presign_media_asset_parts`, `complete_media_asset_upload`, `get_media_asset`, `delete_media_asset`, `create_media_asset_delivery` — these six names are what Task 6's `apps/api/src/openapi.rs` registration and Task 5's route wiring both reference; keep them exact.

- [ ] **Step 1: `state.rs`**

```rust
use std::sync::Arc;

use crate::application::{
    CompleteAssetUpload, CreateAssetDelivery, CreateAssetUpload, DeleteMediaAsset, GetMediaAsset,
    PresignAssetParts,
};

#[derive(Clone)]
pub struct MediaState {
    pub create_asset_upload: Arc<CreateAssetUpload>,
    pub presign_asset_parts: Arc<PresignAssetParts>,
    pub complete_asset_upload: Arc<CompleteAssetUpload>,
    pub get_media_asset: Arc<GetMediaAsset>,
    pub delete_media_asset: Arc<DeleteMediaAsset>,
    pub create_asset_delivery: Arc<CreateAssetDelivery>,
}
```

- [ ] **Step 2: Write the failing route test** — an actix `test::init_service` hitting `create_media_asset` through a fake port, asserting: (a) a request with the `media.assets.manage` permission and a valid `Idempotency-Key` returns 201 with the gateway's view, (b) the gateway is called with the *session's* tenant id even when the request also sends a spoofed `X-Namespace-ID` header. Put this in `crates/contexts/media/src/adapters/http/handlers.rs`'s own `#[cfg(test)]` module — follow whichever existing pattern `crates/contexts/advertising/src/adapters/http/handlers.rs`'s own test module (if it has one) or `crates/platform/http/tests/integration/*.rs` uses for building a `CurrentTenant`/`RequiredPermission`-satisfying test request (these are `actix_web::FromRequest` extractors wired to request extensions set by middleware in production; read one of those existing tests first to copy the exact construction — do not guess it, since getting `CurrentTenant`'s test double wrong is the single most likely place to fabricate an API that does not exist).

- [ ] **Step 3: Run it, confirm it fails** (handlers don't exist).

- [ ] **Step 4: Write `handlers.rs`**

```rust
use actix_web::{HttpRequest, HttpResponse, error::InternalError, web};
use sanvi_http::{ApiError, CurrentTenant, RequiredPermission};

use super::state::MediaState;
use crate::domain::{CompletedPartInput, CreateAssetInput, MediaError};

/// Maps a `MediaError` into the standard `ApiError` status vocabulary while
/// threading the upstream `code` through as an RFC 9457 extension member —
/// the same `.problem()` + `extensions` + `InternalError::from_response`
/// pattern `advertising`'s `campaign_validation_problem` already uses
/// (`crates/contexts/advertising/src/adapters/http/handlers.rs:355-374`).
/// `course-media`'s `CourseApiError` reads `problem.code` directly, so this
/// is load-bearing, not decoration — see TASK-032's "Error mapping" section.
fn media_problem(err: MediaError) -> actix_web::Error {
    let (code, api) = match err {
        MediaError::BadRequest { code, detail } => (code, ApiError::BadRequest { detail }),
        MediaError::NotFound { code, detail } => (code, ApiError::NotFound { resource: detail }),
        MediaError::Conflict { code, detail } => (code, ApiError::Conflict { detail }),
        MediaError::Unavailable { detail } => {
            ("media_unavailable".to_string(), ApiError::ServiceUnavailable { detail })
        }
    };
    let mut problem = api.problem();
    problem.extensions = Some(
        [("code".to_string(), serde_json::Value::String(code))]
            .into_iter()
            .collect(),
    );
    InternalError::from_response(api.clone(), HttpResponse::build(api.status()).json(problem)).into()
}

fn idempotency_key(request: &HttpRequest) -> String {
    request
        .headers()
        .get("Idempotency-Key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/media/v1/assets",
    tag = "media",
    operation_id = "create_media_asset",
    request_body = CreateAssetInput,
    params(
        ("Idempotency-Key" = String, Header, description = "Required by hitox-media-service; a retried key returns the original asset.")
    ),
    responses(
        (status = 201, description = "Asset minted, upload presigned", body = crate::domain::AssetUploadView),
        (status = 400, description = "hitox-media-service rejected the request (e.g. missing Idempotency-Key)", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Missing media.assets.manage permission", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn create_media_asset(
    state: web::Data<MediaState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    request: HttpRequest,
    body: web::Json<CreateAssetInput>,
) -> Result<HttpResponse, actix_web::Error> {
    let view = state
        .create_asset_upload
        .execute(tenant.0.tenant_id, idempotency_key(&request), body.into_inner())
        .await
        .map_err(media_problem)?;
    Ok(HttpResponse::Created().json(view))
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/media/v1/assets/{asset_id}",
    tag = "media",
    operation_id = "get_media_asset",
    params(("asset_id" = String, Path)),
    responses(
        (status = 200, description = "Current asset state", body = crate::domain::AssetView),
        (status = 404, description = "No such asset in this tenant's namespace", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Missing media.assets.manage permission", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn get_media_asset(
    state: web::Data<MediaState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    asset_id: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    let view = state
        .get_media_asset
        .execute(tenant.0.tenant_id, asset_id.into_inner())
        .await
        .map_err(media_problem)?;
    Ok(HttpResponse::Ok().json(view))
}
```

- [ ] **Step 5: Run the route test from Step 2, confirm it passes.**

- [ ] **Step 6: Add the remaining four handlers**, same shape as the two above (`#[utoipa::path]` + `CurrentTenant` + `RequiredPermission` + `.map_err(media_problem)?`):
  - `presign_media_asset_parts`: `POST /api/v1/tenant/media/v1/assets/{asset_id}/parts`, body `{ part_numbers: Vec<i64> }` (define a tiny local `#[derive(Deserialize, ToSchema)] struct PresignPartsRequest { part_numbers: Vec<i64> }` in `handlers.rs` — this one has no counterpart in `domain::dto` because it is not a value the domain layer needs to model, only a wire shape), 200 response `Vec<UploadPartView>` wrapped as `{"part_urls": [...]}` to match `course-media`'s `wire.part_urls` read — i.e. return `HttpResponse::Ok().json(serde_json::json!({"part_urls": urls}))`, not a bare array.
  - `complete_media_asset_upload`: `POST .../complete`, body `{ parts: Vec<CompletedPartInput> }` (same local wrapper-struct treatment), 200, `AssetView`.
  - `delete_media_asset`: `DELETE .../{asset_id}`, no body, `204 No Content` (`HttpResponse::NoContent().finish()`) on success.
  - `create_media_asset_delivery`: `POST .../{asset_id}/delivery`, no body, 200, `AssetDeliveryView`.

  One test per handler, each proving: success path returns the right status/body from a fake use case, and a `MediaError` from the fake surfaces as the right HTTP status with `code` present in the JSON body (`assert_eq!(body["code"], "not_found")` style, deserializing the test response body as `serde_json::Value`).

- [ ] **Step 7: `routes.rs`**

```rust
use actix_web::{http::Method, web};
use sanvi_http::{PermissionMap, require_permission};

use super::handlers::{
    complete_media_asset_upload, create_media_asset, create_media_asset_delivery,
    delete_media_asset, get_media_asset, presign_media_asset_parts,
};

pub fn media_tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    let manage = require_permission!("media.assets.manage", tenant);

    cfg.service(
        web::resource("/v1/assets")
            .app_data(PermissionMap::new(vec![(Method::POST, manage)]))
            .route(web::post().to(create_media_asset)),
    );
    cfg.service(
        web::resource("/v1/assets/{asset_id}")
            .app_data(PermissionMap::new(vec![
                (Method::GET, manage),
                (Method::DELETE, manage),
            ]))
            .route(web::get().to(get_media_asset))
            .route(web::delete().to(delete_media_asset)),
    );
    cfg.service(
        web::resource("/v1/assets/{asset_id}/parts")
            .app_data(PermissionMap::new(vec![(Method::POST, manage)]))
            .route(web::post().to(presign_media_asset_parts)),
    );
    cfg.service(
        web::resource("/v1/assets/{asset_id}/complete")
            .app_data(PermissionMap::new(vec![(Method::POST, manage)]))
            .route(web::post().to(complete_media_asset_upload)),
    );
    cfg.service(
        web::resource("/v1/assets/{asset_id}/delivery")
            .app_data(PermissionMap::new(vec![(Method::POST, manage)]))
            .route(web::post().to(create_media_asset_delivery)),
    );
}
```

Confirm `require_permission!` called four times reuses one `RequiredPermission` value across resources the way `advertising_tenant_scope_routes` does (`let read = require_permission!(...)` once, reused) — it does above (`manage` bound once). If `RequiredPermission` turns out not to be `Clone` when you compile this, bind it per-resource instead (`require_permission!("media.assets.manage", tenant)` inline at each `PermissionMap::new` call) — check `crates/contexts/advertising/src/adapters/http/routes.rs`'s `read`/`write` variables, which are reused exactly this way, so `Clone` is expected to hold.

- [ ] **Step 8: `adapters/http/mod.rs`**

```rust
pub mod handlers;
pub mod routes;
pub mod state;

pub use state::MediaState;
```

- [ ] **Step 9: Uncomment `lib.rs`'s `adapters`/`application`/`config`/`routes` modules** (from Task 1 Step 7), build the whole crate:

```bash
cargo test -p sanvi-media
```

Expected: everything from Tasks 1–4 compiles and passes together.

- [ ] **Step 10: Commit**

```bash
git add crates/contexts/media/src/adapters/http crates/contexts/media/src/lib.rs
git commit -m "feat(media): HTTP handlers, routes and error mapping"
```

---

### Task 5: Register the `media.assets.manage` permission

**Files:**
- Modify: `crates/contexts/access/src/domain/permissions.rs` (end of the `advertising` block, ~line 501)
- Modify: `crates/contexts/access/src/domain/roles.rs` (`tenant_owner` ~line 247, `tenant_admin` ~line 280, and the `advertising_permissions_are_reachable_from_admin_roles` test ~line 373)

**Interfaces:**
- Produces: permission key `"media.assets.manage"`, granted to `tenant_owner` and `tenant_admin` only — the same role set `advertising.campaign.write` already reaches, since creative asset management is the one caller today.

- [ ] **Step 1: Write the failing test** — extend the existing reachability test rather than adding a parallel one (it already asserts owner/admin get every advertising write permission; add the media permission to the same lists):

In `crates/contexts/access/src/domain/roles.rs`, inside `advertising_permissions_are_reachable_from_admin_roles` (or rename it, since it now covers media too — renaming a test whose scope changed is fine, this is not `sanvi-media`'s test file so no external contract depends on the old name):

```rust
        for permission in [
            "advertising.read",
            "advertising.connect",
            "advertising.campaign.read",
            "advertising.campaign.write",
            "advertising.metrics.read",
            "advertising.budget.manage",
            "media.assets.manage",
        ] {
            assert!(owner.contains(&permission), "owner needs {permission}");
        }
        for permission in [
            "advertising.read",
            "advertising.connect",
            "advertising.campaign.read",
            "advertising.campaign.write",
            "advertising.metrics.read",
            "advertising.budget.manage",
            "media.assets.manage",
        ] {
            assert!(admin.contains(&permission), "admin needs {permission}");
        }
        let member = seed_by_key("tenant_member").unwrap().permissions;
        assert!(!member.contains(&"advertising.connect"));
        assert!(!member.contains(&"advertising.budget.manage"));
        assert!(!member.contains(&"media.assets.manage"));
```

- [ ] **Step 2: Run it, confirm it fails**

Run: `cargo test -p sanvi-access advertising_permissions_are_reachable_from_admin_roles`
Expected: FAIL — `owner`/`admin` don't contain `"media.assets.manage"` yet, and `"media.assets.manage"` isn't even a declared permission (this will actually fail earlier, at `validate_seeds`-style construction, if role seeds are validated against declared permissions at test-binary init — if so the failure message will name the undeclared permission rather than the assertion; either failure mode confirms the test is red for the right reason).

- [ ] **Step 3: Declare the permission** — in `crates/contexts/access/src/domain/permissions.rs`, immediately after the `advertising.budget.manage` entry (before the array's closing `];`):

```rust
    PermissionDef {
        key: "media.assets.manage",
        scope: PermissionScope::Tenant,
        description: "Upload, read and delete media assets, and mint delivery URLs, via the tenant's media gateway",
    },
```

- [ ] **Step 4: Grant it to `tenant_owner` and `tenant_admin`** — in `crates/contexts/access/src/domain/roles.rs`, append `"media.assets.manage",` to the end of each role's `permissions: &[...]` list (right after `"advertising.budget.manage",` in both `tenant_owner` and `tenant_admin`).

- [ ] **Step 5: Run the test again, confirm it passes**

Run: `cargo test -p sanvi-access advertising_permissions_are_reachable_from_admin_roles`
Expected: PASS.

- [ ] **Step 6: Run the whole `sanvi-access` crate's tests** (a new permission key can trip a "every declared permission is reachable from at least one role" or "every role only references declared permissions" whole-set invariant elsewhere in this crate — check for one and let it catch any typo):

```bash
cargo test -p sanvi-access
```

- [ ] **Step 7: Commit**

```bash
git add crates/contexts/access/src/domain/permissions.rs crates/contexts/access/src/domain/roles.rs
git commit -m "feat(access): add media.assets.manage permission for tenant_owner/tenant_admin"
```

---

### Task 6: Composition root — config, wiring, OpenAPI

**Files:**
- Modify: `apps/api/Cargo.toml` (add `sanvi-media.workspace = true` dependency)
- Modify: `apps/api/src/config.rs` (add `MediaConfig` field, mirroring `advertising` ~lines 4, 127, 153)
- Modify: `apps/api/src/lib.rs` (build `MediaState`, mount `media_tenant_scope_routes` — mirror the `advertising` wiring at ~lines 3061 and ~3466)
- Modify: `apps/api/src/openapi.rs` (register the six handlers + DTOs + a `media` tag, mirroring the `advertising` block at ~lines 1, 191-216)
- Create (generated): `api/media.yaml`

**Interfaces:**
- Consumes: `sanvi_media::{MediaConfig, adapters::hitox::HitoxMediaAdapter, adapters::http::MediaState, application::*, routes::media_tenant_scope_routes}`.

- [ ] **Step 1: `apps/api/Cargo.toml`** — add `sanvi-media.workspace = true` next to `sanvi-advertising.workspace = true`.

- [ ] **Step 2: `apps/api/src/config.rs`**

Add `use sanvi_media::MediaConfig;` near the other context config imports, add `pub media: MediaConfig,` to the `AppConfig` struct next to `pub advertising: AdvertisingConfig,`, and `media: MediaConfig::default(),` in its `Default` impl next to `advertising: AdvertisingConfig::default(),`. (`MediaConfig` already derives `Default` from Task 2 Step 1 — nothing else needed here; there is no feature-flag bool for this context, matching TASK-032's "Ships behind: none.")

- [ ] **Step 3: `apps/api/src/lib.rs` — build `MediaState`**

Find the function that builds `AdvertisingState` (~line 3061) and add an equivalent, sibling function (or inline block, matching whatever style that function uses) that builds `MediaState`:

```rust
fn build_media_state(config: &sanvi_media::MediaConfig) -> sanvi_media::adapters::http::MediaState {
    let gateway: std::sync::Arc<dyn sanvi_media::domain::MediaGatewayPort> =
        std::sync::Arc::new(sanvi_media::adapters::hitox::HitoxMediaAdapter::new(config.clone()));
    sanvi_media::adapters::http::MediaState {
        create_asset_upload: std::sync::Arc::new(sanvi_media::application::CreateAssetUpload::new(gateway.clone())),
        presign_asset_parts: std::sync::Arc::new(sanvi_media::application::PresignAssetParts::new(gateway.clone())),
        complete_asset_upload: std::sync::Arc::new(sanvi_media::application::CompleteAssetUpload::new(gateway.clone())),
        get_media_asset: std::sync::Arc::new(sanvi_media::application::GetMediaAsset::new(gateway.clone())),
        delete_media_asset: std::sync::Arc::new(sanvi_media::application::DeleteMediaAsset::new(gateway.clone())),
        create_asset_delivery: std::sync::Arc::new(sanvi_media::application::CreateAssetDelivery::new(gateway)),
    }
}
```

Call it once where `AdvertisingState` is built and register `.app_data(web::Data::new(media_state))` alongside wherever `AdvertisingState` is registered as `web::Data`.

- [ ] **Step 4: Mount the routes** — near the `tenant_scope.configure(advertising_routes::advertising_tenant_scope_routes)` call (~line 3466), add a **nested** scope so the path prefix matches TASK-032's `/api/v1/tenant/media` design:

```rust
tenant_scope.service(web::scope("/media").configure(media_routes::media_tenant_scope_routes))
```

adjusting to match the exact chaining style already used around line 3426-3467 (some contexts are `.configure()`'d directly onto `tenant_scope`, which only works when the context's own routes already include their full path segment — `media_tenant_scope_routes` in Task 4 registers `/v1/assets...` with no leading context segment, so it needs the extra `web::scope("/media")` wrapper here; contrast with `advertising_tenant_scope_routes`, which registers `/ads/...` itself and so is `.configure()`'d directly). Add `use sanvi_media::routes as media_routes;` near the other `routes as ..._routes` imports (~line 57).

- [ ] **Step 5: `apps/api/src/openapi.rs`**

Add imports:
```rust
use sanvi_media::adapters::http::handlers::{
    complete_media_asset_upload, create_media_asset, create_media_asset_delivery,
    delete_media_asset, get_media_asset, presign_media_asset_parts,
};
use sanvi_media::domain::{AssetDeliveryView, AssetUploadView, AssetView, CompletedPartInput, CreateAssetInput, UploadPartView};
```

In the `paths(...)` list, add the six handler paths (same style as the `sanvi_advertising::adapters::http::handlers::...` entries at ~lines 191-216). In `components(schemas(...))`, add the six DTO types. In `tags(...)`, add a `media` tag entry (copy the `advertising` tag's `(name = "...", description = "...")` shape).

- [ ] **Step 6: Generate and commit the contract**

```bash
cargo run -p sanvi-cli -- openapi --write
git status --short api/media.yaml   # confirm it was created
```

- [ ] **Step 7: Build the whole workspace, run the fast suite**

```bash
cargo c
just test-fast
```

Fix any compile error before proceeding — this is the first point everything links together.

- [ ] **Step 8: Commit**

```bash
git add apps/api Cargo.lock api/media.yaml
git commit -m "feat(media): wire MediaState, mount routes, generate OpenAPI contract"
```

---

### Task 7: Full verification and task-doc closeout

**Files:**
- Modify: `docs/tasks/phase-10/TASK-032-10-4a-media-asset-gateway.md` (tick every checkbox, flip `**Status:**` to `done`)
- Modify: `docs/tasks/backlog.md` (flip the `TASK-032` row's status to `done`, note the commit)

- [ ] **Step 1: Run the full gate**

```bash
cargo c
just fmt
just clippy
just layering
cargo nextest run -p sanvi-media
cargo nextest run -p sanvi-access
just openapi
```

(`just openapi` should be a no-op if Task 6 Step 6 already committed a matching `api/media.yaml` — if it diffs, the handler annotations and the committed file disagree; fix and recommit before continuing.)

- [ ] **Step 2: Manually exercise one round trip against a locally running `hitox-media-service`**, if `scripts/dev.sh up` / an equivalent local media-service instance is available in this environment — `POST` a real `create_media_asset` request through `apps/api` with a valid tenant session and confirm a `201` with a real `asset_id`. If no local media-service is reachable in this environment, say so explicitly in the task doc's Notes/gotchas (matching how TASK-013's own notes recorded live-credential blockers honestly) rather than claiming this was verified.

- [ ] **Step 3: Update `TASK-032-10-4a-media-asset-gateway.md`**

Tick every `- [ ]` in "What to do" and "Acceptance criteria" that is now true (leave any that genuinely aren't — e.g. if Step 2 above couldn't run, leave its acceptance criterion unchecked and say why in Notes/gotchas, the same honesty pattern TASK-013 and TASK-018 already use in this repo). Change `**Status:** in-progress` to `**Status:** done`.

- [ ] **Step 4: Update `docs/tasks/backlog.md`**'s `TASK-032` row: `in-progress` → `done`, append the commit/PR reference to the Notes cell, in the same commit as Step 3 (the file's own header states this invariant).

- [ ] **Step 5: Commit**

```bash
git add docs/tasks/phase-10/TASK-032-10-4a-media-asset-gateway.md docs/tasks/backlog.md
git commit -m "docs(phase-10): close out TASK-032 media asset gateway"
```

- [ ] **Step 6: Open the PR** (or hand back to the user to do so) from `feat/task-032-media-asset-gateway` — do not merge without review, per the global instruction that push/merge needs explicit user go-ahead.

---

## Self-Review Notes (for whoever executes this plan)

- **Two intentionally-marked unknowns**: the exact in-test `TenantId` constructor (Tasks 2 & 3) and the exact test-double construction for `CurrentTenant`/`RequiredPermission` (Task 4, Step 2). Both are flagged inline rather than guessed — grep the codebase for the real answer before writing those lines; this plan would rather cost you one extra grep than ship a fabricated signature.
- **`RequiredPermission: Clone`** (Task 4, Step 7) is asserted by analogy to `advertising_tenant_scope_routes`, not independently verified against the type definition — if the build disagrees, the fallback (bind inline per resource) is already spelled out in that step.
- Every wire field name (`kind`, `content_type`, `size_bytes`, `part_numbers`, `part_urls`, `etag`, `code`, `detail`, …) was checked against both sides of the existing, shipped contract — `sanvi-frontend/packages/course-media/src/api/assets.ts` and `hitox-media-service/internal/media/{adapters/httpapi,application}/*.go` — not invented for this plan.
- TASK-032's "request without a valid session gets the standard 401 shape" acceptance criterion has no dedicated test in this plan on purpose: `CurrentTenant`/`RequiredPermission` are the exact extractors every other context's routes already use, and the 401/403 path they produce on an unauthenticated or unpermitted request is that shared infrastructure's contract (covered by `crates/platform/http/tests/integration/*.rs`), not something a new context re-proves. Using the same extractors *is* how this context inherits that guarantee.
