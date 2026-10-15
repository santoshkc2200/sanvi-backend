//! Disposable local services and production crypto; never deployment secrets.
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sanvi_kernel::{SnowflakeGenerator, TenantId};
use sanvi_persistence::{PgPool, TxManager};
use sanvi_privacy::{
    adapters::{SqlxEnvelopeCrypto, SqlxSubjectKeyRepository},
    domain::EnvelopeCrypto,
};
use serde::Serialize;
use sha2::Sha256;
use std::{
    path::Path,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Serialize)]
struct Evidence {
    credential: &'static str,
    duration_ms: u128,
    new_accepted: bool,
    old_rejected: bool,
    runbook: &'static str,
}
fn evidence(
    name: &'static str,
    start: Instant,
    new: bool,
    old: bool,
    book: &str,
) -> Result<Evidence> {
    ensure!(new && old, "{name}: acceptance or rejection proof failed");
    let runbook = match book {
        "infra" => "docs/runbooks/rotation/database-redis.md",
        "envelope" => "docs/runbooks/rotation/envelope-keys.md",
        _ => "docs/runbooks/rotation/signing-webhooks.md",
    };
    Ok(Evidence {
        credential: name,
        duration_ms: start.elapsed().as_millis(),
        new_accepted: new,
        old_rejected: old,
        runbook,
    })
}
// Never render command arguments/output, which can contain drill credentials.
fn docker(args: &[&str]) -> Result<String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .context("launch Docker")?;
    ensure!(
        output.status.success(),
        "Docker operation failed; details suppressed to protect credentials"
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
struct Container(String);
impl Drop for Container {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", "-v", &self.0]);
    }
}
fn secret(prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("{prefix}{}", hex::encode(bytes))
}
async fn wait_port(container: &Container, probe: &[&str], internal: &str) -> Result<u16> {
    for _ in 0..120 {
        let mut args = vec!["exec", &container.0];
        args.extend_from_slice(probe);
        if docker(&args).is_ok() {
            let port = docker(&["port", &container.0, internal])?;
            return port
                .rsplit(':')
                .next()
                .context("port missing")?
                .parse()
                .context("invalid port");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("disposable service did not become ready within 30 seconds")
}
fn pg_options(port: u16, user: &str, password: &str) -> sqlx::postgres::PgConnectOptions {
    sqlx::postgres::PgConnectOptions::new()
        .host("127.0.0.1")
        .port(port)
        .database("postgres")
        .username(user)
        .password(password)
}
async fn pg_accepts(port: u16, password: &str) -> Result<bool> {
    match PgPool::connect_with(pg_options(port, "rotation_app", password)).await {
        Ok(pool) => {
            sqlx::query("SELECT 1").execute(&pool).await?;
            pool.close().await;
            Ok(true)
        }
        Err(sqlx::Error::Database(err)) if err.code().as_deref() == Some("28P01") => Ok(false),
        Err(_) => anyhow::bail!("database unavailable; cannot prove credential rejection"),
    }
}
async fn redis_accepts(port: u16, password: &str) -> Result<bool> {
    let client = redis::Client::open(format!("redis://:{password}@127.0.0.1:{port}/"))?;
    match client.get_multiplexed_async_connection().await {
        Ok(mut conn) => Ok(redis::cmd("PING").query_async::<String>(&mut conn).await? == "PONG"),
        Err(err) if err.kind() == redis::ErrorKind::AuthenticationFailed => Ok(false),
        Err(_) => anyhow::bail!("Redis unavailable; cannot prove credential rejection"),
    }
}
fn webhook_signature(secret: &[u8], payload: &[u8]) -> String {
    let timestamp = Utc::now().timestamp();
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC key");
    mac.update(format!("{timestamp}.").as_bytes());
    mac.update(payload);
    format!(
        "t={timestamp},v1={}",
        hex::encode(mac.finalize().into_bytes())
    )
}
pub async fn run(report: &Path) -> Result<()> {
    let started = Utc::now();
    let overall = Instant::now();
    let mut rows = Vec::new();
    let sid = uuid::Uuid::now_v7().simple().to_string();
    let old = secret("rotation-old-");
    let new = secret("rotation-new-");
    let start = Instant::now();
    let pg = Container(format!("sanvi-rotation-pg-{sid}"));
    docker(&[
        "run",
        "-d",
        "--name",
        &pg.0,
        "-p",
        "127.0.0.1::5432",
        "-e",
        &format!("POSTGRES_PASSWORD={new}"),
        "postgres:18-alpine",
        "postgres",
        "-c",
        "password_encryption=scram-sha-256",
    ])?;
    // The image's temporary initialization server accepts Unix sockets
    // before the final server listens on TCP. Probe the transport we use.
    let port = wait_port(
        &pg,
        &["pg_isready", "-h", "127.0.0.1", "-U", "postgres"],
        "5432/tcp",
    )
    .await?;
    let admin = PgPool::connect_with(pg_options(port, "postgres", &new))
        .await
        .map_err(|_| anyhow::anyhow!("drill admin connection failed"))?;
    // Secrets are generated solely from an ASCII prefix and hex random bytes.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE ROLE rotation_app LOGIN PASSWORD '{old}'"
    )))
    .execute(&admin)
    .await?;
    ensure!(
        pg_accepts(port, &old).await?,
        "old database credential never worked"
    );
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER ROLE rotation_app PASSWORD '{new}'"
    )))
    .execute(&admin)
    .await?;
    rows.push(evidence(
        "postgres",
        start,
        pg_accepts(port, &new).await?,
        !pg_accepts(port, &old).await?,
        "infra",
    )?);
    let start = Instant::now();
    let cache = Container(format!("sanvi-rotation-redis-{sid}"));
    docker(&[
        "run",
        "-d",
        "--name",
        &cache.0,
        "-p",
        "127.0.0.1::6379",
        "redis:8-alpine",
        "redis-server",
        "--requirepass",
        &old,
    ])?;
    let redis_port = wait_port(&cache, &["redis-cli", "-a", &old, "PING"], "6379/tcp").await?;
    ensure!(
        redis_accepts(redis_port, &old).await?,
        "old Redis credential never worked"
    );
    docker(&[
        "exec",
        &cache.0,
        "redis-cli",
        "-a",
        &old,
        "CONFIG",
        "SET",
        "requirepass",
        &new,
    ])?;
    rows.push(evidence(
        "redis",
        start,
        redis_accepts(redis_port, &new).await?,
        !redis_accepts(redis_port, &old).await?,
        "infra",
    )?);
    let tenant = TenantId::try_from(101_i64)?;
    let now = Utc::now();
    let start = Instant::now();
    use sanvi_tenancy::adapters::resolution::{SigningKey, mint, verify};
    let k1 = SigningKey {
        id: "old".into(),
        secret: sanvi_telemetry::Redacted::new(old.clone()),
    };
    let k2 = SigningKey {
        id: "new".into(),
        secret: sanvi_telemetry::Redacted::new(new.clone()),
    };
    let retired = mint(&k1, tenant, now);
    let skew = Duration::from_secs(60);
    ensure!(
        verify(&retired, std::slice::from_ref(&k1), now, skew).is_ok(),
        "baseline signing verification"
    );
    ensure!(
        verify(&retired, &[k2.clone(), k1], now, skew).is_ok(),
        "signing overlap failed"
    );
    rows.push(evidence(
        "tenant_signing",
        start,
        verify(
            &mint(&k2, tenant, now),
            std::slice::from_ref(&k2),
            now,
            skew,
        )
        .is_ok(),
        verify(&retired, &[k2], now, skew).is_err(),
        "signing",
    )?);
    let start = Instant::now();
    use sanvi_advertising::application::tracking::site_key::{generate_site_key, verify_site_key};
    let origin = "https://drill.invalid";
    let retired = generate_site_key(&old, tenant, origin)?;
    ensure!(
        verify_site_key(&old, &retired, origin).is_ok(),
        "baseline tracking verification"
    );
    rows.push(evidence(
        "tracking_signing",
        start,
        verify_site_key(&new, &generate_site_key(&new, tenant, origin)?, origin).is_ok(),
        verify_site_key(&new, &retired, origin).is_err(),
        "signing",
    )?);
    let start = Instant::now();
    use sanvi_advertising::domain::{OAuthStateError, OAuthStateManager, PlatformKey};
    let platform = PlatformKey::parse("google_ads")?;
    let issuer = OAuthStateManager::new(old.as_bytes());
    let replacement = OAuthStateManager::new(new.as_bytes());
    let retired = issuer
        .issue(tenant, platform.clone(), now, chrono::Duration::minutes(5))
        .await?;
    let baseline = issuer
        .issue(tenant, platform.clone(), now, chrono::Duration::minutes(5))
        .await?;
    ensure!(
        issuer
            .redeem(&baseline, tenant, &platform, now)
            .await
            .is_ok(),
        "baseline OAuth verification"
    );
    let fresh = replacement
        .issue(tenant, platform.clone(), now, chrono::Duration::minutes(5))
        .await?;
    let rejected = matches!(
        replacement.redeem(&retired, tenant, &platform, now).await,
        Err(OAuthStateError::Forged)
    );
    rows.push(evidence(
        "oauth_state_signing",
        start,
        replacement
            .redeem(&fresh, tenant, &platform, now)
            .await
            .is_ok(),
        rejected,
        "signing",
    )?);
    let start = Instant::now();
    use sanvi_theming::{ThemeState, sign_preview_token, verify_preview_token};
    let retired = sign_preview_token(old.as_bytes(), tenant, ThemeState::Draft, 60, now);
    ensure!(
        verify_preview_token(old.as_bytes(), &retired, now).is_ok(),
        "baseline preview verification"
    );
    rows.push(evidence(
        "preview_signing",
        start,
        verify_preview_token(
            new.as_bytes(),
            &sign_preview_token(new.as_bytes(), tenant, ThemeState::Draft, 60, now),
            now,
        )
        .is_ok(),
        verify_preview_token(new.as_bytes(), &retired, now).is_err(),
        "signing",
    )?);
    let payload = br#"{"id":"evt_rotation"}"#;
    let start = Instant::now();
    let retired = webhook_signature(old.as_bytes(), payload);
    let fresh = webhook_signature(new.as_bytes(), payload);
    use sanvi_billing::adapters::stripe::verify_webhook_signature as billing_verify;
    ensure!(
        billing_verify(old.as_bytes(), payload, &retired, 300).is_ok(),
        "baseline billing signature"
    );
    rows.push(evidence(
        "billing_webhook",
        start,
        billing_verify(new.as_bytes(), payload, &fresh, 300).is_ok(),
        billing_verify(new.as_bytes(), payload, &retired, 300).is_err(),
        "signing",
    )?);
    let start = Instant::now();
    use sanvi_payments::domain::verify_webhook_signature as connect_verify;
    ensure!(
        connect_verify(old.as_bytes(), payload, &retired, 300).is_ok(),
        "baseline Connect signature"
    );
    rows.push(evidence(
        "connect_webhook",
        start,
        connect_verify(new.as_bytes(), payload, &fresh, 300).is_ok(),
        connect_verify(new.as_bytes(), payload, &retired, 300).is_err(),
        "signing",
    )?);
    let start = Instant::now();
    let mut old_kek = [0u8; 32];
    let mut new_kek = [0u8; 32];
    let mut dek = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut old_kek);
    rand::rngs::OsRng.fill_bytes(&mut new_kek);
    rand::rngs::OsRng.fill_bytes(&mut dek);
    let tx = TxManager::new(
        admin.clone(),
        Arc::new(SnowflakeGenerator::new(sanvi_kernel::NodeId::new(3)?)),
    );
    let previous = SqlxEnvelopeCrypto::new(
        tx.clone(),
        Arc::new(SqlxSubjectKeyRepository::new()),
        old_kek,
        "old".into(),
    );
    let replacement = SqlxEnvelopeCrypto::new(
        tx,
        Arc::new(SqlxSubjectKeyRepository::new()),
        new_kek,
        "new".into(),
    );
    let wrapped = previous.wrap(&dek, "old")?;
    let rewrapped = replacement.wrap(&previous.unwrap(&wrapped, "old")?, "new")?;
    sqlx::query("CREATE TABLE rotation_keys (kind text PRIMARY KEY, wrapped bytea NOT NULL, kek_id text NOT NULL)").execute(&admin).await?;
    sqlx::query("INSERT INTO rotation_keys VALUES ('privacy', $1, 'new')")
        .bind(&rewrapped)
        .execute(&admin)
        .await?;
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT wrapped FROM rotation_keys WHERE kind='privacy'")
            .fetch_one(&admin)
            .await?;
    rows.push(evidence(
        "privacy_kek",
        start,
        replacement.unwrap(&stored, "new")? == dek,
        previous.unwrap(&stored, "new").is_err(),
        "envelope",
    )?);
    let start = Instant::now();
    use sanvi_advertising::adapters::SqlxTenantEnvelopeCrypto;
    let previous = SqlxTenantEnvelopeCrypto::new(old_kek, "old".into());
    let replacement = SqlxTenantEnvelopeCrypto::new(new_kek, "new".into());
    let rewrapped = previous.rewrap_dek(&wrapped, &replacement)?;
    sqlx::query("INSERT INTO rotation_keys VALUES ('advertising', $1, 'new')")
        .bind(&rewrapped)
        .execute(&admin)
        .await?;
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT wrapped FROM rotation_keys WHERE kind='advertising'")
            .fetch_one(&admin)
            .await?;
    rows.push(evidence(
        "advertising_kek",
        start,
        replacement.rewrap_dek(&stored, &replacement).is_ok(),
        previous.rewrap_dek(&stored, &previous).is_err(),
        "envelope",
    )?);
    admin.close().await;
    docker(&["rm", "-f", "-v", &cache.0])?;
    docker(&["rm", "-f", "-v", &pg.0])?;
    drop(cache);
    drop(pg);
    if let Some(parent) = report.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let document = serde_json::json!({
        "scope": "local rehearsal; provider revocations parked", "started_at": started, "finished_at": Utc::now(), "duration_ms": overall.elapsed().as_millis(),
        "machine": {"os": std::env::consts::OS, "arch": std::env::consts::ARCH, "cores": std::thread::available_parallelism()?.get()}, "credentials": rows,
        "parked": ["Kratos webhook/admin and session signing keys", "Keto deployment credentials (none held by this backend)", "Stripe platform and restricted API keys, provider webhook revocation", "Google Ads developer token and OAuth client secret", "Meta OAuth client secret", "media service API key", "notification send/read keys", "OpenObserve OTLP auth token", "domains internal shared secret"],
        "non_rotating": ["Connect onboarding secret: ephemeral, single use", "subject and advertising DEKs: preserve during KEK rewrap; destroy subject DEKs on erasure", "OAuth refresh tokens: provider revocation parked", "export passphrase: per-artifact"],
        "limitations": ["Fresh connections tested; existing DB/Redis sessions require drain/restart", "KEK drill rewraps a persisted DEK with production crypto; deployment-wide migration and ciphertext continuity require the envelope runbook", "Durations are local; provider propagation and overlap windows excluded"]
    });
    std::fs::write(report, serde_json::to_vec_pretty(&document)?)?;
    println!(
        "Local rotation rehearsal passed; evidence: {}",
        report.display()
    );
    Ok(())
}
