//! SQLx repositories for the theming context.
//!
//! Platform registry reads go through the pool (reference data); writes run
//! inside the audited platform scope. Tenant rows are reached only through
//! `ScopedTx` (RLS FORCE), including the atomic publish/rollback steps that
//! move draft → live and append the immutable snapshot in one transaction.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::{PgPool, PlatformScope, ScopedTx};
use semver::{Version, VersionReq};

use crate::domain::error::ThemingError;
use crate::domain::manifest::Manifest;
use crate::domain::tenant_theme::{
    BrandAssets, LayoutOverride, TenantTheme, TenantThemeSnapshot, ThemeState,
};
use crate::domain::theme::{
    Theme, ThemeId, ThemeStatus, ThemeVersion, ThemeVersionId, ThemeVisibility, TokenModes,
};
use crate::domain::tokens::TokenValue;
use crate::domain::{TenantThemeRepository, ThemeRepository};

fn storage(err: impl std::fmt::Display) -> ThemingError {
    ThemingError::Storage(err.to_string())
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxThemeRepository;

impl SqlxThemeRepository {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, sqlx::FromRow)]
struct ThemeRow {
    id: i64,
    key: String,
    publisher: String,
    visibility: String,
    required_feature: Option<String>,
    status: String,
    created_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
struct ThemeVersionRow {
    id: i64,
    theme_id: i64,
    semver: String,
    theme_api_range: String,
    manifest: serde_json::Value,
    tokens: serde_json::Value,
    artifact_uri: String,
    artifact_digest: String,
    published_at: DateTime<Utc>,
    yanked_at: Option<DateTime<Utc>>,
}

fn theme_from_row(row: ThemeRow) -> Result<Theme, ThemingError> {
    Ok(Theme {
        id: ThemeId::try_from(row.id)
            .map_err(|err| storage(format!("stored theme has invalid id: {err}")))?,
        key: row.key,
        publisher: row.publisher,
        visibility: ThemeVisibility::parse(&row.visibility)
            .map_err(|err| storage(format!("stored theme has invalid visibility: {err}")))?,
        required_feature: row.required_feature,
        status: ThemeStatus::parse(&row.status)
            .map_err(|err| storage(format!("stored theme has invalid status: {err}")))?,
        created_at: row.created_at,
    })
}

fn version_from_row(row: ThemeVersionRow) -> Result<ThemeVersion, ThemingError> {
    let semver = Version::parse(&row.semver).map_err(|err| {
        storage(format!(
            "stored version {:?} is not semver: {err}",
            row.semver
        ))
    })?;
    let theme_api_range = VersionReq::parse(&row.theme_api_range).map_err(|err| {
        storage(format!(
            "stored range {:?} is invalid: {err}",
            row.theme_api_range
        ))
    })?;
    let manifest = Manifest::parse(&row.manifest)
        .map_err(|err| storage(format!("stored manifest is invalid: {err}")))?;
    let tokens = TokenModes::parse(&row.tokens)
        .map_err(|err| storage(format!("stored tokens are invalid: {err}")))?;
    Ok(ThemeVersion {
        id: ThemeVersionId::try_from(row.id)
            .map_err(|err| storage(format!("stored version has invalid id: {err}")))?,
        theme_id: ThemeId::try_from(row.theme_id)
            .map_err(|err| storage(format!("stored version has invalid theme_id: {err}")))?,
        semver,
        theme_api_range,
        manifest,
        tokens,
        artifact_uri: row.artifact_uri,
        artifact_digest: row.artifact_digest,
        published_at: row.published_at,
        yanked_at: row.yanked_at,
    })
}

#[async_trait]
impl ThemeRepository for SqlxThemeRepository {
    async fn list(&self, pool: &PgPool) -> Result<Vec<Theme>, ThemingError> {
        let rows = sqlx::query_as!(
            ThemeRow,
            "SELECT id, key, publisher, visibility, required_feature, status, created_at
             FROM theming.themes ORDER BY key"
        )
        .fetch_all(pool)
        .await
        .map_err(storage)?;
        rows.into_iter().map(theme_from_row).collect()
    }

    async fn get_by_key(&self, pool: &PgPool, key: &str) -> Result<Option<Theme>, ThemingError> {
        let row = sqlx::query_as!(
            ThemeRow,
            "SELECT id, key, publisher, visibility, required_feature, status, created_at
             FROM theming.themes WHERE key = $1",
            key
        )
        .fetch_optional(pool)
        .await
        .map_err(storage)?;
        row.map(theme_from_row).transpose()
    }

    async fn insert(
        &self,
        scope: &mut PlatformScope<'_>,
        theme: &Theme,
    ) -> Result<(), ThemingError> {
        sqlx::query!(
            "INSERT INTO theming.themes (id, key, publisher, visibility, required_feature, status)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (key) DO NOTHING",
            theme.id.as_i64(),
            theme.key,
            theme.publisher,
            theme.visibility.as_str(),
            theme.required_feature,
            theme.status.as_str(),
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn set_status(
        &self,
        scope: &mut PlatformScope<'_>,
        theme_id: ThemeId,
        status: &str,
    ) -> Result<(), ThemingError> {
        sqlx::query!(
            "UPDATE theming.themes SET status = $2 WHERE id = $1",
            theme_id.as_i64(),
            status
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn list_versions_page(
        &self,
        pool: &PgPool,
        theme_id: ThemeId,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<ThemeVersion>, ThemingError> {
        let rows: Vec<ThemeVersionRow> = sqlx::query_as(
            "SELECT id, theme_id, semver, theme_api_ranges, manifest, tokens, artifact_uri,
                    artifact_digest, published_at, yanked_at
             FROM theming.theme_versions
             WHERE theme_id = $1 AND ($2::bigint IS NULL OR id < $2)
             ORDER BY id DESC LIMIT $3",
        )
        .bind(theme_id.as_i64())
        .bind(after_id)
        .bind(i64::from(limit))
        .fetch_all(pool)
        .await
        .map_err(storage)?;
        rows.into_iter().map(version_from_row).collect()
    }

    async fn list_versions(
        &self,
        pool: &PgPool,
        theme_id: ThemeId,
    ) -> Result<Vec<ThemeVersion>, ThemingError> {
        let rows = sqlx::query_as!(
            ThemeVersionRow,
            "SELECT id, theme_id, semver, theme_api_range, manifest, tokens, artifact_uri,
                    artifact_digest, published_at, yanked_at
             FROM theming.theme_versions WHERE theme_id = $1
             ORDER BY published_at DESC",
            theme_id.as_i64()
        )
        .fetch_all(pool)
        .await
        .map_err(storage)?;
        rows.into_iter().map(version_from_row).collect()
    }

    async fn get_version(
        &self,
        pool: &PgPool,
        theme_key: &str,
        semver: &str,
    ) -> Result<Option<ThemeVersion>, ThemingError> {
        let row = sqlx::query_as!(
            ThemeVersionRow,
            "SELECT v.id, v.theme_id, v.semver, v.theme_api_range, v.manifest, v.tokens,
                    v.artifact_uri, v.artifact_digest, v.published_at, v.yanked_at
             FROM theming.theme_versions v
             JOIN theming.themes t ON t.id = v.theme_id
             WHERE t.key = $1 AND v.semver = $2",
            theme_key,
            semver
        )
        .fetch_optional(pool)
        .await
        .map_err(storage)?;
        row.map(version_from_row).transpose()
    }

    async fn get_latest_version(
        &self,
        pool: &PgPool,
        theme_key: &str,
    ) -> Result<Option<ThemeVersion>, ThemingError> {
        let row = sqlx::query_as!(
            ThemeVersionRow,
            "SELECT v.id, v.theme_id, v.semver, v.theme_api_range, v.manifest, v.tokens,
                    v.artifact_uri, v.artifact_digest, v.published_at, v.yanked_at
             FROM theming.theme_versions v
             JOIN theming.themes t ON t.id = v.theme_id
             WHERE t.key = $1 AND v.yanked_at IS NULL
             ORDER BY v.published_at DESC
             LIMIT 1",
            theme_key
        )
        .fetch_optional(pool)
        .await
        .map_err(storage)?;
        row.map(version_from_row).transpose()
    }

    async fn insert_version(
        &self,
        scope: &mut PlatformScope<'_>,
        version: &ThemeVersion,
    ) -> Result<(), ThemingError> {
        sqlx::query!(
            "INSERT INTO theming.theme_versions
               (id, theme_id, semver, theme_api_range, manifest, tokens, artifact_uri, artifact_digest)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            version.id.as_i64(),
            version.theme_id.as_i64(),
            version.semver.to_string(),
            version.theme_api_range.to_string(),
            serde_json::to_value(&version.manifest).expect("manifests serialize"),
            version.tokens.to_json(),
            version.artifact_uri,
            version.artifact_digest,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn yank_version(
        &self,
        scope: &mut PlatformScope<'_>,
        theme_id: ThemeId,
        semver: &str,
        at: DateTime<Utc>,
    ) -> Result<(), ThemingError> {
        let result = sqlx::query!(
            "UPDATE theming.theme_versions SET yanked_at = $3
             WHERE theme_id = $1 AND semver = $2 AND yanked_at IS NULL",
            theme_id.as_i64(),
            semver,
            at
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        if result.rows_affected() == 0 {
            return Err(ThemingError::UnknownThemeVersion {
                theme: "?".to_string(),
                version: semver.to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxTenantThemeRepository;

impl SqlxTenantThemeRepository {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, sqlx::FromRow)]
struct TenantThemeRow {
    tenant_id: i64,
    state: String,
    theme_id: i64,
    theme_key: String,
    semver: String,
    token_overrides: serde_json::Value,
    layout_overrides: serde_json::Value,
    assets: serde_json::Value,
    custom_css: Option<String>,
    revision: i32,
    updated_by: Option<i64>,
    updated_at: DateTime<Utc>,
}

fn tenant_from_row(row: TenantThemeRow) -> Result<TenantTheme, ThemingError> {
    let semver = Version::parse(&row.semver)
        .map_err(|err| storage(format!("stored semver {:?} is invalid: {err}", row.semver)))?;
    let token_overrides: std::collections::BTreeMap<String, TokenValue> =
        serde_json::from_value(row.token_overrides)
            .map_err(|err| storage(format!("stored token_overrides are invalid: {err}")))?;
    let layout_overrides: std::collections::BTreeMap<String, LayoutOverride> =
        serde_json::from_value(row.layout_overrides)
            .map_err(|err| storage(format!("stored layout_overrides are invalid: {err}")))?;
    let assets: BrandAssets = serde_json::from_value(row.assets)
        .map_err(|err| storage(format!("stored assets are invalid: {err}")))?;
    let updated_by = match row.updated_by {
        Some(uid) => Some(
            UserId::try_from(uid)
                .map_err(|err| storage(format!("stored updated_by is invalid: {err}")))?,
        ),
        None => None,
    };
    Ok(TenantTheme {
        tenant_id: TenantId::try_from(row.tenant_id)
            .map_err(|err| storage(format!("stored tenant_id is invalid: {err}")))?,
        state: ThemeState::parse(&row.state)
            .map_err(|err| storage(format!("stored state is invalid: {err}")))?,
        theme_id: ThemeId::try_from(row.theme_id)
            .map_err(|err| storage(format!("stored theme_id is invalid: {err}")))?,
        theme_key: row.theme_key,
        semver,
        token_overrides,
        layout_overrides,
        assets,
        custom_css: row.custom_css,
        revision: row.revision,
        updated_by,
        updated_at: row.updated_at,
    })
}

#[async_trait]
impl TenantThemeRepository for SqlxTenantThemeRepository {
    async fn get(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        state: ThemeState,
    ) -> Result<Option<TenantTheme>, ThemingError> {
        let row = sqlx::query_as!(
            TenantThemeRow,
            r#"
            SELECT tt.tenant_id, tt.state, tt.theme_id, t.key AS theme_key,
                   tt.semver, tt.token_overrides, tt.layout_overrides, tt.assets,
                   tt.custom_css, tt.revision, tt.updated_by, tt.updated_at
            FROM theming.tenant_themes tt
            JOIN theming.themes t ON t.id = tt.theme_id
            WHERE tt.tenant_id = $1 AND tt.state = $2
            "#,
            tenant_id.as_i64(),
            state.as_str()
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(tenant_from_row).transpose()
    }

    async fn upsert(
        &self,
        scope: &mut ScopedTx<'_>,
        theme: &TenantTheme,
    ) -> Result<(), ThemingError> {
        sqlx::query!(
            "INSERT INTO theming.tenant_themes
               (tenant_id, state, theme_id, semver, token_overrides, layout_overrides, assets,
                custom_css, revision, updated_by, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (tenant_id, state) DO UPDATE SET
               theme_id = EXCLUDED.theme_id,
               semver = EXCLUDED.semver,
               token_overrides = EXCLUDED.token_overrides,
               layout_overrides = EXCLUDED.layout_overrides,
               assets = EXCLUDED.assets,
               custom_css = EXCLUDED.custom_css,
               revision = EXCLUDED.revision,
               updated_by = EXCLUDED.updated_by,
               updated_at = EXCLUDED.updated_at",
            theme.tenant_id.as_i64(),
            theme.state.as_str(),
            theme.theme_id.as_i64(),
            theme.semver.to_string(),
            serde_json::to_value(&theme.token_overrides).expect("overrides serialize"),
            serde_json::to_value(&theme.layout_overrides).expect("layouts serialize"),
            serde_json::to_value(&theme.assets).expect("assets serialize"),
            theme.custom_css,
            theme.revision,
            theme.updated_by.map(UserId::as_i64),
            theme.updated_at,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn publish(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        published_by: Option<UserId>,
        now: DateTime<Utc>,
    ) -> Result<i32, ThemingError> {
        let draft = self
            .get(scope, tenant_id, ThemeState::Draft)
            .await?
            .ok_or_else(|| {
                ThemingError::InvalidState("there is no draft to publish".to_string())
            })?;
        let live = self.get(scope, tenant_id, ThemeState::Live).await?;
        let revision = live.as_ref().map(|live| live.revision).unwrap_or(0) + 1;

        // Every published state is snapshotted. The state being *replaced*
        // is snapshotted at its own revision (idempotent: publishing the
        // same revision twice cannot happen, but a re-delivered transaction
        // must not duplicate history), so rollback is a pointer move to
        // revision - 1.
        if let Some(previous) = &live
            && previous.revision > 0
        {
            let snapshot = TenantThemeSnapshot::of(previous);
            sqlx::query!(
                "INSERT INTO theming.tenant_theme_revisions
                   (tenant_id, revision, snapshot, published_at, published_by)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (tenant_id, revision) DO NOTHING",
                tenant_id.as_i64(),
                previous.revision,
                serde_json::to_value(&snapshot).expect("snapshots serialize"),
                previous.updated_at,
                previous.updated_by.map(UserId::as_i64)
            )
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(storage)?;
        }

        let mut live_row = draft.clone();
        live_row.state = ThemeState::Live;
        live_row.revision = revision;
        live_row.updated_by = published_by;
        live_row.updated_at = now;
        self.upsert(scope, &live_row).await?;

        let snapshot = TenantThemeSnapshot::of(&live_row);
        sqlx::query!(
            "INSERT INTO theming.tenant_theme_revisions
               (tenant_id, revision, snapshot, published_at, published_by)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (tenant_id, revision) DO NOTHING",
            tenant_id.as_i64(),
            revision,
            serde_json::to_value(&snapshot).expect("snapshots serialize"),
            now,
            published_by.map(UserId::as_i64)
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        Ok(revision)
    }

    async fn rollback(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        published_by: Option<UserId>,
        now: DateTime<Utc>,
    ) -> Result<i32, ThemingError> {
        let live = self
            .get(scope, tenant_id, ThemeState::Live)
            .await?
            .ok_or_else(|| {
                ThemingError::InvalidState("there is no live theme to roll back".to_string())
            })?;

        // The previous look is the snapshot one revision back.
        let target = live.revision - 1;
        #[derive(Debug, sqlx::FromRow)]
        struct SnapshotRow {
            snapshot: serde_json::Value,
        }
        let row = sqlx::query_as!(
            SnapshotRow,
            "SELECT snapshot FROM theming.tenant_theme_revisions
             WHERE tenant_id = $1 AND revision = $2",
            tenant_id.as_i64(),
            target
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?
        .ok_or(ThemingError::NothingToRollBack)?;
        let snapshot: TenantThemeSnapshot = serde_json::from_value(row.snapshot)
            .map_err(|err| storage(format!("stored snapshot is invalid: {err}")))?;

        let revision = live.revision + 1;
        let mut restored = live;
        snapshot.apply(&mut restored);
        restored.state = ThemeState::Live;
        restored.revision = revision;
        restored.updated_by = published_by;
        restored.updated_at = now;
        self.upsert(scope, &restored).await?;

        // The rollback is itself a revision (append-only history, monotonic
        // revision, so the ETag always moves forward).
        let snapshot_again = TenantThemeSnapshot::of(&restored);
        sqlx::query!(
            "INSERT INTO theming.tenant_theme_revisions
               (tenant_id, revision, snapshot, published_at, published_by)
             VALUES ($1, $2, $3, $4, $5)",
            tenant_id.as_i64(),
            revision,
            serde_json::to_value(&snapshot_again).expect("snapshots serialize"),
            now,
            published_by.map(UserId::as_i64)
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        Ok(revision)
    }
}
