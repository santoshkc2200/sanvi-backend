//! TASK-028 (FR-1131) tests for `offboard`: the export-then-purge
//! contract end to end, against the real binary.
//!
//! The gate list from the task: `--dry-run` reports without mutating;
//! the export contains every context's data (asserted per context); the
//! purge is unreachable before the export verifies; the purge removes
//! the data, the residual scan finds nothing unexpected (partitions and
//! archives by name), and a neighbour tenant is byte-identical; the
//! purge report separates removed from lawfully retained, and a tenant
//! with completed payments cannot produce an empty retained list;
//! offboarding is idempotent (a replay answers the stored report).

use std::process::Command;

/// Fixed ids in this suite's own range (910_291_xxx).
const TENANT_OFFBOARDED: i64 = 910_291_001;
const TENANT_NEIGHBOUR: i64 = 910_291_002;
const PAYMENT_ID: i64 = 910_291_101;
const NEIGHBOUR_PAYMENT_ID: i64 = 910_291_102;

struct CliOutcome {
    code: i32,
    stdout: String,
    stderr: String,
}

async fn run_cli(db: &sanvi_testsupport::TestDb, database: &str, args: &[&str]) -> CliOutcome {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("apps/cli sits two levels below the repository root")
        .to_path_buf();

    let output = Command::new(env!("CARGO_BIN_EXE_sanvi-cli"))
        .args(["--env", "local", "offboard"])
        .args(args)
        .env("SANVI__DATABASE__HOST", "localhost")
        .env("SANVI__DATABASE__PORT", db.port().to_string())
        .env("SANVI__DATABASE__NAME", database)
        .env("SANVI__DATABASE__USER", "sanvi_app")
        .env("SANVI__DATABASE__PASSWORD", "app")
        // A fixed, valid KEK: the export must seal and the purge must
        // re-verify under it.
        .env(
            "SANVI__PRIVACY__KEK",
            "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        )
        .env("SANVI__PRIVACY__EXPORT_DIR", std::env::temp_dir().join("sanvi-offboard-test").display().to_string())
        .env(
            "SANVI_OFFBOARD_REDIS_URL",
            format!(
                "redis://localhost:{}",
                std::env::var("SANVI_TEST_REDIS_PORT").unwrap_or_else(|_| "6379".to_string())
            ),
        )
        .current_dir(&repo_root)
        .output()
        .expect("the offboard binary spawns");

    CliOutcome {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

async fn pool(db: &sanvi_testsupport::TestDb, database: &str) -> sqlx::PgPool {
    sqlx::PgPool::connect(&format!(
        "postgres://postgres:postgres@localhost:{}/{}",
        db.port(),
        database
    ))
    .await
    .expect("owner pool")
}

async fn seed(db: &sanvi_testsupport::TestDb, database: &str) {
    let pool = pool(db, database).await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('sanvi.platform_scope', 'on', true)")
        .execute(&mut *tx)
        .await
        .unwrap();

    for (id, slug, status) in [
        (TENANT_OFFBOARDED, "offboard-me", "archived"),
        (TENANT_NEIGHBOUR, "offboard-neighbour", "active"),
    ] {
        sqlx::query(
            "INSERT INTO tenancy.tenants (id, slug, display_name, status, region, archived_at)
             VALUES ($1, $2, $3, $4, 'jp', CASE WHEN $4 = 'archived' THEN now() ELSE NULL END)",
        )
        .bind(id)
        .bind(slug)
        .bind(slug)
        .bind(status)
        .execute(&mut *tx)
        .await
        .unwrap();
    }

    // A completed payment well inside its 3650-day statutory window for
    // each tenant: the offboarded tenant's row must be *retained* (with
    // reason and retention end date), the neighbour's must survive
    // byte-identically.
    for (id, tenant) in [
        (PAYMENT_ID, TENANT_OFFBOARDED),
        (NEIGHBOUR_PAYMENT_ID, TENANT_NEIGHBOUR),
    ] {
        sqlx::query(
            "INSERT INTO payments.payments
               (id, tenant_id, external_payment_id, status, amount_minor, currency, created_at)
             VALUES ($1, $2, $3, 'succeeded', 4200, 'JPY', now() - interval '30 days')",
        )
        .bind(id)
        .bind(tenant)
        .bind(format!("pi_offboard_{id}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    }

    // Rows in ordinary contexts: settings (keyed (tenant_id, key), no id
    // column — exercises the pk-driven ordering) and audit entries for
    // the offboarded tenant (retained by construction).
    for tenant in [TENANT_OFFBOARDED, TENANT_NEIGHBOUR] {
        sqlx::query(
            "INSERT INTO tenancy.tenant_settings (tenant_id, key, value)
             VALUES ($1, 'offboard-probe', '\"present\"'::jsonb)",
        )
        .bind(tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO audit.entries
           (id, occurred_at, actor_type, tenant_id, action, prev_hash, hash)
         VALUES ($1, now(), 'system', $2, 'tenant.offboard-test', $3, $4)",
    )
    .bind(910_291_201)
    .bind(TENANT_OFFBOARDED)
    .bind([0u8; 32])
    .bind([1u8; 32])
    .execute(&mut *tx)
    .await
    .unwrap();

    tx.commit().await.unwrap();
}

#[tokio::test]
async fn offboarding_is_export_then_purge_with_residual_scan_and_neighbour_proof() {
    let db = sanvi_testsupport::test_db().await;
    let database = db.name().to_string();
    seed(&db, &database).await;

    // --- 1. dry-run: the plan reports and writes nothing -----------------
    let plan = run_cli(
        &db,
        &database,
        &[
            "--tenant",
            "offboard-me",
            "--key",
            "test-key-1",
            "--dry-run",
        ],
    )
    .await;
    assert_eq!(
        plan.code, 0,
        "plan stdout: {}\nstderr: {}",
        plan.stdout, plan.stderr
    );
    let plan_json: serde_json::Value = serde_json::from_str(&find_json(&plan.stdout)).unwrap();
    assert_eq!(plan_json["stage"], "plan");
    {
        let pool = pool(&db, &database).await;
        let tenants: i64 =
            sqlx::query_scalar("SELECT count(*) FROM tenancy.tenants WHERE slug = 'offboard-me'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(tenants, 1, "the dry-run mutated nothing");
    }

    // --- 2. purge before export: unreachable ------------------------------
    let premature = run_cli(
        &db,
        &database,
        &[
            "--tenant",
            "offboard-me",
            "--key",
            "test-key-1",
            "--stage",
            "purge",
            "--confirm",
            "offboard offboard-me",
        ],
    )
    .await;
    assert_ne!(premature.code, 0, "purge before export must refuse");
    assert!(
        premature.stderr.contains("export"),
        "the refusal must point at the export precondition: {}",
        premature.stderr
    );

    // --- 3. export: every context's data, asserted per context ------------
    let export = run_cli(
        &db,
        &database,
        &[
            "--tenant",
            "offboard-me",
            "--key",
            "test-key-1",
            "--stage",
            "export",
        ],
    )
    .await;
    assert_eq!(export.code, 0, "export stdout: {}", export.stdout);
    let export_json: serde_json::Value = serde_json::from_str(&find_json(&export.stdout)).unwrap();
    assert_eq!(export_json["verified"], true);
    assert!(
        export_json["path"].as_str().unwrap().ends_with(".enc"),
        "the export is an encrypted artifact"
    );
    assert!(
        export.stderr.contains("passphrase"),
        "the one-time passphrase is printed for the operator"
    );
    let tables = export_json["tables"].as_object().unwrap();
    assert_eq!(tables["payments.payments"], 1, "the payment exported");
    assert_eq!(
        tables["tenancy.tenant_settings"], 1,
        "keyed tables export too"
    );
    assert_eq!(tables["audit.entries"], 1, "the audit rows exported");
    // Every app context is present in the archive's data.json.
    {
        let zip_bytes = open_sealed_export(export_json["path"].as_str().unwrap(), &export.stderr);
        let data = read_zip_member(&zip_bytes, "data.json");
        let contexts: Vec<&str> = data["contexts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["context"].as_str().unwrap())
            .collect();
        for expected in [
            "tenancy",
            "identity",
            "access",
            "platform_admin",
            "billing",
            "payments",
            "privacy",
            "localization",
            "theming",
            "domains",
            "advertising",
            "media",
            "notifications",
        ] {
            assert!(
                contexts.contains(&expected),
                "context {expected} missing from the export: {contexts:?}"
            );
        }
        let payment_entry = data["contexts"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|b| b["entries"].as_array().cloned().unwrap_or_default())
            .find(|e| e["label"] == "payments.payments")
            .expect("the payment table is an entry");
        assert_eq!(
            payment_entry["value"].as_array().unwrap().first().unwrap()["external_payment_id"],
            format!("pi_offboard_{PAYMENT_ID}")
        );
    }

    // --- 4. purge without the typed confirmation: refused ------------------
    let unconfirmed = run_cli(
        &db,
        &database,
        &[
            "--tenant",
            "offboard-me",
            "--key",
            "test-key-1",
            "--stage",
            "purge",
        ],
    )
    .await;
    assert_ne!(unconfirmed.code, 0);
    assert!(
        unconfirmed.stderr.contains("--confirm"),
        "the refusal names the typed confirmation"
    );

    // --- 5. the purge, with the neighbour proof ----------------------------
    let purge = run_cli(
        &db,
        &database,
        &[
            "--tenant",
            "offboard-me",
            "--key",
            "test-key-1",
            "--stage",
            "purge",
            "--confirm",
            "offboard offboard-me",
            "--neighbour",
            "offboard-neighbour",
        ],
    )
    .await;
    assert_eq!(
        purge.code, 0,
        "purge stdout: {}\nstderr: {}",
        purge.stdout, purge.stderr
    );
    let report: serde_json::Value = serde_json::from_str(&find_json(&purge.stdout)).unwrap();

    // Removed: the settings and everything else the tenant owned. The
    // tenant row itself becomes a tombstone (FK anchor for the retained
    // rows), and the payment inside its statutory window is NOT removed.
    let removed = report["removed"].as_object().unwrap();
    assert!(
        !removed.contains_key("tenancy.tenants"),
        "the anchor is a tombstone, not a delete"
    );
    assert_eq!(removed["tenancy.tenant_settings"], 1);
    assert!(
        !removed.contains_key("payments.payments"),
        "the in-window payment must not be removed: {removed:?}"
    );

    // Lawfully retained: non-empty, with reason and retention end date —
    // a tenant with completed payments cannot produce an empty list.
    let retained = report["lawfully_retained"].as_array().unwrap();
    assert!(
        !retained.is_empty(),
        "a tenant with a completed payment must retain it (empty retained list is a bug)"
    );
    let payment_retained = retained
        .iter()
        .find(|r| r["table"] == "payments.payments")
        .expect("the payment's retention is reported");
    assert_eq!(payment_retained["rows_retained"], 1);
    assert!(
        payment_retained["legal_basis"]
            .as_str()
            .unwrap()
            .contains("statutory"),
        "the retained class carries its legal basis"
    );
    assert!(
        payment_retained["retention_end"].is_string(),
        "the retained class carries a retention end date"
    );
    assert_eq!(report["neighbour"]["byte_identical"], true);
    assert_eq!(report["residual_scan"], "clean");

    // --- 6. the database after: nothing unexpected, neighbour intact ------
    let pool = pool(&db, &database).await;
    let tombstone: Option<(String, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT display_name, purge_requested_at FROM tenancy.tenants WHERE id = $1",
    )
    .bind(TENANT_OFFBOARDED)
    .fetch_optional(&pool)
    .await
    .unwrap();
    let Some((name, purged_at)) = tombstone else {
        panic!("the tombstone row survives as the retained rows' FK anchor");
    };
    assert_eq!(name, "purged tenant");
    assert!(purged_at.is_some(), "the tombstone stamps the purge");
    let neighbour_payment: Option<String> =
        sqlx::query_scalar("SELECT external_payment_id FROM payments.payments WHERE id = $1")
            .bind(NEIGHBOUR_PAYMENT_ID)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(
        neighbour_payment.as_deref(),
        Some("pi_offboard_910291102"),
        "the neighbour's payment is byte-identical"
    );

    // --- 7. the residual scan, standalone: clean, by name ------------------
    // The slug is gone with the tenant; the scan runs by id — exactly
    // the post-purge shape.
    let scan = run_cli(
        &db,
        &database,
        &[
            "--tenant",
            &TENANT_OFFBOARDED.to_string(),
            "--key",
            "test-key-1",
            "--stage",
            "scan",
        ],
    )
    .await;
    assert_eq!(
        scan.code, 0,
        "scan stdout: {}\nstderr: {}",
        scan.stdout, scan.stderr
    );
    let scan_json: serde_json::Value = serde_json::from_str(&find_json(&scan.stdout)).unwrap();
    assert_eq!(scan_json["clean"], true);
    let residuals = scan_json["residuals"].as_array().unwrap();
    let names: Vec<String> = residuals
        .iter()
        .map(|r| r["table"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n.contains("audit.entries")),
        "the retained audit rows appear by name: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("payments.payments")),
        "the retained statutory rows appear by name: {names:?}"
    );

    // --- 8. idempotent replay: the same report, no failure -----------------
    let replay = run_cli(
        &db,
        &database,
        &[
            "--tenant",
            "offboard-me",
            "--key",
            "test-key-1",
            "--stage",
            "purge",
            "--confirm",
            "offboard offboard-me",
        ],
    )
    .await;
    assert_eq!(
        replay.code, 0,
        "re-running offboarding produces the same report and does not fail"
    );
    let replay_json: serde_json::Value = serde_json::from_str(&find_json(&replay.stdout)).unwrap();
    assert_eq!(replay_json["idempotent_replay"], true);
    assert!(
        replay_json["purge_report"].is_object(),
        "the stored report is answered"
    );
}

/// A residual that must FAIL loudly: a row snuck back after the purge.
#[tokio::test]
async fn the_residual_scan_fails_loudly_on_an_unexpected_survivor() {
    let db = sanvi_testsupport::test_db().await;
    let database = db.name().to_string();
    seed(&db, &database).await;

    let export = run_cli(
        &db,
        &database,
        &[
            "--tenant",
            "offboard-me",
            "--key",
            "test-key-2",
            "--stage",
            "export",
        ],
    )
    .await;
    assert_eq!(export.code, 0, "{}", export.stdout);

    let purge = run_cli(
        &db,
        &database,
        &[
            "--tenant",
            "offboard-me",
            "--key",
            "test-key-2",
            "--stage",
            "purge",
            "--confirm",
            "offboard offboard-me",
        ],
    )
    .await;
    assert_eq!(purge.code, 0, "{}\n{}", purge.stdout, purge.stderr);

    // The purge left the tombstone (expected residual); smuggle a
    // settings row back in against it — an *unexpected* residual the
    // scan must find by name and fail on.
    let pool = pool(&db, &database).await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('sanvi.platform_scope', 'on', true)")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO tenancy.tenant_settings (tenant_id, key, value)
         VALUES ($1, 'residual', '\"leftover\"'::jsonb)",
    )
    .bind(TENANT_OFFBOARDED)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let scan = run_cli(
        &db,
        &database,
        &[
            "--tenant",
            &TENANT_OFFBOARDED.to_string(),
            "--key",
            "test-key-2",
            "--stage",
            "scan",
        ],
    )
    .await;
    assert_ne!(scan.code, 0, "an unexpected residual must fail the scan");
    assert!(
        scan.stderr.contains("RESIDUAL SCAN FAILED"),
        "the failure is loud: {}",
        scan.stderr
    );
}

/// The CLI prints human lines before the JSON on stdout in some stages;
/// the JSON document is the last printable block.
fn find_json(stdout: &str) -> String {
    let start = stdout.find('{').expect("a JSON document on stdout");
    stdout[start..].trim().to_string()
}

/// Decrypt the sealed export (phase-05 passphrase format: salt ‖ nonce ‖
/// AES-256-GCM body, PBKDF2-HMAC-SHA256 600k) with the one-time
/// passphrase the command printed.
fn open_sealed_export(path: &str, stderr: &str) -> Vec<u8> {
    use aes_gcm::Aes256Gcm;
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use sha2::Sha256;

    let passphrase = stderr
        .lines()
        .find_map(|line| line.split_once("passphrase (convey out of band, never stored): "))
        .map(|(_, pass)| pass.trim().to_string())
        .expect("the one-time passphrase is on stderr");
    let sealed = std::fs::read(path).expect("the sealed export exists");
    let (salt, rest) = sealed.split_at(16);
    let (nonce, body) = rest.split_at(12);
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, 600_000, &mut key);
    let cipher = Aes256Gcm::new_from_slice(&key).expect("derived key");
    cipher
        .decrypt(nonce.into(), Payload::from(body))
        .expect("the export decrypts with the printed passphrase")
}

fn read_zip_member(zip_bytes: &[u8], member: &str) -> serde_json::Value {
    let reader = std::io::Cursor::new(zip_bytes);
    let mut zip = zip::ZipArchive::new(reader).expect("the decrypted export is a zip");
    let mut entry = zip.by_name(member).expect("data.json in the export");
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
    serde_json::from_slice(&bytes).expect("data.json parses")
}
