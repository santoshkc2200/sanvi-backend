//! TASK-028 (FR-1128): the partition registry gate — `just partition-check`.
//!
//! Phase 10's rls_matrix proved *RLS* over partitions by name; this suite
//! proves the registry itself: every parent enrolled in
//! `platform.partitioned_tables` is actually partitioned, its key column
//! exists, its current and following month's children exist, and each new
//! child carries the per-child hardening (RLS for tenant-keyed parents,
//! the app role's grants for every parent).
//!
//! The alert half of FR-1128 is asserted here too, against the
//! catalog-only probe `platform.partition_readiness_breaches`: a parent
//! without next month's child is a breach from the 25th on — evaluated
//! against a reference date, so the test does not depend on today being
//! late in the month. The probe reading the catalog (not the job's
//! heartbeat) is what makes "disabling the job fires the alert" true: the
//! job being gone is indistinguishable, to the probe, from the job having
//! failed.

use sqlx::Row;

const EXPECTED_PARENTS: &[(&str, &str, &str)] = &[
    ("advertising", "conversion_events", "occurred_at"),
    ("advertising", "metrics_daily", "date"),
    ("audit", "entries", "occurred_at"),
    ("platform", "outbox", "occurred_at"),
    ("billing", "webhook_events", "received_at"),
    ("payments", "webhook_events", "received_at"),
];

#[derive(Debug, sqlx::FromRow)]
struct RegistryRow {
    schema_name: String,
    table_name: String,
    key_column: String,
    create_ahead_months: i32,
}

async fn registry(pool: &sqlx::PgPool) -> Vec<RegistryRow> {
    sqlx::query_as::<_, RegistryRow>(
        "SELECT schema_name, table_name, key_column, create_ahead_months
         FROM platform.partitioned_tables ORDER BY schema_name, table_name",
    )
    .fetch_all(pool)
    .await
    .expect("registry query")
}

#[tokio::test]
async fn every_registry_parent_is_partitioned_on_its_key_with_children() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let rows = registry(&pool).await;

    let enrolled: Vec<(String, String, String)> = rows
        .iter()
        .map(|r| {
            (
                r.schema_name.clone(),
                r.table_name.clone(),
                r.key_column.clone(),
            )
        })
        .collect();
    for (schema, table, key) in EXPECTED_PARENTS {
        assert!(
            enrolled
                .iter()
                .any(|(s, t, k)| s == schema && t == table && k == key),
            "expected {schema}.{table} (key {key}) enrolled in platform.partitioned_tables; \
             found {enrolled:?}"
        );
    }

    for row in &rows {
        let qualified = format!("{}.{}", row.schema_name, row.table_name);

        // Partitioned, and on the registered key column.
        let parent = sqlx::query(
            "SELECT c.relkind::text AS relkind, att.attnum
             FROM pg_class c
             JOIN pg_namespace n ON n.oid = c.relnamespace
             JOIN pg_attribute att ON att.attrelid = c.oid AND att.attname = $3
             WHERE n.nspname = $1 AND c.relname = $2",
        )
        .bind(&row.schema_name)
        .bind(&row.table_name)
        .bind(&row.key_column)
        .fetch_optional(&pool)
        .await
        .expect("catalog query")
        .map(|row: sqlx::postgres::PgRow| {
            let relkind: String = row.get("relkind");
            let attnum: i16 = row.get("attnum");
            (relkind, attnum)
        });
        let Some((relkind, _)) = parent else {
            panic!(
                "{qualified}: enrolled but missing or keyless (key {})",
                row.key_column
            );
        };
        assert_eq!(relkind, "p", "{qualified}: enrolled but not partitioned");

        // Current and following month exist for every parent.
        let coverage: Vec<(String, bool)> = sqlx::query(
            "SELECT to_char(m.month, 'YYYYMM'),
                    to_regclass(format('%I.%I_%s', r.schema_name, r.table_name,
                                       to_char(m.month, 'YYYYMM'))) IS NOT NULL
             FROM platform.partitioned_tables r
             CROSS JOIN LATERAL (
               SELECT generate_series(
                        date_trunc('month', current_date),
                        date_trunc('month', current_date)
                          + make_interval(months => r.create_ahead_months::int),
                        interval '1 month') AS month
             ) m
             WHERE r.schema_name = $1 AND r.table_name = $2
             ORDER BY 1",
        )
        .bind(&row.schema_name)
        .bind(&row.table_name)
        .map(|row: sqlx::postgres::PgRow| {
            let suffix: String = row.get(0);
            let exists: bool = row.get(1);
            (suffix, exists)
        })
        .fetch_all(&pool)
        .await
        .expect("coverage query");
        assert!(
            coverage.len() as i64 > i64::from(row.create_ahead_months),
            "{qualified}: coverage query returned {} months for ahead={}",
            coverage.len(),
            row.create_ahead_months
        );
        for (suffix, exists) in &coverage {
            let child = format!("{}.{}_{}", row.schema_name, row.table_name, suffix);
            assert!(
                *exists,
                "{child}: missing — the maintenance job or the conversion migration \
                 did not create it"
            );
        }

        // Per-child hardening: tenant-keyed parents' children are RLS-forced
        // with the parent's tenant_isolation policy; every child carries the
        // app role's grants.
        let tenant_keyed: bool = sqlx::query_scalar(
            "SELECT EXISTS (
               SELECT 1 FROM pg_attribute a
               JOIN pg_class c ON c.oid = a.attrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
               WHERE n.nspname = $1 AND c.relname = $2
                 AND a.attname = 'tenant_id' AND a.attnum > 0)",
        )
        .bind(&row.schema_name)
        .bind(&row.table_name)
        .fetch_one(&pool)
        .await
        .expect("tenant-keyed probe");

        for (suffix, _) in &coverage {
            let child = format!("{}.{}_{}", row.schema_name, row.table_name, suffix);

            if tenant_keyed {
                let (rls, force, policies): (bool, bool, i64) = sqlx::query_as(
                    "SELECT c.relrowsecurity, c.relforcerowsecurity,
                            (SELECT count(*) FROM pg_policies p
                             WHERE p.schemaname = $1 AND p.tablename = $2
                               AND p.policyname = 'tenant_isolation')
                     FROM pg_class c
                     JOIN pg_namespace n ON n.oid = c.relnamespace
                     WHERE n.nspname = $1 AND c.relname = $2",
                )
                .bind(&row.schema_name)
                .bind(format!("{}_{}", row.table_name, suffix))
                .fetch_one(&pool)
                .await
                .unwrap_or_else(|_| panic!("{child}: not found in catalog"));
                assert!(rls && force, "{child}: child missing ENABLE/FORCE RLS");
                assert_eq!(
                    policies, 1,
                    "{child}: expected exactly one tenant_isolation policy"
                );
            }

            let grants: i64 = sqlx::query_scalar(
                "SELECT count(DISTINCT privilege_type)
                 FROM information_schema.role_table_grants
                 WHERE table_schema = $1 AND table_name = $2 AND grantee = 'sanvi_app'",
            )
            .bind(&row.schema_name)
            .bind(format!("{}_{}", row.table_name, suffix))
            .fetch_one(&pool)
            .await
            .expect("grant query");
            assert!(
                grants >= 2,
                "{child}: the app role holds only {grants} privileges — the registry's \
                 grant copy did not run for this child"
            );
        }
    }
}

/// The by-25th alert predicate, evaluated on a controlled reference date:
/// a missing next-month child is a breach from the 25th on and not before,
/// and the probe recovers to quiet once the child exists again. This is
/// the "disabling the job fires the alert" assertion — the probe reads
/// the catalog, so a job that never ran is exactly a missing child.
#[tokio::test]
async fn readiness_probe_fires_for_missing_next_month_from_the_25th() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.migrator_pool().await;

    // The probe's reference date is parameterised; late-month = the 26th.
    let late = "2026-10-26".parse::<chrono::NaiveDate>().unwrap();
    let early = "2026-10-24".parse::<chrono::NaiveDate>().unwrap();

    let breaches_on = |date: chrono::NaiveDate, pool: &sqlx::PgPool| {
        let pool = pool.clone();
        async move {
            let rows: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT schema_name, table_name, missing_child
                 FROM platform.partition_readiness_breaches($1)",
            )
            .bind(date)
            .fetch_all(&pool)
            .await
            .expect("probe query");
            rows
        }
    };

    // The migrations pre-created 202610 and 202611 — but the probe's months
    // are relative to the *reference date*, so first align the registry's
    // horizon with it, then simulate a disabled job by dropping the child.
    sqlx::query("SELECT platform.ensure_monthly_partitions('2026-10-10')")
        .execute(&pool)
        .await
        .expect("ensure");
    sqlx::query(
        "DO $$
         DECLARE m date;
         BEGIN
           FOR m IN SELECT generate_series(date_trunc('month', '2026-10-26'::date),
                                           date_trunc('month', '2026-10-26'::date)
                                             + interval '1 month',
                                           interval '1 month')::date LOOP
             PERFORM platform.ensure_monthly_partitions(m);
           END LOOP;
         END $$",
    )
    .execute(&pool)
    .await
    .expect("ensure horizon");
    assert!(
        breaches_on(late, &pool).await.is_empty(),
        "probe must be quiet while every next-month child exists"
    );

    // A disabled job: October passes, nobody creates November's audit child.
    sqlx::query("DROP TABLE IF EXISTS audit.entries_202611")
        .execute(&pool)
        .await
        .expect("drop child");

    let breaches = breaches_on(late, &pool).await;
    assert!(
        breaches
            .iter()
            .any(|(s, t, c)| s == "audit" && t == "entries" && c == "entries_202611"),
        "probe must name the missing child by name; got {breaches:?}"
    );

    // Before the 25th the same gap is silent — the deadline is part of the
    // contract, not an always-on check.
    assert!(
        breaches_on(early, &pool).await.is_empty(),
        "the same missing child must not page before the 25th (FR-1128 deadline)"
    );

    // The repair path: one ensure call clears the breach.
    sqlx::query("SELECT platform.ensure_monthly_partitions('2026-10-10')")
        .execute(&pool)
        .await
        .expect("ensure");
    assert!(breaches_on(late, &pool).await.is_empty());
}

/// The job's heartbeat table: one row per day, last write wins, readable by
/// the app role (the runbook's "did the job run at all" check).
#[tokio::test]
async fn maintenance_heartbeat_is_idempotent_per_day() {
    let db = sanvi_testsupport::test_db().await;
    let app = db.app_pool();

    sqlx::query(
        "INSERT INTO platform.partition_maintenance_log (day, ran_at, ok)
         VALUES (current_date, now(), true)
         ON CONFLICT (day) DO UPDATE SET ran_at = excluded.ran_at, ok = true",
    )
    .execute(&app)
    .await
    .expect("app role writes the heartbeat");

    sqlx::query(
        "INSERT INTO platform.partition_maintenance_log (day, ran_at, ok, detail)
         VALUES (current_date, now(), false, 'simulated failure')
         ON CONFLICT (day) DO UPDATE SET ran_at = excluded.ran_at, ok = false,
                                         detail = excluded.detail",
    )
    .execute(&app)
    .await
    .expect("app role updates the heartbeat");

    let (ok, detail): (bool, Option<String>) = sqlx::query_as(
        "SELECT ok, detail FROM platform.partition_maintenance_log WHERE day = current_date",
    )
    .fetch_one(&app)
    .await
    .expect("heartbeat row");
    assert!(!ok);
    assert_eq!(detail.as_deref(), Some("simulated failure"));
}
