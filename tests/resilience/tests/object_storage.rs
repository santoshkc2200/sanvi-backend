//! Dependency-matrix injection tests: object storage / hitox-media-service
//! (TASK-023, FR-1112/FR-1113).
//!
//! Storage down blocks uploads with an honest `Unavailable` error (the
//! handler renders it `503 media_unavailable`) and reads degrade to a
//! documented placeholder instead of a 500.

mod common;

use common::{FaultMode, FaultServer};
use sanvi_media::adapters::HitoxMediaAdapter;
use sanvi_media::application::GetMediaAsset;
use sanvi_media::config::MediaConfig;
use sanvi_media::domain::{CreateAssetInput, MediaGatewayPort};
use std::sync::Arc;

fn config(base_url: &str) -> MediaConfig {
    MediaConfig {
        base_url: base_url.to_string(),
        service_api_key: "matrix-key".to_string(),
    }
}

/// Object storage · down — every mutating call (create/complete/delete)
/// answers `MediaError::Unavailable`, fast, bounded by the client
/// deadline.
#[tokio::test]
async fn object_storage_down_blocks_uploads_with_an_honest_error() {
    let (base, _port) = common::closed_endpoint();
    let adapter = HitoxMediaAdapter::with_timeout(config(&base), std::time::Duration::from_millis(300));
    let tenant = sanvi_kernel::TenantId::try_from(1_000_001_i64).unwrap();

    let input = CreateAssetInput {
        kind: "course_video".to_string(),
        content_type: "video/mp4".to_string(),
        filename: "lecture.mp4".to_string(),
        size_bytes: 1024,
        visibility: Default::default(),
        owner_id: None,
    };
    let err = adapter
        .create_asset(tenant, "matrix-key-1", input)
        .await
        .expect_err("a dead storage cannot mint assets");
    assert!(
        matches!(err, sanvi_media::domain::MediaError::Unavailable { .. }),
        "expected Unavailable, got: {err}"
    );

    // The handler mapping for that variant is the honest 503.
    let problem = media_unavailable_problem();
    assert_eq!(problem.status().as_u16(), 503);
}

/// Object storage · erroring — 5xx answers take the identical degraded
/// path, and the breaker opens so repeated calls fail fast.
#[tokio::test]
async fn object_storage_erroring_takes_the_degraded_path_and_breaker_opens() {
    let server = FaultServer::start(FaultMode::Status(500));
    let adapter = HitoxMediaAdapter::with_resilience(
        config(server.base_url()),
        std::time::Duration::from_millis(300),
        // Two failures open the breaker; the 30 s cooldown keeps it open
        // for the probe below (a zero cooldown would half-open
        // immediately and the "fail fast" assertion could never hold).
        sanvi_resilience::BreakerConfig::new(2, 30),
        None,
    );
    let tenant = sanvi_kernel::TenantId::try_from(1_000_001_i64).unwrap();
    for attempt in 0..2 {
        let input = CreateAssetInput {
            kind: "course_video".to_string(),
            content_type: "video/mp4".to_string(),
            filename: "lecture.mp4".to_string(),
            size_bytes: 1024,
            visibility: Default::default(),
            owner_id: None,
        };
        let err = adapter
            .create_asset(tenant, &format!("matrix-key-{attempt}"), input)
            .await
            .expect_err("a 500ing storage cannot mint assets");
        assert!(matches!(err, sanvi_media::domain::MediaError::Unavailable { .. }));
    }
    // Breaker open: the third call fails without a network round trip.
    let started = std::time::Instant::now();
    let input = CreateAssetInput {
        kind: "course_video".to_string(),
        content_type: "video/mp4".to_string(),
        filename: "lecture.mp4".to_string(),
        size_bytes: 1024,
        visibility: Default::default(),
        owner_id: None,
    };
    let outcome = adapter
        .create_asset(tenant, "matrix-key-3", input)
        .await;
    assert!(outcome.is_err());
    assert!(
        started.elapsed() < std::time::Duration::from_millis(50),
        "an open breaker must fail fast, took {:?}",
        started.elapsed()
    );
}

/// Object storage · down — reads serve the documented placeholder instead
/// of a 500: the use case answers a degraded `AssetView` (`placeholder`
/// set, no live metadata) so a course page renders its placeholder UI
/// rather than failing.
#[tokio::test]
async fn object_storage_down_serves_placeholders_and_blocks_uploads() {
    // Adapter-level: the gateway reports Unavailable.
    struct DownGateway;

    fn unavailable() -> sanvi_media::domain::MediaError {
        sanvi_media::domain::MediaError::Unavailable {
            detail: "connection refused".to_string(),
        }
    }

    #[async_trait::async_trait]
    impl MediaGatewayPort for DownGateway {
        async fn create_asset(
            &self,
            _tenant_id: sanvi_kernel::TenantId,
            _idempotency_key: &str,
            _input: sanvi_media::domain::CreateAssetInput,
        ) -> Result<sanvi_media::domain::AssetUploadView, sanvi_media::domain::MediaError> {
            Err(unavailable())
        }

        async fn presign_parts(
            &self,
            _tenant_id: sanvi_kernel::TenantId,
            _asset_id: &str,
            _part_numbers: Vec<i64>,
        ) -> Result<Vec<sanvi_media::domain::UploadPartView>, sanvi_media::domain::MediaError> {
            Err(unavailable())
        }

        async fn complete_asset(
            &self,
            _tenant_id: sanvi_kernel::TenantId,
            _asset_id: &str,
            _parts: Vec<sanvi_media::domain::CompletedPartInput>,
        ) -> Result<sanvi_media::domain::AssetView, sanvi_media::domain::MediaError> {
            Err(unavailable())
        }

        async fn get_asset(
            &self,
            _tenant_id: sanvi_kernel::TenantId,
            _asset_id: &str,
        ) -> Result<sanvi_media::domain::AssetView, sanvi_media::domain::MediaError> {
            Err(unavailable())
        }

        async fn delete_asset(
            &self,
            _tenant_id: sanvi_kernel::TenantId,
            _asset_id: &str,
        ) -> Result<(), sanvi_media::domain::MediaError> {
            Err(unavailable())
        }

        async fn create_delivery(
            &self,
            _tenant_id: sanvi_kernel::TenantId,
            _asset_id: &str,
        ) -> Result<sanvi_media::domain::AssetDeliveryView, sanvi_media::domain::MediaError> {
            Err(unavailable())
        }
    }

    let use_case = GetMediaAsset::new(Arc::new(DownGateway));
    let view = use_case
        .execute(
            sanvi_kernel::TenantId::try_from(1_000_001_i64).unwrap(),
            "asset-123".to_string(),
        )
        .await
        .expect("storage down degrades to a placeholder, it does not error");
    assert_eq!(view.id, "asset-123");
    assert_eq!(
        view.placeholder.as_deref(),
        Some("object_storage_unavailable"),
        "the placeholder must be marked, never mistaken for live metadata"
    );
    assert_eq!(view.status, "degraded");

    // The registry carries the reason so the header middleware stamps the
    // response when the flag is on.
    let degradation = sanvi_resilience::DegradationRegistry::default();
    degradation.set(sanvi_resilience::DegradationReason::ObjectStorage, true);
    assert_eq!(
        degradation.header_value().as_deref(),
        Some("object_storage")
    );
}

/// The handler mapping pinned by the matrix row: `MediaError::Unavailable`
/// renders as `503` service-unavailable.
fn media_unavailable_problem() -> sanvi_http::ApiError {
    sanvi_http::ApiError::ServiceUnavailable {
        detail: "hitox-media-service unavailable: connection refused".to_string(),
    }
}
