//! TASK-028 (FR-1131): tenant offboarding — export-then-purge.
//!
//! The purge is deliberately not reversible, so the shape is: dry-run,
//! a verified export as a hard precondition, and a typed confirmation
//! naming the tenant (the replacement for the two-person confirmation
//! this deployment cannot have). The durable record the API created
//! (`tenancy.offboarding_requests`) advances through
//! pending → exporting → exported → purging → purged, and the purge
//! step is unreachable — in the record's own state machine — before
//! the export verifies.
//!
//! The export reuses the phase-05 machinery rather than reinventing it:
//! the DSR flow's `ZipPackager` (data.json + data.csv + data.html + a
//! per-file-checksum manifest) and the envelope scheme's passphrase
//! encryption, with a one-time random passphrase printed to stderr for
//! the operator to convey. The purge is catalog-driven across every
//! context (children before parents, FK topological order), keeps the
//! lawfully retained classes (statutory windows from
//! `privacy.retention_rules`, with the reason and retention end date in
//! the report), sweeps Redis caches and the tenant's cold archives, and
//! finishes with a residual scan over every table and partition child
//! **by name** that fails loudly on anything unexpected — and on a
//! neighbour tenant that is not byte-identical to before.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Context as _;
use sqlx::Connection as _;

use sanvi_archival::ranges;
use sanvi_archival::storage::{LocalEncryptedVolume, SealedStorage};
use sanvi_kernel::RawIdGenerator;
use sanvi_persistence::TxManager;
use sanvi_privacy::adapters::archival_cipher::EnvelopeArchiveCipher;
use sanvi_privacy::adapters::crypto::SqlxEnvelopeCrypto;
use sanvi_privacy::adapters::packager::ZipPackager;
use sanvi_privacy::adapters::repos::SqlxSubjectKeyRepository;
use sanvi_privacy::domain::ports::{EnvelopeCrypto, ExportPackager};
use sanvi_privacy::domain::types::{DataBundle, DataEntry, SubjectKind, SubjectRef};
use sanvi_tenancy::adapters::repo::SqlxOffboardingRepository;
use sanvi_tenancy::adapters::repo::SqlxTenantRepository;
use sanvi_tenancy::application::{
    AdvanceOffboarding, GetOffboarding, RequestOffboarding, SystemClock as TenancySystemClock,
};
use sanvi_tenancy::domain::{OffboardingStatus, TenancyError};

/// Schemas the export and purge cover — the same enumeration
/// `tenant-restore` proved complete.
use crate::tenant_restore::APP_SCHEMAS;

/// Tables the purge never deletes, with the reason. Everything here is
/// either lawfully retained (the audit chain is append-only by
/// construction: deleting tenant rows would break verification for
/// every other tenant) or is the evidence of the offboarding itself.
const RETAINED_ALWAYS: &[(&str, &str, &str)] = &[
    (
        "audit",
        "entries",
        "the platform's own hash-chained audit trail is append-only by construction; deleting rows would break chain verification for every tenant (audit_log class: >= 1 year by construction, archival is the removal path)",
    ),
    (
        "tenancy",
        "tenants",
        "the tombstone row: FK anchor for the statutorily retained records (every tenant-keyed table cascades from it) and the pin against slug/id reuse",
    ),
    (
        "tenancy",
        "offboarding_requests",
        "the offboarding evidence row itself: the purge report must survive the purge",
    ),
];

#[derive(Clone)]
pub struct OffboardArgs {
    pub tenant: String,
    /// The idempotency key of the offboarding record (the API's POST or
    /// this command creates it).
    pub key: String,
    pub stage: Option<String>,
    pub confirm: Option<String>,
    pub dry_run: bool,
    /// A neighbour tenant whose data must be byte-identical after the
    /// purge (checksummed before and after).
    pub neighbour: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct TableCount {
    table: String,
    rows: i64,
}

pub async fn run(environment: &str, args: OffboardArgs) -> anyhow::Result<()> {
    let mut cli_config = crate::config::load(environment)?;
    if let Some(user) = args.user.clone() {
        cli_config.database.user = user;
    }
    if let Some(password) = args.password.clone() {
        cli_config.database.password = password;
    }
    // The purge deletes across every context and lifts the audit guard;
    // it legitimately outlives any request-shaped budget.
    cli_config.database.statement_timeout_ms = 0;

    let app_pool = sanvi_persistence::build_pool(&cli_config.database).await?;

    // The scan runs after the purge, when the tenant row — and with it
    // the slug — is gone: it takes the decimal id, exactly like the
    // purge report records it.
    let tenant_id = if args.stage.as_deref() == Some("scan") {
        args.tenant
            .parse::<i64>()
            .with_context(|| format!("scan takes the tenant's decimal id, got '{}'", args.tenant))?
    } else {
        resolve_tenant(&app_pool, &args.tenant).await?
    };

    match args.stage.as_deref() {
        None if args.dry_run => plan(&app_pool, tenant_id, &args).await,
        Some("plan") => plan(&app_pool, tenant_id, &args).await,
        Some("export") => export(&app_pool, &cli_config, tenant_id, &args).await,
        Some("purge") => purge(&app_pool, &cli_config, tenant_id, &args).await,
        Some("scan") => scan(&app_pool, &args, tenant_id).await,
        other => anyhow::bail!(
            "unknown stage {:?}; use plan | export | purge | scan (or --dry-run for the plan)",
            other
        ),
    }
}

/// Slug first, then decimal id: after a purge only the id exists (the
/// slug row is gone), and the residual scan runs precisely then.
async fn resolve_tenant(pool: &sqlx::PgPool, slug_or_id: &str) -> anyhow::Result<i64> {
    let id: Option<i64> = sqlx::query_scalar("SELECT id FROM tenancy.tenants WHERE slug = $1")
        .bind(slug_or_id)
        .fetch_optional(pool)
        .await?;
    if let Some(id) = id {
        return Ok(id);
    }
    if let Ok(id) = slug_or_id.parse::<i64>() {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tenancy.tenants WHERE id = $1)")
                .bind(id)
                .fetch_one(pool)
                .await?;
        if exists {
            return Ok(id);
        }
    }
    anyhow::bail!("tenant '{slug_or_id}' does not exist")
}

/// One catalog-driven plan: every tenant table (FK topological order,
/// parents first for reading), row counts, the archives, the retained
/// preview. Reads under the audited platform scope; writes nothing.
async fn plan(pool: &sqlx::PgPool, tenant_id: i64, args: &OffboardArgs) -> anyhow::Result<()> {
    let tables = tenant_tables(pool).await?;
    let mut counts = Vec::new();
    {
        let manager = TxManager::new(pool.clone(), cli_ids()?);
        let mut scope = manager
            .begin_unscoped("offboard_plan", "cli:offboard")
            .await?;
        let conn = &mut **scope.tx_mut().as_inner();
        for table in &tables {
            // The anchor row's id *is* the tenant: counted by id, like
            // tenant-restore special-cases it.
            let count = if table.qualified() == "tenancy.tenants" {
                1
            } else {
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT count(*) FROM {}.{} WHERE tenant_id = $1",
                    quote(table.schema.as_str()),
                    quote(table.name.as_str())
                )))
                .bind(tenant_id)
                .fetch_one(&mut *conn)
                .await?
            };
            counts.push(TableCount {
                table: table.qualified(),
                rows: count,
            });
        }
    }

    let archives = tenant_archives(pool, tenant_id).await?;
    let retained = retained_preview(pool, tenant_id).await?;

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "command": "offboard",
            "stage": "plan",
            "dry_run": true,
            "tenant": args.tenant,
            "tenant_id": tenant_id.to_string(),
            "tables": counts,
            "archives": archives.iter().map(|a| serde_json::json!({
                "id": a.id.to_string(),
                "table": a.schema_table(),
                "range": format!("{}..{}", a.range_start, a.range_end),
                "status": a.status,
            })).collect::<Vec<_>>(),
            "lawfully_retained_preview": retained,
            "note": "nothing is written by the plan; export then purge",
        }))?
    );
    Ok(())
}

/// Every tenant-keyed base table in FK topological order (parents
/// first) — the purge reverses this.
async fn tenant_tables(pool: &sqlx::PgPool) -> anyhow::Result<Vec<TableRef>> {
    let mut conn = pool.acquire().await?;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.table_schema, c.table_name
         FROM information_schema.columns c
         JOIN information_schema.tables t
           ON t.table_schema = c.table_schema AND t.table_name = c.table_name
          AND t.table_type = 'BASE TABLE'
         WHERE c.column_name = 'tenant_id' AND c.table_schema = ANY($1)
         GROUP BY 1, 2 ORDER BY 1, 2",
    )
    .bind(
        APP_SCHEMAS
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut tables: Vec<TableRef> = rows
        .into_iter()
        .map(|(schema, name)| TableRef {
            schema,
            name,
            pk: Vec::new(),
        })
        .collect();
    tables.push(TableRef {
        schema: "tenancy".to_string(),
        name: "tenants".to_string(),
        pk: Vec::new(),
    });

    // Kahn over the FK edges (same discipline as tenant-restore: a cycle
    // is a schema fact the plan refuses to guess through).
    let fks: Vec<(String, String)> = sqlx::query_as(
        "SELECT conrelid::regclass::text, confrelid::regclass::text
         FROM pg_constraint WHERE contype = 'f'",
    )
    .fetch_all(&mut *conn)
    .await?;
    let selected: std::collections::BTreeSet<String> =
        tables.iter().map(|t| t.qualified()).collect();
    let mut indegree: BTreeMap<String, usize> = selected.iter().map(|t| (t.clone(), 0)).collect();
    let mut edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (child, parent) in &fks {
        if child == parent || !selected.contains(child) || !selected.contains(parent) {
            continue;
        }
        edges.entry(parent.clone()).or_default().push(child.clone());
        *indegree.get_mut(child).expect("selected table") += 1;
    }
    let mut queue: std::collections::VecDeque<String> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(t, _)| t.clone())
        .collect();
    let mut ordered = Vec::with_capacity(selected.len());
    while let Some(parent) = queue.pop_front() {
        ordered.push(parent.clone());
        for child in edges.get(&parent).cloned().unwrap_or_default() {
            let degree = indegree.get_mut(&child).expect("known node");
            *degree -= 1;
            if *degree == 0 {
                queue.push_back(child);
            }
        }
    }
    if ordered.len() != selected.len() {
        anyhow::bail!("foreign-key cycle across tenant tables; the purge needs a manual ordering");
    }
    let mut ordered_tables = Vec::with_capacity(ordered.len());
    for qualified in ordered {
        let (schema, name) = qualified.split_once('.').expect("qualified");
        let pk: Vec<String> = sqlx::query_scalar(
            "SELECT a.attname
             FROM pg_index i
             JOIN LATERAL unnest(i.indkey) WITH ORDINALITY AS k(attnum, ord) ON true
             JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
             WHERE i.indrelid = format('%I.%I', $1, $2)::regclass AND i.indisprimary
             ORDER BY k.ord",
        )
        .bind(schema)
        .bind(name)
        .fetch_all(&mut *conn)
        .await?;
        if pk.is_empty() {
            anyhow::bail!("{qualified} has no primary key; add it to the plan by hand");
        }
        ordered_tables.push(TableRef {
            schema: schema.to_string(),
            name: name.to_string(),
            pk,
        });
    }
    Ok(ordered_tables)
}

#[derive(Debug, Clone)]
struct TableRef {
    schema: String,
    name: String,
    /// Primary-key columns (ordering authority for deterministic
    /// serialization; `tenant_settings` is keyed `(tenant_id, key)`, not
    /// `id`).
    pk: Vec<String>,
}

impl TableRef {
    fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }

    fn order_by(&self) -> String {
        self.pk
            .iter()
            .map(|c| quote(c))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// The archive-action classes' tables (children before parents — the
/// same dependency order the archival sweep uses).
const RETAINED_CLASS_TABLES: &[(&str, &str)] = &[
    ("billing", "invoices"),
    ("payments", "refunds"),
    ("payments", "disputes"),
    ("payments", "payments"),
];

fn class_for_schema(schema: &str) -> &'static str {
    match schema {
        "tenancy" | "access" => "membership",
        "identity" => "identity_profile",
        "platform_admin" => "request_record",
        "billing" => "billing",
        "payments" => "transaction_record",
        "privacy" => "request_record",
        "localization" | "notifications" | "theming" => "communication",
        "domains" => "request_record",
        "advertising" => "advertising_metric",
        "media" => "media_asset",
        "audit" => "audit_log",
        _ => "request_record",
    }
}

async fn export(
    app_pool: &sqlx::PgPool,
    cli_config: &crate::config::CliConfig,
    tenant_id: i64,
    args: &OffboardArgs,
) -> anyhow::Result<()> {
    // The record: the API's POST or this command — same idempotent use
    // case, same lifecycle gate.
    let ids = cli_ids()?;
    let tx = TxManager::new(app_pool.clone(), ids.clone());
    let request_use_case = RequestOffboarding::new(
        tx.clone(),
        Arc::new(SqlxTenantRepository::new()),
        Arc::new(SqlxOffboardingRepository::new()),
        Arc::new(TenancySystemClock),
        Arc::new(TenancyIdGenerator { raw: ids.clone() }),
    );
    let record = request_use_case
        .execute(
            sanvi_kernel::TenantId::try_from(tenant_id)?,
            args.key.clone(),
            None,
        )
        .await
        .map_err(|err| anyhow::anyhow!("offboarding request: {err}"))?;

    let tables = tenant_tables(app_pool).await?;

    // --- extract, one bundle per context ---------------------------------
    struct Extracted {
        bundles: Vec<DataBundle>,
        per_table: BTreeMap<String, i64>,
    }
    let mut extracted = {
        let manager = TxManager::new(app_pool.clone(), ids.clone());
        let mut scope = manager
            .begin_unscoped("offboard_export", "cli:offboard")
            .await?;
        let conn = &mut **scope.tx_mut().as_inner();

        // The tenant row itself first.
        let tenant_row: serde_json::Value =
            sqlx::query_scalar("SELECT to_jsonb(t) FROM tenancy.tenants t WHERE id = $1")
                .bind(tenant_id)
                .fetch_one(&mut *conn)
                .await?;

        let mut bundles: Vec<DataBundle> = vec![DataBundle {
            context: "tenancy".to_string(),
            entries: vec![DataEntry {
                data_class: parse_class("membership")?,
                category: "tenant".to_string(),
                label: "tenancy.tenants".to_string(),
                value: serde_json::json!([tenant_row]),
            }],
        }];
        let mut per_table: BTreeMap<String, i64> =
            BTreeMap::from([("tenancy.tenants".to_string(), 1i64)]);

        for table in &tables {
            if table.qualified() == "tenancy.tenants" {
                continue; // the anchor row is exported first, by id
            }
            let rows: serde_json::Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                r#"SELECT COALESCE(jsonb_agg(row), '[]'::jsonb)
                   FROM (SELECT * FROM {}.{} WHERE tenant_id = $1
                         ORDER BY {}) row"#,
                quote(table.schema.as_str()),
                quote(table.name.as_str()),
                table.order_by(),
            )))
            .bind(tenant_id)
            .fetch_one(&mut *conn)
            .await?;
            let count = rows.as_array().map(|a| a.len() as i64).unwrap_or(0);
            per_table.insert(table.qualified(), count);
            if count == 0 {
                continue;
            }
            let schema = table.schema.as_str();
            if let Some(bundle) = bundles.iter_mut().find(|b| b.context == schema) {
                bundle.entries.push(DataEntry {
                    data_class: parse_class(class_for_schema(schema))?,
                    category: "table".to_string(),
                    label: table.qualified(),
                    value: rows,
                });
            } else {
                bundles.push(DataBundle {
                    context: schema.to_string(),
                    entries: vec![DataEntry {
                        data_class: parse_class(class_for_schema(schema))?,
                        category: "table".to_string(),
                        label: table.qualified(),
                        value: rows,
                    }],
                });
            }
        }

        // The tenant's audit entries: retained in the live system (the
        // chain is append-only) but exported like everything else.
        {
            let audit_rows: serde_json::Value = sqlx::query_scalar(
                r#"SELECT COALESCE(jsonb_agg(row), '[]'::jsonb)
                   FROM (SELECT * FROM audit.entries WHERE tenant_id = $1
                         ORDER BY seq) row"#,
            )
            .bind(tenant_id)
            .fetch_one(&mut *conn)
            .await?;
            let count = audit_rows.as_array().map(|a| a.len() as i64).unwrap_or(0);
            per_table.insert("audit.entries".to_string(), count);
            if count > 0 {
                bundles.push(DataBundle {
                    context: "audit".to_string(),
                    entries: vec![DataEntry {
                        data_class: parse_class("audit_log")?,
                        category: "table".to_string(),
                        label: "audit.entries".to_string(),
                        value: audit_rows,
                    }],
                });
            }
        }
        Extracted { bundles, per_table }
    };

    // The per-context assertion: every app schema is present in the
    // export, even the ones holding zero rows for this tenant (as an
    // explicit empty-context marker).
    for schema in APP_SCHEMAS {
        if !extracted.bundles.iter().any(|b| b.context == *schema) {
            extracted.bundles.push(DataBundle {
                context: schema.to_string(),
                entries: vec![],
            });
        }
    }

    // --- package + encrypt (phase-05 machinery) --------------------------
    let subject = SubjectRef {
        kind: SubjectKind::EndUser,
        tenant_id: Some(sanvi_kernel::TenantId::try_from(tenant_id)?),
        key: format!("tenant:{}", args.tenant),
        identifiers: Vec::new(),
    };
    let packager = ZipPackager::new();
    let archive = packager
        .package(&subject, extracted.bundles.clone())
        .map_err(|err| anyhow::anyhow!("packaging failed: {err}"))?;

    if args.dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "command": "offboard",
                "stage": "export",
                "dry_run": true,
                "tenant": args.tenant,
                "request_id": record.id.to_string(),
                "contexts": extracted.bundles.len(),
                "tables": extracted.per_table,
                "archive_bytes": archive.len(),
            }))?
        );
        return Ok(());
    }

    if !cli_config.privacy.encryption_enabled() {
        anyhow::bail!(
            "offboard export requires the phase-05 envelope KEK (SANVI__PRIVACY__KEK); \
             an export that cannot be decrypted is not a verified export"
        );
    }
    let kek: [u8; 32] = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        cli_config.privacy.kek.expose_secret(),
    )?
    .try_into()
    .map_err(|bytes: Vec<u8>| anyhow::anyhow!("KEK must be 32 bytes, got {}", bytes.len()))?;
    let crypto: Arc<dyn EnvelopeCrypto> = Arc::new(SqlxEnvelopeCrypto::new(
        TxManager::new(app_pool.clone(), ids.clone()),
        Arc::new(SqlxSubjectKeyRepository::new()),
        kek,
        cli_config.privacy.kek_id.clone(),
    ));

    // One-time random passphrase, printed once for the operator to
    // convey out of band — same delivery story as the DSR exports.
    let passphrase = random_passphrase();
    let sealed = crypto
        .encrypt_with_passphrase(&archive, &passphrase)
        .map_err(|err| anyhow::anyhow!("encryption failed: {err}"))?;
    let checksum = sha256_hex(&sealed);
    let export_dir = std::path::Path::new(&cli_config.privacy.export_dir).join("offboarding");
    std::fs::create_dir_all(&export_dir)?;
    let path = export_dir.join(format!(
        "{}-offboard-{}.enc",
        chrono::Utc::now().format("%Y%m%d"),
        record.id
    ));
    std::fs::write(&path, &sealed)?;

    // --- verify: the file decrypts and its manifest's checksums hold ----
    verify_export(&crypto, &path, &passphrase).await?;
    let advance = AdvanceOffboarding::new(tx.clone(), Arc::new(SqlxOffboardingRepository::new()));
    advance
        .execute(record.id, OffboardingStatus::Exporting, None, None)
        .await
        .map_err(map_tenancy)?;
    advance
        .mark_exported(record.id, path.display().to_string(), checksum.clone())
        .await
        .map_err(map_tenancy)?;

    eprintln!(
        "offboard: one-time export passphrase (convey out of band, never stored): {passphrase}"
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "command": "offboard",
            "stage": "export",
            "tenant": args.tenant,
            "request_id": record.id.to_string(),
            "path": path.display().to_string(),
            "checksum": checksum,
            "sealed_bytes": sealed.len(),
            "contexts": extracted.bundles.len(),
            "tables": extracted.per_table,
            "verified": true,
        }))?
    );
    Ok(())
}

fn parse_class(class: &str) -> anyhow::Result<sanvi_privacy::domain::types::DataClass> {
    sanvi_privacy::domain::types::DataClass::parse(class)
        .map_err(|err| anyhow::anyhow!("data class: {err}"))
}

fn random_passphrase() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// The verification the purge gate requires: read the sealed file back,
/// decrypt it, and check the manifest's per-file checksums against the
/// archive's actual contents.
async fn verify_export(
    crypto: &Arc<dyn EnvelopeCrypto>,
    path: &std::path::Path,
    passphrase: &str,
) -> anyhow::Result<()> {
    let sealed = std::fs::read(path)?;
    let archive = crypto
        .decrypt_with_passphrase(&sealed, passphrase)
        .map_err(|err| anyhow::anyhow!("export does not decrypt: {err}"))?;
    verify_zip_manifest(&archive)?;
    Ok(())
}

/// Minimal manifest verification over the packaged zip: every file the
/// manifest names is present with the checksum it claims.
fn verify_zip_manifest(archive_bytes: &[u8]) -> anyhow::Result<()> {
    let reader = std::io::Cursor::new(archive_bytes);
    let mut zip = zip::ZipArchive::new(reader)?;
    let manifest_idx = zip
        .index_for_name("manifest.json")
        .ok_or_else(|| anyhow::anyhow!("export archive has no manifest.json"))?;
    let manifest_bytes = {
        let mut entry = zip.by_index(manifest_idx)?;
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut buf)?;
        buf
    };
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
    let files = manifest["files"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("manifest has no files list"))?;
    for file in files {
        let name = file["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("manifest file entry without a name"))?;
        let claimed = file["sha256"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("manifest entry for {name} has no checksum"))?;
        let mut entry = zip
            .by_name(name)
            .with_context(|| format!("manifest names {name}, the archive lacks it"))?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes)?;
        let actual = sha256_hex(&bytes);
        if actual != claimed {
            anyhow::bail!(
                "export checksum mismatch for {name}: manifest {claimed}, actual {actual}"
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Purge
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
struct RetainedClass {
    data_class: String,
    table: String,
    rows_retained: i64,
    legal_basis: String,
    retention_end: Option<String>,
}

async fn purge(
    app_pool: &sqlx::PgPool,
    cli_config: &crate::config::CliConfig,
    tenant_id: i64,
    args: &OffboardArgs,
) -> anyhow::Result<()> {
    // Typed confirmation naming the tenant — the replacement for the
    // two-person confirmation.
    let expected = format!("offboard {}", args.tenant);
    if args.confirm.as_deref() != Some(expected.as_str()) {
        anyhow::bail!(
            "refusing to purge: --confirm must be exactly '{expected}' \
             (names the tenant; there is no undo)"
        );
    }

    let tx = TxManager::new(app_pool.clone(), cli_ids()?);
    let get = GetOffboarding::new(tx.clone(), Arc::new(SqlxOffboardingRepository::new()));
    let record = get
        .execute(sanvi_kernel::TenantId::try_from(tenant_id)?)
        .await
        .map_err(map_tenancy)?
        .ok_or_else(|| anyhow::anyhow!("no offboarding record for tenant '{}'; record one first (POST /api/v1/platform/tenants/{{id}}/offboarding or the export stage)", args.tenant))?;

    // Idempotent completion: an already-purged request answers with its
    // stored report and deletes nothing.
    if record.status == OffboardingStatus::Purged {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "command": "offboard",
                "stage": "purge",
                "tenant": args.tenant,
                "request_id": record.id.to_string(),
                "status": "purged",
                "idempotent_replay": true,
                "purge_report": record.purge_report,
            }))?
        );
        return Ok(());
    }
    if record.status != OffboardingStatus::Exported {
        anyhow::bail!(
            "offboarding request {} is {}: the purge runs only from exported",
            record.id,
            record.status.as_str()
        );
    }
    let export = record
        .export
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("exported request without evidence; refusing"))?;

    let tables = tenant_tables(app_pool).await?;
    let retained_preview = retained_preview(app_pool, tenant_id).await?;

    if args.dry_run {
        let mut plan_counts = Vec::new();
        {
            let manager = TxManager::new(app_pool.clone(), cli_ids()?);
            let mut scope = manager
                .begin_unscoped("offboard_purge_plan", "cli:offboard")
                .await?;
            let conn = &mut **scope.tx_mut().as_inner();
            for table in &tables {
                let count = if table.qualified() == "tenancy.tenants" {
                    1
                } else {
                    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                        "SELECT count(*) FROM {}.{} WHERE tenant_id = $1",
                        quote(table.schema.as_str()),
                        quote(table.name.as_str())
                    )))
                    .bind(tenant_id)
                    .fetch_one(&mut *conn)
                    .await?
                };
                plan_counts.push(TableCount {
                    table: table.qualified(),
                    rows: count,
                });
            }
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "command": "offboard",
                "stage": "purge",
                "dry_run": true,
                "tenant": args.tenant,
                "request_id": record.id.to_string(),
                "would_delete": plan_counts,
                "lawfully_retained": retained_preview,
                "note": "nothing is written by the dry-run",
            }))?
        );
        return Ok(());
    }

    // The hard precondition, re-verified at purge time: the export file
    // still exists and its whole-file checksum still matches. The record
    // saying exported is necessary, not sufficient.
    // The passphrase was printed at export time and never stored; the
    // whole-file checksum is the purge-time integrity gate (the
    // manifest-level verification ran at export time, under the
    // passphrase).
    let sealed = std::fs::read(&export.path).with_context(|| {
        format!(
            "the verified export {} is missing; refusing to purge",
            export.path
        )
    })?;
    let actual = sha256_hex(&sealed);
    if actual != export.checksum {
        anyhow::bail!(
            "the export artifact's checksum changed (recorded {}, actual {}); refusing to purge",
            export.checksum,
            actual
        );
    }

    let advance = AdvanceOffboarding::new(tx.clone(), Arc::new(SqlxOffboardingRepository::new()));
    advance
        .execute(record.id, OffboardingStatus::Purging, None, None)
        .await
        .map_err(map_tenancy)?;

    // Neighbour baseline, if asked for.
    let neighbour_before = match &args.neighbour {
        Some(slug) => {
            let id = resolve_tenant(app_pool, slug).await?;
            Some((slug.clone(), id, tenant_digest(app_pool, id).await?))
        }
        None => None,
    };

    // --- the deletes: children first, chunked, per table -----------------
    let mut removed: BTreeMap<String, i64> = BTreeMap::new();
    let mut retained: Vec<RetainedClass> = Vec::new();
    {
        let manager = TxManager::new(app_pool.clone(), cli_ids()?);
        let mut scope = manager
            .begin_unscoped("offboard_purge", "cli:offboard")
            .await?;
        let conn = &mut **scope.tx_mut().as_inner();

        // The audit guard comes off for the tenant's own chain rows only
        // if audit deletion were legal — it is not (RETAINED_ALWAYS), so
        // the guard stays armed the whole time.

        for table in tables.iter().rev() {
            let qualified = table.qualified();
            if qualified == "tenancy.tenants" {
                continue; // the anchor dies last
            }
            if let Some((_, _, reason)) = RETAINED_ALWAYS
                .iter()
                .find(|(s, t, _)| *s == table.schema && *t == table.name)
            {
                let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT count(*) FROM {}.{} WHERE tenant_id = $1",
                    quote(table.schema.as_str()),
                    quote(table.name.as_str())
                )))
                .bind(tenant_id)
                .fetch_one(&mut *conn)
                .await?;
                if count > 0 {
                    retained.push(RetainedClass {
                        data_class: "audit_log".to_string(),
                        table: qualified.clone(),
                        rows_retained: count,
                        legal_basis: reason.to_string(),
                        retention_end: None,
                    });
                }
                continue;
            }

            if let Some(class) = retained_class_of(&table.name) {
                let window_days = class_window_days(&mut *conn, class).await?;
                // Statutory window: rows past it are deleted, rows inside
                // it are lawfully retained with their end date.
                let deleted: i64 = sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DELETE FROM {}.{} WHERE tenant_id = $1 AND {} < now() - ($2::bigint * interval '1 day')",
                    quote(table.schema.as_str()),
                    quote(table.name.as_str()),
                    retained_class_key(&table.name),
                )))
                .bind(tenant_id)
                .bind(window_days)
                .execute(&mut *conn)
                .await?
                .rows_affected() as i64;
                let kept: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT count(*) FROM {}.{} WHERE tenant_id = $1",
                    quote(table.schema.as_str()),
                    quote(table.name.as_str())
                )))
                .bind(tenant_id)
                .fetch_one(&mut *conn)
                .await?;
                let end: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
                    sqlx::AssertSqlSafe(format!(
                        "SELECT max({} + ($2::bigint * interval '1 day')) FROM {}.{} WHERE tenant_id = $1",
                        retained_class_key(&table.name),
                        quote(table.schema.as_str()),
                        quote(table.name.as_str()),
                    )),
                )
                .bind(tenant_id)
                .bind(window_days)
                .fetch_optional(&mut *conn)
                .await?
                .flatten();
                if deleted > 0 {
                    removed.insert(qualified.clone(), deleted);
                }
                if kept > 0 {
                    retained.push(RetainedClass {
                        data_class: class.to_string(),
                        table: qualified.clone(),
                        rows_retained: kept,
                        legal_basis: "statutory retention window (privacy.retention_rules)"
                            .to_string(),
                        retention_end: end.map(|e| e.to_rfc3339()),
                    });
                }
                continue;
            }

            let deleted: i64 = sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {}.{} WHERE tenant_id = $1",
                quote(table.schema.as_str()),
                quote(table.name.as_str()),
            )))
            .bind(tenant_id)
            .execute(&mut *conn)
            .await?
            .rows_affected() as i64;
            if deleted > 0 {
                removed.insert(qualified.clone(), deleted);
            }
        }

        // The anchor: the tenant row becomes a tombstone, not a DELETE —
        // every tenant-keyed table references it ON DELETE CASCADE, so
        // deleting it would destroy the lawfully retained rows the
        // report just promised to keep. The tombstone strips the
        // human-facing fields and stamps the purge; it also pins the
        // slug and id against reuse (a new tenant is a new identity).
        sqlx::query(
            "UPDATE tenancy.tenants
             SET display_name = 'purged tenant',
                 suspended_reason = NULL,
                 purge_requested_at = now(),
                 updated_at = now()
             WHERE id = $1",
        )
        .bind(tenant_id)
        .execute(&mut *conn)
        .await?;
        scope.commit().await?;
    }

    // --- the tenant's cold archives --------------------------------------
    let mut archives_purged = 0i64;
    {
        let storage = offboard_storage(cli_config, app_pool)?;
        let archives = tenant_archives(app_pool, tenant_id).await?;
        let mut conn = app_pool.acquire().await?;
        for archive in archives.iter().filter(|a| a.status == "archived") {
            storage.delete(&archive.storage_path)?;
            ranges::mark_purged(&mut conn, archive.id).await?;
            archives_purged += 1;
        }
    }

    // --- caches -----------------------------------------------------------
    let cache_keys = purge_redis_caches(cli_config, &args.tenant, tenant_id).await;

    // --- the residual scan: fail loudly ----------------------------------
    let residuals = residual_scan(app_pool, tenant_id).await?;
    let unexpected: Vec<&Residual> = residuals.iter().filter(|r| !r.expected_retained).collect();
    if !unexpected.is_empty() {
        // The purge report says failed, loudly: quiet completion with
        // residuals is the failure mode that makes purges untrustworthy.
        let _ = advance
            .execute(
                record.id,
                OffboardingStatus::Failed,
                None,
                Some(format!("residual rows after purge: {unexpected:?}")),
            )
            .await;
        anyhow::bail!(
            "RESIDUAL SCAN FAILED: {} table(s) still hold rows for tenant '{}': {:?}",
            unexpected.len(),
            args.tenant,
            unexpected
                .iter()
                .map(|r| r.table.clone())
                .collect::<Vec<_>>()
        );
    }

    // --- the neighbour is byte-identical ----------------------------------
    let neighbour_result = match neighbour_before {
        Some((slug, id, before)) => {
            let after = tenant_digest(app_pool, id).await?;
            let identical = before == after;
            if !identical {
                let _ = advance
                    .execute(
                        record.id,
                        OffboardingStatus::Failed,
                        None,
                        Some(format!(
                            "neighbour tenant '{slug}' changed during the purge"
                        )),
                    )
                    .await;
                anyhow::bail!(
                    "NEIGHBOUR CHECK FAILED: tenant '{slug}' is not byte-identical after the purge"
                );
            }
            serde_json::json!({ "tenant": slug, "byte_identical": identical })
        }
        None => serde_json::Value::Null,
    };

    let purge_report = serde_json::json!({
        "tenant": args.tenant,
        "tenant_id": tenant_id.to_string(),
        "request_id": record.id.to_string(),
        "removed": removed,
        "lawfully_retained": retained,
        "retained_assertion": if retained.is_empty() {
            "no retained classes"
        } else {
            "retained classes carry reason and retention end date"
        },
        "archives_purged": archives_purged,
        "cache_keys_deleted": cache_keys,
        "backup_policy_note": format!(
            "database and WAL backups expire on their existing schedule ({} days); \
             backup media is outside the purge's reach by design",
            cli_config.privacy.backup_retention_days
        ),
        "object_storage_note":
            "media bytes live in hitox-media-service; the offboarding runbook's media-service \
             purge step (owner-reference deletion) is executed there, not here",
        "search_indexes_note":
            "no search infrastructure beyond Postgres full-text: the indexes died with the rows",
        "neighbour": neighbour_result,
        "residual_scan": "clean",
    });

    advance
        .execute(
            record.id,
            OffboardingStatus::Purged,
            Some(purge_report.clone()),
            None,
        )
        .await
        .map_err(map_tenancy)?;

    println!("{}", serde_json::to_string_pretty(&purge_report)?);
    Ok(())
}

/// Which tables hold statutory-window classes. The window itself comes
/// from the live registry (`class_window_days`) — the same single source
/// of truth the sweeps read.
fn retained_class_of(table: &str) -> Option<&'static str> {
    match table {
        "invoices" => Some("invoice"),
        "refunds" | "disputes" | "payments" => Some("transaction_record"),
        _ => None,
    }
}

fn retained_class_key(table: &str) -> &'static str {
    match table {
        "invoices" | "refunds" | "disputes" | "payments" => "created_at",
        _ => "created_at",
    }
}

/// The statutory window (days) for a retained class, from the live
/// registry — the same single source of truth the sweeps read.
async fn class_window_days(conn: &mut sqlx::PgConnection, class: &str) -> anyhow::Result<i64> {
    let days: Option<i64> = sqlx::query_scalar(
        "SELECT (EXTRACT(EPOCH FROM period) / 86400)::bigint
         FROM privacy.retention_rules WHERE data_class = $1",
    )
    .bind(class)
    .fetch_optional(conn)
    .await?;
    days.ok_or_else(|| anyhow::anyhow!("retention rule for {class} disappeared mid-purge"))
}

#[derive(Debug, Clone, serde::Serialize)]
struct Residual {
    table: String,
    rows: i64,
    expected_retained: bool,
    reason: Option<String>,
}

/// The residual scan: every tenant-keyed table **and every partition
/// child by name** (pg_inherits walk — a residual hidden in a child the
/// parent's pruning misses is still found), plus the tenant's cold
/// archives. Expected survivors — the audit chain, the offboarding
/// evidence, statutory-window rows — are marked as such; anything else
/// is an unexpected residual.
async fn residual_scan(app_pool: &sqlx::PgPool, tenant_id: i64) -> anyhow::Result<Vec<Residual>> {
    let mut residuals = Vec::new();
    let mut conn = app_pool.acquire().await?;

    // Every base table with a tenant_id column (parents included).
    let tables: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.table_schema, c.table_name
         FROM information_schema.columns c
         JOIN information_schema.tables t
           ON t.table_schema = c.table_schema AND t.table_name = c.table_name
          AND t.table_type = 'BASE TABLE'
         WHERE c.column_name = 'tenant_id'
           AND c.table_schema NOT IN ('pg_catalog', 'information_schema')
         ORDER BY 1, 2",
    )
    .fetch_all(&mut *conn)
    .await?;

    for (schema, table) in &tables {
        let count: i64 = count_platform_scope(&mut conn, schema, table, tenant_id).await?;
        if count == 0 {
            continue;
        }
        let (expected, reason) = residual_expectation(schema, table);
        residuals.push(Residual {
            table: format!("{schema}.{table}"),
            rows: count,
            expected_retained: expected,
            reason: reason.map(str::to_string),
        });
    }

    // Partition children by name: the scan names every child holding
    // the tenant's rows, even when the parent's count already covers it.
    // Partitioned *indexes* also inherit (relkind 'I') — filtered out:
    // only table parents ('p') have children worth counting.
    let children: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT pn.nspname, pc.relname, cc.relname
         FROM pg_inherits i
         JOIN pg_class pc ON pc.oid = i.inhparent
         JOIN pg_namespace pn ON pn.oid = pc.relnamespace
         JOIN pg_class cc ON cc.oid = i.inhrelid
         WHERE pc.relkind = 'p' AND cc.relkind = 'r'
           AND EXISTS (
             SELECT 1 FROM pg_attribute a
             WHERE a.attrelid = i.inhparent AND a.attname = 'tenant_id' AND a.attnum > 0)
           AND pn.nspname NOT LIKE 'pg\\_%'
           AND pn.nspname NOT IN ('information_schema', 'public')
         ORDER BY 1, 2, 3",
    )
    .fetch_all(&mut *conn)
    .await?;
    for (schema, parent, child) in &children {
        let count: i64 = count_platform_scope(&mut conn, schema, child, tenant_id).await?;
        if count == 0 {
            continue;
        }
        let (expected, reason) = residual_expectation(schema, parent);
        residuals.push(Residual {
            table: format!("{schema}.{child} (child of {schema}.{parent})"),
            rows: count,
            expected_retained: expected,
            reason: reason.map(str::to_string),
        });
    }

    // The tenant's cold archives: purged/restored rows are evidence;
    // an active archive still holding the tenant's rows is unexpected.
    let archives: Vec<(String, i64)> = sqlx::query_as(
        "SELECT status, count(*) FROM platform.archives
         WHERE tenant_id = $1 GROUP BY status",
    )
    .bind(tenant_id)
    .fetch_all(&mut *conn)
    .await?;
    for (status, count) in archives {
        residuals.push(Residual {
            table: format!("platform.archives[{status}]"),
            rows: count,
            expected_retained: status != "archived",
            reason: Some(format!("cold-storage registry rows in status {status}")),
        });
    }

    Ok(residuals)
}

async fn count_platform_scope(
    conn: &mut sqlx::PgConnection,
    schema: &str,
    table: &str,
    tenant_id: i64,
) -> anyhow::Result<i64> {
    let mut tx = conn.begin().await?;
    sqlx::query("SELECT set_config('sanvi.platform_scope', 'on', true)")
        .execute(&mut *tx)
        .await?;
    let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM {}.{} WHERE tenant_id = $1",
        quote(schema),
        quote(table)
    )))
    .bind(tenant_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(count)
}

fn residual_expectation(schema: &str, table: &str) -> (bool, Option<&'static str>) {
    // Monthly partition children carry a `_YYYYMM` suffix; their
    // expectation is their parent's.
    let base = match table.rsplit_once('_') {
        Some((base, suffix)) if suffix.len() == 6 && suffix.bytes().all(|b| b.is_ascii_digit()) => {
            base
        }
        _ => table,
    };
    if let Some((_, _, reason)) = RETAINED_ALWAYS
        .iter()
        .find(|(s, t, _)| *s == schema && *t == base)
    {
        return (true, Some(reason));
    }
    if retained_class_of(base).is_some() {
        return (
            true,
            Some("statutory retention window rows are expected until the window elapses"),
        );
    }
    (false, None)
}

async fn scan(app_pool: &sqlx::PgPool, args: &OffboardArgs, tenant_id: i64) -> anyhow::Result<()> {
    let residuals = residual_scan(app_pool, tenant_id).await?;
    let unexpected: Vec<&Residual> = residuals.iter().filter(|r| !r.expected_retained).collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "command": "offboard",
            "stage": "scan",
            "tenant": args.tenant,
            "residuals": residuals,
            "clean": unexpected.is_empty(),
        }))?
    );
    if !unexpected.is_empty() {
        anyhow::bail!(
            "RESIDUAL SCAN FAILED for tenant '{}': {} unexpected residual(s): {:?}",
            args.tenant,
            unexpected.len(),
            unexpected
                .iter()
                .map(|r| r.table.clone())
                .collect::<Vec<_>>()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn map_tenancy(err: TenancyError) -> anyhow::Error {
    anyhow::anyhow!("{err}")
}

fn cli_ids() -> anyhow::Result<Arc<dyn RawIdGenerator>> {
    Ok(Arc::new(sanvi_kernel::SnowflakeGenerator::new(
        sanvi_kernel::NodeId::new(25)?,
    )))
}

/// The tenancy IdGenerator over the CLI's raw generator.
struct TenancyIdGenerator {
    raw: Arc<dyn RawIdGenerator>,
}

impl sanvi_tenancy::domain::IdGenerator for TenancyIdGenerator {
    fn next_tenant_id(&self) -> Result<sanvi_tenancy::domain::TenantId, TenancyError> {
        let raw = self
            .raw
            .next_id()
            .map_err(|err| TenancyError::Storage(format!("tenant id generation: {err}")))?;
        sanvi_tenancy::domain::TenantId::try_from(raw.as_i64())
            .map_err(|err| TenancyError::Storage(format!("tenant id generation: {err}")))
    }
}

async fn tenant_archives(
    pool: &sqlx::PgPool,
    tenant_id: i64,
) -> anyhow::Result<Vec<ranges::ArchiveRecord>> {
    let mut conn = pool.acquire().await?;
    Ok(ranges::active_for_tenant(&mut conn, tenant_id).await?)
}

/// The retained-class preview (plan + dry-run): rows inside their
/// statutory windows, with the window from the live registry.
async fn retained_preview(
    pool: &sqlx::PgPool,
    tenant_id: i64,
) -> anyhow::Result<Vec<RetainedClass>> {
    let mut conn = pool.acquire().await?;
    let mut out = Vec::new();
    for (schema, table) in RETAINED_CLASS_TABLES {
        let Some(class) = retained_class_of(table) else {
            continue;
        };
        let days = class_window_days(&mut conn, class).await?;
        let kept: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {}.{} WHERE tenant_id = $1 AND {} >= now() - ($2::bigint * interval '1 day')",
            quote(schema),
            quote(table),
            retained_class_key(table),
        )))
        .bind(tenant_id)
        .bind(days)
        .fetch_one(&mut *conn)
        .await?;
        if kept > 0 {
            let end: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
                sqlx::AssertSqlSafe(format!(
                    "SELECT max({} + ($2::bigint * interval '1 day')) FROM {}.{} WHERE tenant_id = $1",
                    retained_class_key(table),
                    quote(schema),
                    quote(table),
                )),
            )
            .bind(tenant_id)
            .bind(days)
            .fetch_optional(&mut *conn)
            .await?
            .flatten();
            out.push(RetainedClass {
                data_class: class.to_string(),
                table: format!("{schema}.{table}"),
                rows_retained: kept,
                legal_basis: "statutory retention window (privacy.retention_rules)".to_string(),
                retention_end: end.map(|e| e.to_rfc3339()),
            });
        }
    }
    Ok(out)
}

/// A tenant's content digest: sha256 over every tenant table's canonical
/// row serialization — the byte-identical neighbour proof.
async fn tenant_digest(pool: &sqlx::PgPool, tenant_id: i64) -> anyhow::Result<String> {
    let tables = tenant_tables(pool).await?;
    let manager = TxManager::new(pool.clone(), cli_ids()?);
    let mut scope = manager
        .begin_unscoped("offboard_digest", "cli:offboard")
        .await?;
    let conn = &mut **scope.tx_mut().as_inner();
    let mut digest = String::new();
    for table in &tables {
        let rows: serde_json::Value = if table.qualified() == "tenancy.tenants" {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                r#"SELECT COALESCE(jsonb_agg(row), '[]'::jsonb)
                   FROM (SELECT * FROM {}.{} WHERE id = $1 ORDER BY {}) row"#,
                quote(table.schema.as_str()),
                quote(table.name.as_str()),
                table.order_by(),
            )))
            .bind(tenant_id)
            .fetch_one(&mut *conn)
            .await?
        } else {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                r#"SELECT COALESCE(jsonb_agg(row), '[]'::jsonb)
                   FROM (SELECT * FROM {}.{} WHERE tenant_id = $1 ORDER BY {}) row"#,
                quote(table.schema.as_str()),
                quote(table.name.as_str()),
                table.order_by(),
            )))
            .bind(tenant_id)
            .fetch_one(&mut *conn)
            .await?
        };
        digest.push_str(&table.qualified());
        digest.push(':');
        digest.push_str(&sha256_hex(serde_json::to_string(&rows)?.as_bytes()));
        digest.push('\n');
    }
    Ok(sha256_hex(digest.as_bytes()))
}

/// Best-effort Redis sweep of the tenant's cache keys. The URL comes
/// from a plain environment variable (not the figment tree — the CLI's
/// config slice has no redis section).
async fn purge_redis_caches(
    cli_config: &crate::config::CliConfig,
    slug: &str,
    tenant_id: i64,
) -> i64 {
    let _ = cli_config;
    let url = std::env::var("SANVI_OFFBOARD_REDIS_URL")
        .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let client = match redis::Client::open(url) {
        Ok(client) => client,
        Err(_) => return 0,
    };
    let mut conn = match client.get_connection() {
        Ok(conn) => conn,
        Err(_) => return 0,
    };
    let patterns = [format!("*{slug}*"), format!("*{tenant_id}*")];
    let mut deleted = 0i64;
    for pattern in patterns {
        let mut cursor: u64 = 0;
        loop {
            let (next, keys): (u64, Vec<String>) = match redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(500)
                .query(&mut conn)
            {
                Ok(result) => result,
                Err(_) => return deleted,
            };
            if !keys.is_empty() {
                let _: Result<i64, _> = redis::cmd("DEL").arg(&keys).query(&mut conn);
                deleted += keys.len() as i64;
            }
            cursor = next;
            if cursor == 0 {
                break;
            }
        }
    }
    deleted
}

fn offboard_storage(
    cli_config: &crate::config::CliConfig,
    pool: &sqlx::PgPool,
) -> anyhow::Result<Arc<SealedStorage>> {
    let kek: [u8; 32] = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        cli_config.privacy.kek.expose_secret(),
    )?
    .try_into()
    .map_err(|bytes: Vec<u8>| anyhow::anyhow!("KEK must be 32 bytes, got {}", bytes.len()))?;
    let crypto: Arc<dyn EnvelopeCrypto> = Arc::new(SqlxEnvelopeCrypto::new(
        TxManager::new(pool.clone(), cli_ids()?),
        Arc::new(SqlxSubjectKeyRepository::new()),
        kek,
        cli_config.privacy.kek_id.clone(),
    ));
    Ok(Arc::new(SealedStorage::new(
        Arc::new(LocalEncryptedVolume::new(&cli_config.archival.volume)),
        Arc::new(EnvelopeArchiveCipher::new(crypto, &kek)),
    )))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}
