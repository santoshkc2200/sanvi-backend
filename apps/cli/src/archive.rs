//! TASK-028 (FR-1129): archival operator commands — `archive` and
//! `archive-restore`.
//!
//! The mover itself lives in `sanvi-archival`; this module is the
//! operator surface: resolve the tenant, refuse loudly on overlap, move
//! (or restore) one range, print the evidence. The connection uses the
//! migrator role (like `migrate`): lifting the append-only guard for the
//! delete is an owner operation, and archival *is* the documented
//! removal path for those tables.
//!
//! Encryption is the phase-05 envelope scheme's passphrase form: the
//! same `SqlxEnvelopeCrypto` the exports use, with a passphrase derived
//! from the configured KEK (HMAC over a fixed context label). Rotating
//! the KEK rewraps subject DEKs but does not change already-written
//! archives — those were sealed under the passphrase of the KEK that
//! wrote them, so the rotation runbook archives-and-restores under both
//! keys when needed (docs/runbooks/rotation/envelope-keys.md).

use std::sync::Arc;

use sanvi_archival::mover::{ArchiveMover, ArchiveTarget};
use sanvi_archival::ranges;
use sanvi_archival::storage::{LocalEncryptedVolume, SealedStorage};
use sanvi_privacy::adapters::archival_cipher::EnvelopeArchiveCipher;
use sanvi_privacy::adapters::crypto::SqlxEnvelopeCrypto;
use sanvi_privacy::adapters::repos::SqlxSubjectKeyRepository;
use sanvi_privacy::domain::ports::EnvelopeCrypto;

#[derive(Clone)]
pub struct ArchiveArgs {
    /// `schema.table` of the archival target (must be partitioned and
    /// enrolled where applicable; validated against the catalog).
    pub table: String,
    /// Tenant slug, decimal id, or `platform` for the platform scope.
    pub tenant: String,
    /// Inclusive range start (RFC 3339).
    pub from: String,
    /// Exclusive range end (RFC 3339).
    pub until: String,
    pub dry_run: bool,
    pub user: Option<String>,
    pub password: Option<String>,
}

#[derive(Clone)]
pub struct ArchiveRestoreArgs {
    pub id: i64,
    pub dry_run: bool,
    pub user: Option<String>,
    pub password: Option<String>,
}

struct Prepared {
    tx: sanvi_persistence::TxManager,
    storage: Arc<SealedStorage>,
    ids: Arc<dyn sanvi_kernel::RawIdGenerator>,
}

/// Connect as the migrator (owner) and build the mover with the real
/// cipher. Fails closed when no KEK is configured: an archive that
/// cannot be decrypted is a deletion with extra steps.
async fn prepare(
    environment: &str,
    user: Option<String>,
    password: Option<String>,
) -> anyhow::Result<Prepared> {
    let mut cli_config = crate::config::load(environment)?;
    if let Some(user) = user {
        cli_config.database.user = user;
    }
    if let Some(password) = password {
        cli_config.database.password = password;
    }
    // DDL (guard lifting) and range deletes legitimately outlive any
    // request-shaped budget.
    cli_config.database.statement_timeout_ms = 0;

    if !cli_config.privacy.encryption_enabled() {
        anyhow::bail!(
            "archival requires the phase-05 envelope KEK (SANVI__PRIVACY__KEK); \
             an archive that cannot be decrypted is deletion with extra steps"
        );
    }
    let kek: [u8; 32] = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        cli_config.privacy.kek.expose_secret(),
    )?
    .try_into()
    .map_err(|bytes: Vec<u8>| anyhow::anyhow!("KEK must be 32 bytes, got {}", bytes.len()))?;

    let ids: Arc<dyn sanvi_kernel::RawIdGenerator> = Arc::new(
        sanvi_kernel::SnowflakeGenerator::new(sanvi_kernel::NodeId::new(23)?),
    );
    let pool = sanvi_persistence::build_pool(&cli_config.database).await?;
    let tx = sanvi_persistence::TxManager::new(pool, ids.clone());
    let crypto: Arc<dyn EnvelopeCrypto> = Arc::new(SqlxEnvelopeCrypto::new(
        sanvi_persistence::TxManager::new(
            sanvi_persistence::build_pool(&cli_config.database).await?,
            ids.clone(),
        ),
        Arc::new(SqlxSubjectKeyRepository::new()),
        kek,
        cli_config.privacy.kek_id.clone(),
    ));
    let storage = Arc::new(SealedStorage::new(
        Arc::new(LocalEncryptedVolume::new(&cli_config.archival.volume)),
        Arc::new(EnvelopeArchiveCipher::new(crypto, &kek)),
    ));
    Ok(Prepared { tx, storage, ids })
}

pub async fn run_archive(environment: &str, args: ArchiveArgs) -> anyhow::Result<()> {
    let (schema_name, table_name) = args
        .table
        .split_once('.')
        .ok_or_else(|| anyhow::anyhow!("--table must be schema.table, got '{}'", args.table))?;
    let range_start: chrono::DateTime<chrono::Utc> = args
        .from
        .parse()
        .map_err(|err| anyhow::anyhow!("--from must be RFC 3339: {err}"))?;
    let range_end: chrono::DateTime<chrono::Utc> = args
        .until
        .parse()
        .map_err(|err| anyhow::anyhow!("--until must be RFC 3339: {err}"))?;
    if range_end <= range_start {
        anyhow::bail!("--until must be after --from");
    }

    let prepared = prepare(environment, args.user, args.password).await?;
    let tenant_id = resolve_scope(prepared.tx.pool(), &args.tenant).await?;

    // The range key column: `occurred_at`/`received_at` by convention of
    // every partitioned table; read it from the registry when enrolled,
    // fall back to the catalog check for unenrolled tables (archival is
    // legal on any ranged table, the registry is for partitioning).
    let key_column = resolve_key_column(prepared.tx.pool(), schema_name, table_name).await?;

    if args.dry_run {
        let count = dry_run_count(
            prepared.tx.pool(),
            schema_name,
            table_name,
            &key_column,
            tenant_id,
            range_start,
            range_end,
        )
        .await?;
        println!(
            "archive: DRY RUN — {}.{} scope '{}' range {range_start}..{range_end}: {count} row(s) would move; nothing is written",
            schema_name, table_name, args.tenant
        );
        return Ok(());
    }

    let mover = ArchiveMover::new(prepared.tx, prepared.ids, prepared.storage);
    let outcome = mover
        .archive(&ArchiveTarget {
            schema_name: schema_name.to_string(),
            table_name: table_name.to_string(),
            key_column,
            tenant_id,
            range_start,
            range_end,
        })
        .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "command": "archive",
            "archive_id": outcome.archive_id.to_string(),
            "table": outcome.table,
            "tenant_id": outcome.tenant_id.map(|id| id.to_string()),
            "range_start": outcome.range_start,
            "range_end": outcome.range_end,
            "row_count": outcome.row_count,
            "checksum": outcome.checksum,
            "sealed_bytes": outcome.bytes,
            "duration_ms": outcome.duration_ms,
        }))?
    );
    Ok(())
}

pub async fn run_archive_restore(
    environment: &str,
    args: ArchiveRestoreArgs,
) -> anyhow::Result<()> {
    let prepared = prepare(environment, args.user, args.password).await?;

    let record = ranges::by_id(prepared.tx.pool(), args.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("archive {} does not exist", args.id))?;

    if args.dry_run {
        println!(
            "archive-restore: DRY RUN — archive {} of {}.{} (scope {}, {}..{}, {} rows, checksum {}); nothing is written",
            record.id,
            record.schema_name,
            record.table_name,
            record
                .tenant_id
                .map(|id| id.to_string())
                .as_deref()
                .unwrap_or("platform"),
            record.range_start,
            record.range_end,
            record.row_count,
            record.checksum,
        );
        return Ok(());
    }

    let mover = ArchiveMover::new(prepared.tx, prepared.ids, prepared.storage);
    let outcome = mover.restore(args.id).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "command": "archive-restore",
            "archive_id": outcome.archive_id.to_string(),
            "table": outcome.table,
            "row_count": outcome.row_count,
            "checksum": outcome.checksum,
            "duration_ms": outcome.duration_ms,
        }))?
    );
    Ok(())
}

/// `platform` | decimal snowflake | tenant slug.
async fn resolve_scope(pool: &sqlx::PgPool, tenant: &str) -> anyhow::Result<Option<i64>> {
    if tenant == "platform" {
        return Ok(None);
    }
    if let Ok(id) = tenant.parse::<i64>() {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tenancy.tenants WHERE id = $1)")
                .bind(id)
                .fetch_one(pool)
                .await?;
        if exists {
            return Ok(Some(id));
        }
    }
    let id: Option<i64> = sqlx::query_scalar("SELECT id FROM tenancy.tenants WHERE slug = $1")
        .bind(tenant)
        .fetch_optional(pool)
        .await?;
    id.map(Some)
        .ok_or_else(|| anyhow::anyhow!("tenant '{tenant}' does not exist"))
}

async fn resolve_key_column(
    pool: &sqlx::PgPool,
    schema: &str,
    table: &str,
) -> anyhow::Result<String> {
    let enrolled: Option<String> = sqlx::query_scalar(
        "SELECT key_column FROM platform.partitioned_tables
         WHERE schema_name = $1 AND table_name = $2",
    )
    .bind(schema)
    .bind(table)
    .fetch_optional(pool)
    .await?;
    if let Some(key) = enrolled {
        return Ok(key);
    }
    anyhow::bail!(
        "{schema}.{table} is not enrolled in platform.partitioned_tables; \
         archival targets must be registry-partitioned tables (FR-1128 first)"
    )
}

#[allow(clippy::too_many_arguments)]
async fn dry_run_count(
    pool: &sqlx::PgPool,
    schema: &str,
    table: &str,
    key_column: &str,
    tenant_id: Option<i64>,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<i64> {
    let tenant_clause = match tenant_id {
        Some(tenant) => format!(" AND tenant_id = {tenant}"),
        None => " AND tenant_id IS NULL".to_string(),
    };
    let statement = format!(
        "SELECT count(*) FROM \"{}\".\"{}\" WHERE \"{}\" >= $1 AND \"{}\" < $2{tenant_clause}",
        schema.replace('"', "\"\""),
        table.replace('"', "\"\""),
        key_column.replace('"', "\"\""),
        key_column.replace('"', "\"\""),
    );
    let mut conn = pool.acquire().await?;
    let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(statement))
        .bind(start)
        .bind(end)
        .fetch_one(&mut *conn)
        .await?;
    Ok(count)
}
