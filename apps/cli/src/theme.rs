//! `theme` — the phase-07 installation pipeline.
//!
//! `theme publish <dir>` validates a theme artifact directory (manifest
//! schema, `themeApi` range, DTCG token schema, WCAG AA contrast gate),
//! builds the content-addressed zip artifact and installs the version row —
//! the same code path CI runs for first-party themes, so "installable" is
//! proven, not asserted. No application deploy is involved.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use sanvi_kernel::RawIdGenerator;
use sha2::{Digest, Sha256};

use sanvi_persistence::build_pool;
use sanvi_theming::adapters::{
    LocalFileStore, OutboxEventPublisher, SqlxThemeRepository, SystemClock,
};
use sanvi_theming::application::install::InstallThemeVersionCommand;
use sanvi_theming::application::{InstallThemeVersion, ListThemeVersionsAdmin, ListThemesAdmin};
use sanvi_theming::domain::manifest::Manifest;
use sanvi_theming::domain::theme::TokenModes;
use sanvi_theming::domain::{ArtifactStore, Clock, EventPublisher, ThemeRepository, check_theme};

/// Publish one theme artifact directory into the registry.
pub async fn publish(
    environment: &str,
    dir: &str,
    publisher: &str,
    visibility: &str,
    required_feature: Option<&str>,
    allow_contrast_warnings: bool,
    ids: std::sync::Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let config = crate::config::load(environment)?;
    let theme_dir = PathBuf::from(dir);

    // 1. Manifest + tokens, read from the directory the same way the API's
    //    install pipeline validates them again before insert.
    let manifest_value: serde_json::Value = {
        let raw = std::fs::read_to_string(theme_dir.join("theme.json"))
            .with_context(|| format!("{dir}/theme.json is unreadable"))?;
        serde_json::from_str(&raw).context("theme.json is not valid JSON")?
    };
    let manifest = Manifest::parse(&manifest_value).context("manifest validation failed")?;

    let light = read_token_file(&theme_dir, &manifest.tokens.light)?;
    let dark = match &manifest.tokens.dark {
        Some(path) => Some(read_token_file(&theme_dir, path)?),
        None => None,
    };
    let tokens = TokenModes::parse(&serde_json::json!({
        "light": light,
        "dark": dark,
    }))
    .context("token validation failed")?;

    // 2. Contrast gate (WCAG AA on every defined semantic pair).
    if !allow_contrast_warnings {
        check_theme(&tokens.light, tokens.dark.as_ref()).context("contrast gate failed")?;
    } else {
        eprintln!(
            "warning: contrast failures were explicitly allowed for {}-{}",
            manifest.id, manifest.version
        );
    }

    // 3. Content-addressed artifact (zip of the whole directory).
    let artifact_bytes = build_artifact(&theme_dir)?;
    let digest = hex::encode(Sha256::digest(&artifact_bytes));
    let artifact_key = format!("{}-{}-{digest}.zip", manifest.id, manifest.version);
    let store = std::sync::Arc::new(LocalFileStore::new(config.theming.artifact_dir.clone()));
    store
        .put(&artifact_key, &artifact_bytes)
        .await
        .with_context(|| format!("failed to store artifact {artifact_key}"))?;

    // 4. Registry insert, in the audited platform scope, atomically with
    //    the published event.
    let pool = build_pool(&config.database).await?;
    let themes: std::sync::Arc<dyn ThemeRepository> =
        std::sync::Arc::new(SqlxThemeRepository::new());
    let events: std::sync::Arc<dyn EventPublisher> =
        std::sync::Arc::new(OutboxEventPublisher::new(ids.clone()));
    let clock: std::sync::Arc<dyn Clock> = std::sync::Arc::new(SystemClock);
    let install = InstallThemeVersion::new(themes, events, clock, pool.clone(), store, ids.clone());

    let tx_manager = sanvi_persistence::TxManager::new(pool, ids);
    let mut scope = tx_manager
        .begin_unscoped("theming.theme.publish", "cli")
        .await
        .context("failed to open the platform scope")?;
    let view = install
        .execute(
            &mut scope,
            &InstallThemeVersionCommand {
                manifest: manifest_value,
                tokens: serde_json::json!({ "light": light, "dark": dark }),
                artifact_uri: artifact_key,
                artifact_digest: digest,
                publisher: publisher.to_string(),
                visibility: visibility.to_string(),
                required_feature: required_feature.map(str::to_string),
                allow_contrast_warnings,
            },
        )
        .await;
    match view {
        Ok(view) => {
            scope.commit().await.context("failed to commit")?;
            println!(
                "published {}@{} (artifact {}, digest {})",
                manifest.id, view.semver, view.artifact_uri, view.artifact_digest
            );
            Ok(())
        }
        Err(err) => {
            let _ = scope.rollback().await;
            Err(anyhow::anyhow!("install failed: {err}"))
        }
    }
}

/// List the registry (themes with version history).
pub async fn list(environment: &str) -> anyhow::Result<()> {
    let config = crate::config::load(environment)?;
    let pool = build_pool(&config.database).await?;
    let themes: std::sync::Arc<dyn ThemeRepository> =
        std::sync::Arc::new(SqlxThemeRepository::new());
    let views = ListThemesAdmin::new(themes, pool)
        .execute()
        .await
        .context("failed to read the registry")?;
    println!("{}", serde_json::to_string_pretty(&views)?);
    Ok(())
}

/// List one theme's version history.
pub async fn versions(environment: &str, key: &str) -> anyhow::Result<()> {
    let config = crate::config::load(environment)?;
    let pool = build_pool(&config.database).await?;
    let themes: std::sync::Arc<dyn ThemeRepository> =
        std::sync::Arc::new(SqlxThemeRepository::new());
    let views = ListThemeVersionsAdmin::new(themes, pool)
        .execute(key)
        .await
        .context("failed to read the theme versions")?;
    println!("{}", serde_json::to_string_pretty(&views)?);
    Ok(())
}

/// Withdraw a version (the kill switch; pinned tenants keep rendering).
pub async fn yank(
    environment: &str,
    key: &str,
    version: &str,
    ids: std::sync::Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let config = crate::config::load(environment)?;
    let pool = build_pool(&config.database).await?;
    let themes: std::sync::Arc<dyn ThemeRepository> =
        std::sync::Arc::new(SqlxThemeRepository::new());
    let events: std::sync::Arc<dyn EventPublisher> =
        std::sync::Arc::new(OutboxEventPublisher::new(ids.clone()));
    let clock: std::sync::Arc<dyn Clock> = std::sync::Arc::new(SystemClock);
    let use_case =
        sanvi_theming::application::YankThemeVersion::new(themes, events, clock, pool.clone());

    let tx_manager = sanvi_persistence::TxManager::new(pool, ids);
    let mut scope = tx_manager
        .begin_unscoped("theming.theme.yank", "cli")
        .await
        .context("failed to open the platform scope")?;
    match use_case.execute(&mut scope, key, version).await {
        Ok(()) => {
            scope.commit().await.context("failed to commit")?;
            println!("withdrawn {key}@{version}");
            Ok(())
        }
        Err(err) => {
            let _ = scope.rollback().await;
            Err(anyhow::anyhow!("yank failed: {err}"))
        }
    }
}

fn read_token_file(theme_dir: &Path, path: &str) -> anyhow::Result<serde_json::Value> {
    let full = theme_dir.join(path);
    let raw = std::fs::read_to_string(&full)
        .with_context(|| format!("token file {} is unreadable", full.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("{} is not valid JSON", full.display()))
}

/// Build the zip artifact of the whole theme directory (deterministic
/// order, no absolute paths).
fn build_artifact(dir: &Path) -> anyhow::Result<Vec<u8>> {
    let mut entries: Vec<PathBuf> = walk(dir)?;
    entries.sort();

    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for entry in &entries {
            let relative = entry
                .strip_prefix(dir)
                .expect("entries come from the directory");
            let name = relative.to_string_lossy().replace('\\', "/");
            writer.start_file(name, options)?;
            let bytes = std::fs::read(entry)?;
            writer.write_all(&bytes)?;
        }
        writer.finish()?;
    }
    Ok(cursor.into_inner())
}

fn walk(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)
            .with_context(|| format!("cannot read {}", current.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    if out.is_empty() {
        bail!("theme directory is empty");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_reproducible_artifact() {
        let dir =
            std::env::temp_dir().join(format!("sanvi-theme-artifact-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join("tokens")).unwrap();
        std::fs::write(dir.join("theme.json"), r#"{"id":"base"}"#).unwrap();
        std::fs::write(dir.join("tokens/light.json"), "{}").unwrap();
        let first = build_artifact(&dir).unwrap();
        let second = build_artifact(&dir).unwrap();
        assert_eq!(first, second);
        assert!(!first.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
