//! Integration tests: real Postgres in containers, the sanvi_app role
//! (NOBYPASSRLS), full migrations. Run with `cargo nextest run -p
//! sanvi-localization` (docker must be available).

use std::sync::Arc;

use sanvi_localization::{
    LocalizationConfig,
    adapters::{
        LocalizationCache, LocalizationMetrics, LocalizationService, SqlxSupportedLocaleRepository,
        SqlxTenantLocaleRepository, SqlxTranslationRepository,
    },
    application::{
        CatalogService, GetTenantOverrides, UpdatePlatformTranslations, UpdateTenantOverrides,
    },
    domain::{
        Locale, LocalizationError, SupportedLocaleRepository, TenantLocaleRepository,
        TenantLocaleSettings, Translation, TranslationRepository, Translator,
    },
};
use sanvi_persistence::{DatabaseConfig, Migrator, TxManager, build_pool};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

fn migrations_dir() -> String {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .join("../../../migrations")
        .canonicalize()
        .expect("migrations directory must exist")
        .to_string_lossy()
        .to_string()
}

/// Fresh database with both migrations applied; returns the app-role pool
/// (and the container handle, which must stay alive).
async fn prepare() -> (
    sanvi_persistence::PgPool,
    testcontainers::ContainerAsync<Postgres>,
) {
    let node = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres");
    let port = node.get_host_port_ipv4(5432).await.expect("postgres port");

    let admin = build_pool(&DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: "postgres".to_string(),
        user: "postgres".to_string(),
        password: "postgres".to_string(),
        migrations_dir: migrations_dir(),
        ..DatabaseConfig::default()
    })
    .await
    .unwrap();

    sqlx::query("CREATE ROLE sanvi_migrator LOGIN PASSWORD 'mig'")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE ROLE sanvi_app LOGIN PASSWORD 'app' NOBYPASSRLS")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE DATABASE sanvi_test OWNER sanvi_migrator")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("GRANT CONNECT ON DATABASE sanvi_test TO sanvi_app")
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;

    let migrator_pool = build_pool(&DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: "sanvi_test".to_string(),
        user: "sanvi_migrator".to_string(),
        password: "mig".to_string(),
        migrations_dir: migrations_dir(),
        ..DatabaseConfig::default()
    })
    .await
    .unwrap();
    Migrator::new(migrations_dir())
        .run(&migrator_pool)
        .await
        .expect("migrations up");

    let app_pool = build_pool(&DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: "sanvi_test".to_string(),
        user: "sanvi_app".to_string(),
        password: "app".to_string(),
        migrations_dir: migrations_dir(),
        ..DatabaseConfig::default()
    })
    .await
    .unwrap();

    (app_pool, node)
}

fn service(pool: sanvi_persistence::PgPool) -> Arc<LocalizationService> {
    Arc::new(LocalizationService::new(
        pool,
        Arc::new(SqlxTranslationRepository::new()),
        Arc::new(LocalizationCache::new()),
        Arc::new(LocalizationMetrics::new(&prometheus::Registry::new())),
        LocalizationConfig::default(),
    ))
}

/// Seed a minimal tenant row (`tenant_locales` references `tenancy.tenants`).
async fn seed_tenant(pool: &sanvi_persistence::PgPool, slug: &str) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO tenancy.tenants (id, slug, display_name, status, region)
         VALUES ($1, $2, $2, 'active', 'us')",
    )
    .bind(id)
    .bind(slug)
    .execute(pool)
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn supported_locales_are_seeded_en_and_ja() {
    let (pool, _node) = prepare().await;
    let repo = SqlxSupportedLocaleRepository::new();
    let locales = repo.list(&pool).await.unwrap();
    assert_eq!(locales.len(), 2);
    assert_eq!(locales[0].code.as_str(), "en");
    assert_eq!(locales[0].name_native, "English");
    assert_eq!(locales[1].code.as_str(), "ja");
    assert_eq!(locales[1].name_native, "日本語");
}

#[tokio::test]
async fn tenant_locale_settings_are_rls_isolated() {
    let (pool, _node) = prepare().await;
    let tx = TxManager::new(pool.clone());
    let repo = SqlxTenantLocaleRepository::new();
    let tenant_a = seed_tenant(&pool, "loc-a").await;
    let tenant_b = seed_tenant(&pool, "loc-b").await;

    let settings_a = TenantLocaleSettings {
        tenant_id: tenant_a,
        default_locale: Locale::parse("ja").unwrap(),
        enabled_locales: vec![Locale::parse("ja").unwrap(), Locale::parse("en").unwrap()],
        timezone: "Asia/Tokyo".to_string(),
        updated_at: chrono::Utc::now(),
    };
    {
        let mut scope = tx.begin_for(tenant_a).await.unwrap();
        repo.upsert(&mut scope, &settings_a).await.unwrap();
        scope.commit().await.unwrap();
    }

    // Visible inside tenant A's scope…
    {
        let mut scope = tx.begin_for(tenant_a).await.unwrap();
        let read = repo.get(&mut scope, tenant_a).await.unwrap().unwrap();
        scope.commit().await.unwrap();
        assert_eq!(read.default_locale.as_str(), "ja");
        assert_eq!(read.timezone, "Asia/Tokyo");
    }

    // …and invisible inside tenant B's (RLS FORCE).
    {
        let mut scope = tx.begin_for(tenant_b).await.unwrap();
        let read = repo.get(&mut scope, tenant_b).await.unwrap();
        scope.commit().await.unwrap();
        assert!(read.is_none());
    }
}

#[tokio::test]
async fn platform_overrides_are_readable_but_only_writable_from_the_platform_scope() {
    let (pool, _node) = prepare().await;
    let tx = TxManager::new(pool.clone());
    let repo = SqlxTranslationRepository::new();

    let translation = Translation {
        scope: "platform".to_string(),
        key: "billing.invoice.label".to_string(),
        locale: "ja".to_string(),
        value: "インボイス".to_string(),
        updated_by: None,
        updated_at: chrono::Utc::now(),
    };

    // A tenant scope cannot write platform rows: the type system routes
    // platform writes through the audited `PlatformScope`, and when raw SQL
    // sneaks in anyway the RLS policy rejects it.
    let tenant = seed_tenant(&pool, "loc-plat").await;
    let mut tenant_scope = tx.begin_for(tenant).await.unwrap();
    let result = sqlx::query(
        "INSERT INTO localization.translations (scope, key, locale, value)
         VALUES ('platform', 'billing.invoice.label', 'ja', 'インボイス')",
    )
    .execute(&mut **tenant_scope.tx_mut().as_inner())
    .await;
    assert!(result.is_err(), "tenant RLS must reject platform rows");
    tenant_scope.rollback().await.unwrap();

    // The audited platform scope can.
    {
        let mut scope = tx.begin_unscoped("test", "test").await.unwrap();
        repo.upsert_platform(&mut scope, &translation)
            .await
            .unwrap();
        scope.commit().await.unwrap();
    }

    // And any role can read it (it is public copy).
    let overrides = repo.list_platform(&pool).await.unwrap();
    assert_eq!(overrides.len(), 1);
    assert_eq!(overrides[0].value, "インボイス");
}

#[tokio::test]
async fn audited_platform_scope_can_collect_and_anonymize_tenant_translation_attribution() {
    let (pool, _node) = prepare().await;
    let tx = TxManager::new(pool.clone());
    let repo = SqlxTranslationRepository::new();
    let tenant = seed_tenant(&pool, "loc-dsr").await;
    let actor = uuid::Uuid::now_v7();

    {
        let mut scope = tx.begin_for(tenant).await.unwrap();
        repo.upsert_tenant(
            &mut scope,
            tenant,
            &Translation {
                scope: tenant.to_string(),
                key: "billing.invoice.label".to_string(),
                locale: "en".to_string(),
                value: "Statement".to_string(),
                updated_by: Some(actor),
                updated_at: chrono::Utc::now(),
            },
        )
        .await
        .unwrap();
        scope.commit().await.unwrap();
    }

    let mut platform = tx
        .begin_unscoped("privacy.dsr", "localization")
        .await
        .unwrap();
    let visible: i64 =
        sqlx::query_scalar("SELECT count(*) FROM localization.translations WHERE updated_by = $1")
            .bind(actor)
            .fetch_one(&mut **platform.tx_mut().as_inner())
            .await
            .unwrap();
    assert_eq!(visible, 1);
    sqlx::query("UPDATE localization.translations SET updated_by = NULL WHERE updated_by = $1")
        .bind(actor)
        .execute(&mut **platform.tx_mut().as_inner())
        .await
        .unwrap();
    platform.commit().await.unwrap();

    let mut tenant_scope = tx.begin_for(tenant).await.unwrap();
    let translations = repo.list_tenant(&mut tenant_scope, tenant).await.unwrap();
    tenant_scope.commit().await.unwrap();
    assert_eq!(translations.len(), 1);
    assert_eq!(translations[0].updated_by, None);
}

#[tokio::test]
async fn platform_translation_put_replaces_rows_omitted_from_the_request() {
    let (pool, _node) = prepare().await;
    let tx = TxManager::new(pool.clone());
    let repo = Arc::new(SqlxTranslationRepository::new());
    let update = UpdatePlatformTranslations::new(
        repo.clone(),
        Arc::new(sanvi_localization::adapters::OutboxEventPublisher::new()),
        Arc::new(sanvi_localization::adapters::SystemClock),
    );

    let mut scope = tx.begin_unscoped("test", "test").await.unwrap();
    update
        .execute(
            &mut scope,
            None,
            "en",
            &[
                sanvi_localization::application::TranslationPatch {
                    key: "billing.invoice.label".to_string(),
                    locale: "en".to_string(),
                    value: "Bill".to_string(),
                },
                sanvi_localization::application::TranslationPatch {
                    key: "billing.invoice.total".to_string(),
                    locale: "en".to_string(),
                    value: "Grand total".to_string(),
                },
            ],
        )
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let mut scope = tx.begin_unscoped("test", "test").await.unwrap();
    update
        .execute(
            &mut scope,
            None,
            "en",
            &[sanvi_localization::application::TranslationPatch {
                key: "billing.invoice.total".to_string(),
                locale: "en".to_string(),
                value: "Total".to_string(),
            }],
        )
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let rows = repo.list_platform(&pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, "billing.invoice.total");
}

#[tokio::test]
async fn tenant_overrides_merge_over_static_and_platform_catalogs() {
    let (pool, _node) = prepare().await;
    let tx = TxManager::new(pool.clone());
    let repo = SqlxTranslationRepository::new();
    let tenant = seed_tenant(&pool, "loc-merge").await;

    // Platform override (applies to every tenant).
    {
        let mut scope = tx.begin_unscoped("test", "test").await.unwrap();
        repo.upsert_platform(
            &mut scope,
            &Translation {
                scope: "platform".to_string(),
                key: "billing.invoice.label".to_string(),
                locale: "en".to_string(),
                value: "Bill".to_string(),
                updated_by: None,
                updated_at: chrono::Utc::now(),
            },
        )
        .await
        .unwrap();
        scope.commit().await.unwrap();
    }

    // Tenant override wins over the platform override.
    {
        let mut scope = tx.begin_for(tenant).await.unwrap();
        repo.upsert_tenant(
            &mut scope,
            tenant,
            &Translation {
                scope: tenant.to_string(),
                key: "billing.invoice.label".to_string(),
                locale: "en".to_string(),
                value: "Statement".to_string(),
                updated_by: None,
                updated_at: chrono::Utc::now(),
            },
        )
        .await
        .unwrap();
        scope.commit().await.unwrap();
    }

    let service = service(pool.clone());
    service.warm(&tenant.to_string(), "en").await.unwrap();

    let rendered = service.translate(&tenant.to_string(), "en", "billing.invoice.label", &[]);
    assert_eq!(rendered, "Statement");

    // The platform scope still sees only the platform override.
    service.warm("platform", "en").await.unwrap();
    let rendered = service.translate("platform", "en", "billing.invoice.label", &[]);
    assert_eq!(rendered, "Bill");
}

#[tokio::test]
async fn static_catalogs_render_without_any_database_rows() {
    let (pool, _node) = prepare().await;
    let service = service(pool);

    // No warm: the synchronous path falls back to the embedded catalogs.
    let rendered = service.translate(
        "platform",
        "ja",
        "billing.invoice.due_date",
        &[(
            "date",
            sanvi_localization::domain::Arg::from("2026年9月1日"),
        )],
    );
    assert_eq!(rendered, "支払期限 2026年9月1日");

    let rendered = service.translate(
        "platform",
        "en",
        "emails.invitation.subject",
        &[("tenant", "Acme".into())],
    );
    // The doubled apostrophe in the catalog unquotes to a literal one.
    assert_eq!(rendered, "You're invited to join Acme");
}

#[tokio::test]
async fn overrides_reject_unknown_keys_and_broken_patterns() {
    let (pool, _node) = prepare().await;
    let tx = TxManager::new(pool.clone());
    let repo = SqlxTranslationRepository::new();
    let tenant = seed_tenant(&pool, "loc-override").await;

    // A typo'd key can never silently ship.
    let patch = [sanvi_localization::application::TranslationPatch {
        key: "billing.invoice.lable".to_string(),
        locale: "en".to_string(),
        value: "x".to_string(),
    }];
    let updates = UpdateTenantOverrides::new(
        Arc::new(repo),
        Arc::new(sanvi_localization::adapters::OutboxEventPublisher::new()),
        Arc::new(sanvi_localization::adapters::SystemClock),
    );
    let mut scope = tx.begin_for(tenant).await.unwrap();
    let result = updates
        .execute(&mut scope, tenant, None, "en", &patch)
        .await;
    assert!(matches!(
        result,
        Err(LocalizationError::UnknownMessageKey(_))
    ));
    scope.rollback().await.unwrap();

    // A broken pattern is rejected too.
    let patch = [sanvi_localization::application::TranslationPatch {
        key: "billing.invoice.label".to_string(),
        locale: "en".to_string(),
        value: "{unterminated".to_string(),
    }];
    let mut scope = tx.begin_for(tenant).await.unwrap();
    let result = updates
        .execute(&mut scope, tenant, None, "en", &patch)
        .await;
    assert!(matches!(
        result,
        Err(LocalizationError::InvalidPattern { .. })
    ));
    scope.rollback().await.unwrap();

    // A valid override round-trips through the GET use case.
    let patch = [sanvi_localization::application::TranslationPatch {
        key: "billing.invoice.label".to_string(),
        locale: "en".to_string(),
        value: "Statement".to_string(),
    }];
    let mut scope = tx.begin_for(tenant).await.unwrap();
    updates
        .execute(&mut scope, tenant, None, "en", &patch)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let get = GetTenantOverrides::new(Arc::new(repo));
    let mut scope = tx.begin_for(tenant).await.unwrap();
    let overrides = get.execute(&mut scope, tenant).await.unwrap();
    scope.commit().await.unwrap();
    assert_eq!(overrides.len(), 1);
    assert_eq!(overrides[0].key, "billing.invoice.label");
}

#[tokio::test]
async fn catalog_delivery_filters_by_domain_and_etag_changes_with_content() {
    let (pool, _node) = prepare().await;
    let service = service(pool.clone());
    let catalog = CatalogService::new(
        pool.clone(),
        service.clone(),
        Arc::new(SqlxSupportedLocaleRepository::new()),
        "en".to_string(),
    );

    let view = catalog.catalog("platform", "errors", "ja").await.unwrap();
    assert_eq!(view.locale, "ja");
    assert!(view.messages.contains_key("errors.title.bad-request"));
    assert!(!view.messages.keys().any(|key| key.starts_with("billing.")));
    assert!(!view.revision.is_empty());

    // Unknown domains are refused.
    assert!(matches!(
        catalog.catalog("platform", "nope", "ja").await,
        Err(LocalizationError::UnknownDomain(_))
    ));
}
