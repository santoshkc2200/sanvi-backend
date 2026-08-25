//! Integration tests: real Postgres, the sanvi_app role (NOBYPASSRLS),
//! full migrations — proving the publish/rollback state
//! machine, resolved-theme delivery, cache invalidation, entitlement
//! filtering, yank semantics and RLS isolation.
//!
//! Run with `just test` (docker must be available).

use std::collections::BTreeMap;
use std::sync::Arc;

use sanvi_privacy::domain::{EraseMode, PersonalDataProvider, SubjectRef};
use sanvi_testsupport::TestDb;
use sanvi_theming::{
    ThemingConfig,
    adapters::{
        LocalFileStore, OutboxEventPublisher, ResolvedThemeCache, SqlxTenantThemeRepository,
        SqlxThemeRepository, SystemClock, ThemingMetrics, privacy_provider::ThemingPrivacyProvider,
    },
    application::{
        InstallThemeVersion, InstallThemeVersionCommand, ListAvailableThemes, PublishTenantTheme,
        ResolveThemeService, RollbackTenantTheme, UpdateTenantThemeDraft,
        UpdateTenantThemeDraftCommand,
    },
    domain::{
        ArtifactStore, BrandAssetStore, Clock, EventPublisher, ResolvedTenantRef,
        TenantContextResolver, TenantThemeRepository, ThemeFeatureGate, ThemeRepository,
        ThemeState, ThemingError, TokenValue,
    },
};

async fn prepare() -> (sanvi_persistence::PgPool, TestDb) {
    let db = sanvi_testsupport::test_db().await;
    (db.app_pool(), db)
}

async fn seed_tenant(pool: &sanvi_persistence::PgPool, slug: &str) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO tenancy.tenants (id, slug, display_name, status, region)
         VALUES ($1, $2, $3, 'active', 'us')",
    )
    .bind(id)
    .bind(slug)
    .bind(slug)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// An always-disabled gate (except for the allow-listed keys).
#[derive(Default)]
struct TestGate {
    enabled: Vec<&'static str>,
}

#[async_trait::async_trait]
impl ThemeFeatureGate for TestGate {
    async fn is_enabled(
        &self,
        _tenant_id: uuid::Uuid,
        feature: &str,
    ) -> Result<bool, ThemingError> {
        Ok(self.enabled.contains(&feature))
    }
}

/// Host resolver: `tenant.<slug>` maps to a known tenant; everything else
/// is unresolved.
struct TestResolver {
    tenant_id: uuid::Uuid,
    slug: String,
}

struct TestArtifactStore;

#[async_trait::async_trait]
impl ArtifactStore for TestArtifactStore {
    async fn put(&self, _key: &str, _bytes: &[u8]) -> Result<(), ThemingError> {
        Ok(())
    }

    async fn verify(&self, _key: &str, _digest: &str) -> Result<(), ThemingError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl TenantContextResolver for TestResolver {
    async fn resolve(&self, host: &str) -> Result<Option<ResolvedTenantRef>, ThemingError> {
        if host == format!("{}.example.com", self.slug) {
            Ok(Some(ResolvedTenantRef {
                tenant_id: self.tenant_id,
                slug: self.slug.clone(),
            }))
        } else {
            Ok(None)
        }
    }
}

struct Harness {
    themes: Arc<dyn ThemeRepository>,
    tenants: Arc<dyn TenantThemeRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    service: Arc<ResolveThemeService>,
    update_draft: Arc<UpdateTenantThemeDraft>,
    publish: Arc<PublishTenantTheme>,
    rollback: Arc<RollbackTenantTheme>,
    install: Arc<InstallThemeVersion>,
    available: Arc<ListAvailableThemes>,
    tx: sanvi_persistence::TxManager,
}

fn build_harness(pool: sanvi_persistence::PgPool, gate: Arc<dyn ThemeFeatureGate>) -> Harness {
    let themes: Arc<dyn ThemeRepository> = Arc::new(SqlxThemeRepository::new());
    let tenants: Arc<dyn TenantThemeRepository> = Arc::new(SqlxTenantThemeRepository::new());
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let cache = Arc::new(ResolvedThemeCache::new(None));
    let metrics = Arc::new(ThemingMetrics::new(&prometheus::Registry::new()));
    let tx = sanvi_persistence::TxManager::new(pool.clone());
    let resolver: Arc<dyn TenantContextResolver> = Arc::new(TestResolver {
        tenant_id: uuid::Uuid::nil(),
        slug: "acme".to_string(),
    });
    let service = Arc::new(ResolveThemeService::new(
        themes.clone(),
        tenants.clone(),
        resolver,
        cache.clone(),
        metrics,
        clock.clone(),
        pool.clone(),
        "/assets/theming".to_string(),
        86_400,
    ));
    let config = ThemingConfig::default();
    Harness {
        update_draft: Arc::new(UpdateTenantThemeDraft::new(
            themes.clone(),
            tenants.clone(),
            gate.clone(),
            events.clone(),
            clock.clone(),
            pool.clone(),
            config.custom_css_max_bytes,
        )),
        publish: Arc::new(PublishTenantTheme::new(
            themes.clone(),
            tenants.clone(),
            gate.clone(),
            events.clone(),
            clock.clone(),
            pool.clone(),
            config.custom_css_max_bytes,
            cache.clone(),
        )),
        rollback: Arc::new(RollbackTenantTheme::new(
            tenants.clone(),
            events.clone(),
            clock.clone(),
            cache.clone(),
        )),
        install: Arc::new(InstallThemeVersion::new(
            themes.clone(),
            events.clone(),
            clock.clone(),
            pool.clone(),
            Arc::new(TestArtifactStore),
        )),
        available: Arc::new(ListAvailableThemes::new(
            themes.clone(),
            gate,
            pool.clone(),
            "/assets/theming".to_string(),
        )),
        themes,
        tenants,
        events,
        clock,
        service,
        tx,
    }
}

fn aurora_command() -> InstallThemeVersionCommand {
    let manifest: serde_json::Value = serde_json::from_str(
        r##"{
            "id": "aurora",
            "version": "1.0.0",
            "themeApi": "^1.0.0",
            "name": { "en": "Aurora", "ja": "オーロラ" },
            "capabilities": ["dark-mode"],
            "tokens": { "light": "tokens/light.json", "dark": "tokens/dark.json" },
            "overridable": ["color.brand.*", "color.background.*", "font.family.*", "radius.*", "spacing.scale"],
            "layouts": {
                "storefront.home": { "slots": ["hero", "featured"] },
                "storefront.checkout": { "slots": ["summary", "payment"], "locked": true }
            },
            "assets": { "preview": "preview.png" },
            "fonts": [{ "family": "Noto Sans JP", "subsets": ["latin", "japanese"], "source": "self-hosted" }]
        }"##,
    )
    .unwrap();
    let tokens: serde_json::Value = serde_json::from_str(
        r##"{
            "light": {
                "color": {
                    "background": {
                        "primary": { "$value": "#fbfaff", "$type": "color" },
                        "inverse": { "$value": "#1e1b2e", "$type": "color" }
                    },
                    "text": {
                        "primary": { "$value": "#201a3d", "$type": "color" },
                        "secondary": { "$value": "#4c4566", "$type": "color" },
                        "inverse": { "$value": "#ffffff", "$type": "color" }
                    },
                    "border": { "primary": { "$value": "#e2ddf2", "$type": "color" } },
                    "brand": {
                        "primary": { "$value": "#7c3aed", "$type": "color" },
                        "hover": { "$value": "#6d28d9", "$type": "color" },
                        "contrast": { "$value": "#ffffff", "$type": "color" }
                    },
                    "focus": { "ring": { "$value": "#6d28d9", "$type": "color" } }
                },
                "font": {
                    "family": {
                        "base": { "$value": "Noto Sans JP, sans-serif", "$type": "fontFamily" },
                        "heading": { "$value": "Noto Sans JP, sans-serif", "$type": "fontFamily" },
                        "mono": { "$value": "monospace", "$type": "fontFamily" }
                    },
                    "size": {
                        "sm": { "$value": "0.875rem", "$type": "dimension" },
                        "base": { "$value": "1rem", "$type": "dimension" },
                        "lg": { "$value": "1.125rem", "$type": "dimension" },
                        "xl": { "$value": "1.5rem", "$type": "dimension" }
                    },
                    "weight": {
                        "normal": { "$value": 400, "$type": "fontWeight" },
                        "bold": { "$value": 700, "$type": "fontWeight" }
                    }
                },
                "radius": {
                    "sm": { "$value": "0.375rem", "$type": "dimension" },
                    "md": { "$value": "0.75rem", "$type": "dimension" },
                    "lg": { "$value": "1rem", "$type": "dimension" },
                    "full": { "$value": "9999px", "$type": "dimension" }
                },
                "spacing": {
                    "scale": { "$value": 1, "$type": "number" },
                    "unit": { "$value": "0.25rem", "$type": "dimension" }
                }
            },
            "dark": {
                "color": {
                    "background": {
                        "primary": { "$value": "#131020", "$type": "color" },
                        "inverse": { "$value": "#ffffff", "$type": "color" }
                    },
                    "text": {
                        "primary": { "$value": "#ece9f6", "$type": "color" },
                        "secondary": { "$value": "#a89fc9", "$type": "color" },
                        "inverse": { "$value": "#131020", "$type": "color" }
                    },
                    "border": { "primary": { "$value": "#2e2848", "$type": "color" } },
                    "brand": {
                        "primary": { "$value": "#6d28d9", "$type": "color" },
                        "hover": { "$value": "#7c3aed", "$type": "color" },
                        "contrast": { "$value": "#ffffff", "$type": "color" }
                    },
                    "focus": { "ring": { "$value": "#8b5cf6", "$type": "color" } }
                },
                "font": {
                    "family": {
                        "base": { "$value": "Noto Sans JP, sans-serif", "$type": "fontFamily" },
                        "heading": { "$value": "Noto Sans JP, sans-serif", "$type": "fontFamily" },
                        "mono": { "$value": "monospace", "$type": "fontFamily" }
                    },
                    "size": {
                        "sm": { "$value": "0.875rem", "$type": "dimension" },
                        "base": { "$value": "1rem", "$type": "dimension" },
                        "lg": { "$value": "1.125rem", "$type": "dimension" },
                        "xl": { "$value": "1.5rem", "$type": "dimension" }
                    },
                    "weight": {
                        "normal": { "$value": 400, "$type": "fontWeight" },
                        "bold": { "$value": 700, "$type": "fontWeight" }
                    }
                },
                "radius": {
                    "sm": { "$value": "0.375rem", "$type": "dimension" },
                    "md": { "$value": "0.75rem", "$type": "dimension" },
                    "lg": { "$value": "1rem", "$type": "dimension" },
                    "full": { "$value": "9999px", "$type": "dimension" }
                },
                "spacing": {
                    "scale": { "$value": 1, "$type": "number" },
                    "unit": { "$value": "0.25rem", "$type": "dimension" }
                }
            }
        }"##,
    )
    .unwrap();
    InstallThemeVersionCommand {
        manifest,
        tokens,
        artifact_uri: "aurora-1.0.0.zip".to_string(),
        artifact_digest: "d".repeat(64),
        publisher: "sanvi".to_string(),
        visibility: "premium".to_string(),
        required_feature: Some("theming.premium_themes".to_string()),
        allow_contrast_warnings: false,
    }
}

#[tokio::test]
async fn publish_rollback_and_resolution_round_trip() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "acme").await;
    let harness = build_harness(pool.clone(), Arc::new(TestGate::default()));

    // Default resolution: the seeded base theme, revision 0.
    let resolved = harness.service.resolve_live(tenant_id, "en").await.unwrap();
    assert_eq!(resolved.theme_key, "base");
    assert_eq!(resolved.revision, 0);
    assert!(resolved.css_vars.contains("--color-brand-primary:#2563eb"));
    assert!(resolved.css_vars.contains("prefers-color-scheme"));

    // Install aurora as a premium theme (visible only with the feature).
    let tx = sanvi_persistence::TxManager::new(pool.clone());
    let mut scope = tx.begin_unscoped("test.install", "test").await.unwrap();
    harness
        .install
        .execute(&mut scope, &aurora_command())
        .await
        .unwrap();
    scope.commit().await.unwrap();

    // Without the entitlement it is invisible.
    let available = harness.available.execute(tenant_id).await.unwrap();
    assert!(!available.themes.iter().any(|theme| theme.key == "aurora"));

    // Switch the draft to aurora (a premium theme: requires the gate to
    // pass; use a gate that has the feature enabled for this update).
    let gate = Arc::new(TestGate {
        enabled: vec!["theming.premium_themes"],
    });
    let harness = build_harness(pool.clone(), gate);
    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let mut overrides = BTreeMap::new();
    overrides.insert(
        "color.brand.primary".to_string(),
        TokenValue {
            value: serde_json::json!("#b91c1c"),
            r#type: Some("color".to_string()),
            description: None,
        },
    );
    let view = harness
        .update_draft
        .execute(
            &mut scope,
            tenant_id,
            None,
            "en",
            &UpdateTenantThemeDraftCommand {
                theme_key: Some("aurora".to_string()),
                version: None,
                token_overrides: Some(overrides),
                layout_overrides: None,
                custom_css: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(view.theme_key, "aurora");
    assert_eq!(view.version, "1.0.0");
    scope.commit().await.unwrap();

    // Draft-only: the live theme is still the base theme.
    let resolved = harness.service.resolve_live(tenant_id, "en").await.unwrap();
    assert_eq!(resolved.theme_key, "base");

    // Publish.
    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let view = harness
        .publish
        .execute(&mut scope, tenant_id, None, "en")
        .await
        .unwrap();
    assert_eq!(view.revision, 1);
    assert_eq!(view.theme_key, "aurora");
    scope.commit().await.unwrap();

    // The live theme is aurora with the override applied.
    let resolved = harness.service.resolve_live(tenant_id, "en").await.unwrap();
    assert_eq!(resolved.theme_key, "aurora");
    assert_eq!(resolved.revision, 1);
    assert!(resolved.css_vars.contains("--color-brand-primary:#b91c1c"));
    assert!(resolved.css_vars.contains("--radius-md:0.75rem"));

    // The first publish has no previous look: rollback is refused.
    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let err = harness
        .rollback
        .execute(&mut scope, tenant_id, None, "en")
        .await
        .unwrap_err();
    assert!(matches!(err, ThemingError::NothingToRollBack), "{err}");
    let _ = scope.rollback().await;

    // Publishing a newer registry version must not silently upgrade an
    // existing draft when the tenant only edits an override.
    let mut newer = aurora_command();
    newer.manifest["version"] = serde_json::json!("1.1.0");
    newer.artifact_uri = "aurora-1.1.0.zip".to_string();
    let mut scope = tx
        .begin_unscoped("test.install-newer", "test")
        .await
        .unwrap();
    harness.install.execute(&mut scope, &newer).await.unwrap();
    scope.commit().await.unwrap();

    // A second publish (with a different override) makes revision 1
    // restorable.
    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let mut overrides = BTreeMap::new();
    overrides.insert(
        "color.brand.primary".to_string(),
        TokenValue {
            value: serde_json::json!("#0a7ea4"),
            r#type: Some("color".to_string()),
            description: None,
        },
    );
    let updated = harness
        .update_draft
        .execute(
            &mut scope,
            tenant_id,
            None,
            "en",
            &UpdateTenantThemeDraftCommand {
                theme_key: None,
                version: None,
                token_overrides: Some(overrides),
                layout_overrides: None,
                custom_css: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.version, "1.0.0");
    let view = harness
        .publish
        .execute(&mut scope, tenant_id, None, "en")
        .await
        .unwrap();
    assert_eq!(view.revision, 2);
    scope.commit().await.unwrap();

    let resolved = harness.service.resolve_live(tenant_id, "en").await.unwrap();
    assert!(resolved.css_vars.contains("--color-brand-primary:#0a7ea4"));

    // Rollback restores revision 1 exactly (theme + overrides).
    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let view = harness
        .rollback
        .execute(&mut scope, tenant_id, None, "en")
        .await
        .unwrap();
    assert_eq!(view.theme_key, "aurora");
    assert_eq!(view.revision, 3);
    scope.commit().await.unwrap();

    let resolved = harness.service.resolve_live(tenant_id, "en").await.unwrap();
    assert_eq!(resolved.theme_key, "aurora");
    assert_eq!(resolved.revision, 3);
    assert!(resolved.css_vars.contains("--color-brand-primary:#b91c1c"));
}

#[tokio::test]
async fn public_resolution_resolves_hosts_and_defaults() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "acme").await;

    // Unknown host: the platform default (base) resolves.
    let resolver: Arc<dyn TenantContextResolver> = Arc::new(TestResolver {
        tenant_id,
        slug: "acme".to_string(),
    });
    let themes: Arc<dyn ThemeRepository> = Arc::new(SqlxThemeRepository::new());
    let tenants: Arc<dyn TenantThemeRepository> = Arc::new(SqlxTenantThemeRepository::new());
    let cache = Arc::new(ResolvedThemeCache::new(None));
    let service = Arc::new(ResolveThemeService::new(
        themes,
        tenants,
        resolver,
        cache,
        Arc::new(ThemingMetrics::new(&prometheus::Registry::new())),
        Arc::new(SystemClock),
        pool,
        "/assets/theming".to_string(),
        86_400,
    ));

    let unknown = service
        .resolve_public("unknown.example.com", "en")
        .await
        .unwrap();
    assert_eq!(unknown.theme_key, "base");

    let known = service
        .resolve_public("acme.example.com", "ja")
        .await
        .unwrap();
    assert_eq!(known.theme_key, "base");
    assert_eq!(known.locale, "ja");
}

#[tokio::test]
async fn tenant_rows_are_rls_isolated() {
    let (pool, _node) = prepare().await;
    let tenant_a = seed_tenant(&pool, "tenant-a").await;
    let tenant_b = seed_tenant(&pool, "tenant-b").await;
    let harness = build_harness(pool.clone(), Arc::new(TestGate::default()));

    let mut scope = harness.tx.begin_for(tenant_a).await.unwrap();
    harness
        .update_draft
        .execute(
            &mut scope,
            tenant_a,
            None,
            "en",
            &UpdateTenantThemeDraftCommand {
                theme_key: None,
                version: None,
                token_overrides: None,
                layout_overrides: None,
                custom_css: None,
            },
        )
        .await
        .unwrap();
    scope.commit().await.unwrap();

    // B's scope cannot see A's draft.
    let mut scope = harness.tx.begin_for(tenant_b).await.unwrap();
    assert!(
        harness
            .tenants
            .get(&mut scope, tenant_a, ThemeState::Draft)
            .await
            .unwrap()
            .is_none()
    );
    scope.commit().await.unwrap();
}

#[tokio::test]
async fn theme_versions_are_immutable_and_yank_keeps_pins_renderable() {
    let (pool, _node) = prepare().await;
    let harness = build_harness(pool.clone(), Arc::new(TestGate::default()));

    // The trigger rejects updates and deletes.
    let update =
        sqlx::query("UPDATE theming.theme_versions SET semver = '9.9.9' WHERE semver = '1.0.0'")
            .execute(&pool)
            .await;
    assert!(update.is_err());
    let delete = sqlx::query("DELETE FROM theming.theme_versions WHERE semver = '1.0.0'")
        .execute(&pool)
        .await;
    assert!(delete.is_err());

    // Yank semantics: the seeded base 1.0.0 is pinned by a tenant, then
    // withdrawn — resolution keeps rendering it.
    let tenant_id = seed_tenant(&pool, "pinned").await;
    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    harness
        .update_draft
        .execute(
            &mut scope,
            tenant_id,
            None,
            "en",
            &UpdateTenantThemeDraftCommand {
                theme_key: Some("base".to_string()),
                version: None,
                token_overrides: None,
                layout_overrides: None,
                custom_css: None,
            },
        )
        .await
        .unwrap();
    harness
        .publish
        .execute(&mut scope, tenant_id, None, "en")
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let tx = sanvi_persistence::TxManager::new(pool.clone());
    let mut scope = tx.begin_unscoped("test.yank", "test").await.unwrap();
    sanvi_theming::application::YankThemeVersion::new(
        harness.themes.clone(),
        harness.events.clone(),
        harness.clock.clone(),
        pool.clone(),
    )
    .execute(&mut scope, "base", "1.0.0")
    .await
    .unwrap();
    scope.commit().await.unwrap();

    // The pinned tenant still resolves.
    let resolved = harness.service.resolve_live(tenant_id, "en").await.unwrap();
    assert_eq!(resolved.theme_key, "base");

    // But a fresh selection of "base" is refused (no non-yanked version).
    let fresh = seed_tenant(&pool, "fresh").await;
    let gate = Arc::new(TestGate { enabled: vec![] });
    let harness = build_harness(pool.clone(), gate);
    let mut scope = harness.tx.begin_for(fresh).await.unwrap();
    let err = harness
        .update_draft
        .execute(
            &mut scope,
            fresh,
            None,
            "en",
            &UpdateTenantThemeDraftCommand {
                theme_key: Some("base".to_string()),
                version: None,
                token_overrides: None,
                layout_overrides: None,
                custom_css: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ThemingError::UnknownTheme(_)), "{err}");
    let _ = scope.rollback().await;
}

#[tokio::test]
async fn contrast_gate_refuses_unreadable_palettes() {
    let (pool, _node) = prepare().await;
    let harness = build_harness(pool.clone(), Arc::new(TestGate::default()));

    let mut command = aurora_command();
    command.manifest["version"] = serde_json::json!("1.1.0");
    // Break the text contrast: primary text becomes barely-visible.
    command.tokens["light"]["color"]["text"]["primary"] =
        serde_json::json!({ "$value": "#94a3b8", "$type": "color" });

    let tx = sanvi_persistence::TxManager::new(pool.clone());
    let mut scope = tx.begin_unscoped("test.install", "test").await.unwrap();
    let err = harness
        .install
        .execute(&mut scope, &command)
        .await
        .unwrap_err();
    assert!(matches!(err, ThemingError::ContrastFailure(_)), "{err}");
    let _ = scope.rollback().await;

    // The explicit override installs (and is the only way).
    command.allow_contrast_warnings = true;
    let mut scope = tx.begin_unscoped("test.install", "test").await.unwrap();
    harness.install.execute(&mut scope, &command).await.unwrap();
    scope.commit().await.unwrap();
}

#[tokio::test]
async fn custom_css_requires_the_entitlement_and_is_scoped() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "css").await;

    let gate = Arc::new(TestGate {
        enabled: vec!["theming.custom_css"],
    });
    let harness = build_harness(pool.clone(), gate);
    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let view = harness
        .update_draft
        .execute(
            &mut scope,
            tenant_id,
            None,
            "en",
            &UpdateTenantThemeDraftCommand {
                theme_key: None,
                version: None,
                token_overrides: None,
                layout_overrides: None,
                custom_css: Some(Some(".hero { background-color: #f6f7f9; }".to_string())),
            },
        )
        .await
        .unwrap();
    assert!(view.custom_css.as_deref().unwrap().contains(".hero"));
    scope.commit().await.unwrap();

    // Publish and confirm the live theme delivers the CSS scoped under the
    // storefront wrapper class.
    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    harness
        .publish
        .execute(&mut scope, tenant_id, None, "en")
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let resolved = harness.service.resolve_live(tenant_id, "en").await.unwrap();
    let css = resolved.custom_css.expect("custom css delivered");
    assert!(css.contains(".sanvi-theme-scope .hero"), "{css}");

    // Without the entitlement the same command is refused.
    let harness = build_harness(pool.clone(), Arc::new(TestGate::default()));
    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    let err = harness
        .update_draft
        .execute(
            &mut scope,
            tenant_id,
            None,
            "en",
            &UpdateTenantThemeDraftCommand {
                theme_key: None,
                version: None,
                token_overrides: None,
                layout_overrides: None,
                custom_css: Some(Some("body { background-color: #fff; }".to_string())),
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, ThemingError::CustomCssNotEntitled(_)),
        "{err}"
    );
    let _ = scope.rollback().await;
}

#[tokio::test]
async fn brand_asset_upload_validates_and_stores() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "brand").await;

    // A minimal PNG (2x2). Content-addressed file lands in the store.
    let png: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];
    let root = std::env::temp_dir().join(format!("sanvi-theming-it-{}", uuid::Uuid::now_v7()));
    let store: Arc<dyn BrandAssetStore> = Arc::new(LocalFileStore::new(root.clone()));
    let themes: Arc<dyn ThemeRepository> = Arc::new(SqlxThemeRepository::new());
    let tenants: Arc<dyn TenantThemeRepository> = Arc::new(SqlxTenantThemeRepository::new());
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let upload = sanvi_theming::application::UploadBrandAsset::new(
        tenants.clone(),
        themes,
        store.clone(),
        events.clone(),
        clock.clone(),
        pool.clone(),
        2 * 1024 * 1024,
    );
    use base64::Engine as _;
    let tx_manager = sanvi_persistence::TxManager::new(pool.clone());
    let mut scope = tx_manager.begin_for(tenant_id).await.unwrap();
    let view = upload
        .execute(
            &mut scope,
            tenant_id,
            None,
            "en",
            &sanvi_theming::application::UploadBrandAssetCommand {
                kind: "logo".to_string(),
                content_type: Some("image/png".to_string()),
                data_base64: base64::engine::general_purpose::STANDARD.encode(png),
            },
        )
        .await
        .unwrap();
    let logo = view.assets.logo.expect("logo stored");
    assert_eq!(logo.mime, "image/png");
    scope.commit().await.unwrap();

    let stored = root
        .join("brand")
        .join(tenant_id.to_string())
        .join(format!("{}.{}", logo.digest, logo.ext));
    assert!(stored.exists());

    // Once published, removing the asset from the mutable draft must not
    // delete the content-addressed object still referenced by live/revision.
    let harness = build_harness(pool.clone(), Arc::new(TestGate::default()));
    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    harness
        .publish
        .execute(&mut scope, tenant_id, None, "en")
        .await
        .unwrap();
    scope.commit().await.unwrap();
    let remove = sanvi_theming::application::RemoveBrandAsset::new(tenants, events, clock);
    let mut scope = tx_manager.begin_for(tenant_id).await.unwrap();
    remove
        .execute(
            &mut scope,
            tenant_id,
            None,
            "en",
            sanvi_theming::BrandAssetKind::Logo,
        )
        .await
        .unwrap();
    scope.commit().await.unwrap();
    assert!(stored.exists());

    // SVG with a script marker is refused.
    let mut scope = tx_manager.begin_for(tenant_id).await.unwrap();
    let err = upload
        .execute(
            &mut scope,
            tenant_id,
            None,
            "en",
            &sanvi_theming::application::UploadBrandAssetCommand {
                kind: "logo".to_string(),
                content_type: None,
                data_base64: base64::engine::general_purpose::STANDARD
                    .encode(b"<svg><script>alert(1)</script></svg>"),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ThemingError::InvalidBrandAsset(_)), "{err}");
    let _ = scope.rollback().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn privacy_erasure_anonymises_theme_revision_actors() {
    let (pool, _node) = prepare().await;
    let tenant_id = seed_tenant(&pool, "privacy-theme").await;
    let actor = uuid::Uuid::now_v7();
    let harness = build_harness(pool.clone(), Arc::new(TestGate::default()));

    let mut scope = harness.tx.begin_for(tenant_id).await.unwrap();
    harness
        .update_draft
        .execute(
            &mut scope,
            tenant_id,
            Some(actor),
            "en",
            &UpdateTenantThemeDraftCommand {
                theme_key: None,
                version: None,
                token_overrides: None,
                layout_overrides: None,
                custom_css: None,
            },
        )
        .await
        .unwrap();
    harness
        .publish
        .execute(&mut scope, tenant_id, Some(actor), "en")
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let provider = ThemingPrivacyProvider::new(harness.tx.clone());
    let report = provider
        .erase(&SubjectRef::user(actor), EraseMode::Full)
        .await
        .unwrap();
    assert_eq!(report.anonymised, 3);

    let mut scope = harness
        .tx
        .begin_unscoped("test.verify-erasure", "test")
        .await
        .unwrap();
    let revision_actors: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM theming.tenant_theme_revisions WHERE published_by = $1",
    )
    .bind(actor)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    assert_eq!(revision_actors, 0);
    scope.commit().await.unwrap();
}
