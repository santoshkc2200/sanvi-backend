use std::sync::atomic::{AtomicU64, Ordering};

use sanvi_persistence::{DatabaseConfig, Migrator, PgPool, build_pool};
use sqlx::AssertSqlSafe;
use testcontainers::{ImageExt, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;
use tokio::sync::OnceCell;

/// Migration owner. Matches the production split: a role that owns the schema
/// and a separate least-privilege role the application connects with.
const MIGRATOR_ROLE: &str = "sanvi_migrator";
const MIGRATOR_PASSWORD: &str = "mig";
const APP_ROLE: &str = "sanvi_app";
const APP_PASSWORD: &str = "app";

/// Migrated once, never connected to, cloned by every test.
const TEMPLATE_DB: &str = "sanvi_template";

/// Guards template creation across the *processes* that share one server —
/// `cargo nextest` runs each test in its own process, so a per-process
/// `OnceCell` alone is not enough.
const BOOTSTRAP_LOCK: i64 = 0x5AE7_1DB0;

/// Test pools stay small: with the whole workspace in flight there can be a
/// hundred of them against one server.
const POOL_MAX_CONNECTIONS: u32 = 8;

static PG_PORT: OnceCell<u16> = OnceCell::const_new();
static BOOTSTRAPPED: OnceCell<()> = OnceCell::const_new();
static DB_SEQ: AtomicU64 = AtomicU64::new(0);

/// Absolute path to the workspace `migrations/` directory.
///
/// Resolved against *this* crate's manifest, so it is the same answer no
/// matter which crate's test binary asks.
pub fn migrations_dir() -> String {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../migrations")
        .canonicalize()
        .expect("migrations directory must exist")
        .to_string_lossy()
        .to_string()
}

fn config(port: u16, name: &str, user: &str, password: &str) -> DatabaseConfig {
    DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: name.to_string(),
        user: user.to_string(),
        password: password.to_string(),
        max_connections: POOL_MAX_CONNECTIONS,
        min_connections: 0,
        migrations_dir: migrations_dir(),
        ..DatabaseConfig::default()
    }
}

/// Port of the Postgres every test in this process talks to.
///
/// `SANVI_TEST_PG_PORT` points at a server started outside the test run
/// (`scripts/test-db.sh up`) and shared by every test *process* — the fast
/// path, and the only one that leaves no container behind. Without it a
/// container is started for this process alone.
async fn pg_port() -> u16 {
    *PG_PORT
        .get_or_init(|| async {
            if let Ok(port) = std::env::var("SANVI_TEST_PG_PORT") {
                return port
                    .parse()
                    .expect("SANVI_TEST_PG_PORT must be a port number");
            }
            let node = Postgres::default()
                // Durability buys nothing for a database that is discarded at
                // the end of the run, and the connection ceiling has to clear
                // every pool the workspace opens at once.
                .with_cmd([
                    "-c",
                    "fsync=off",
                    "-c",
                    "synchronous_commit=off",
                    "-c",
                    "full_page_writes=off",
                    "-c",
                    "max_connections=600",
                    "-c",
                    "shared_buffers=256MB",
                ])
                .start()
                .await
                .expect("failed to start postgres");
            let port = node.get_host_port_ipv4(5432).await.expect("postgres port");
            // The container must outlive every test in this process, and
            // nothing drops a `static`. `scripts/test-db.sh prune` reaps the
            // strays; the `SANVI_TEST_PG_PORT` path avoids them entirely.
            std::mem::forget(node);
            port
        })
        .await
}

/// A superuser pool on the `postgres` database, for issuing `CREATE DATABASE`.
///
/// Deliberately **not** cached in a `static`: a sqlx pool binds its background
/// tasks to the runtime that created it, and every `#[tokio::test]` builds and
/// then tears down its own. A shared pool starts failing with "a Tokio 1.x
/// context ... is being shutdown" the moment the first test finishes. Opening
/// one connection per test is the price of that.
async fn admin_pool(port: u16) -> PgPool {
    build_pool(&config(port, "postgres", "postgres", "postgres"))
        .await
        .expect("admin pool")
}

/// Create the two roles and the migrated template database, once per server.
async fn bootstrap(port: u16) {
    BOOTSTRAPPED
        .get_or_init(|| async {
            let admin = admin_pool(port).await;
            // Advisory locks are session-scoped, so lock and unlock must run
            // on one connection — not on whichever the pool hands out next.
            let mut lock = admin.acquire().await.expect("bootstrap connection");
            sqlx::query("SELECT pg_advisory_lock($1)")
                .bind(BOOTSTRAP_LOCK)
                .execute(&mut *lock)
                .await
                .expect("bootstrap lock");

            for (role, password, extra) in [
                (MIGRATOR_ROLE, MIGRATOR_PASSWORD, ""),
                (APP_ROLE, APP_PASSWORD, "NOBYPASSRLS"),
            ] {
                let statement = format!(
                    "DO $$ BEGIN
                       IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role}') THEN
                         CREATE ROLE {role} LOGIN PASSWORD '{password}'
                           NOSUPERUSER NOCREATEDB NOCREATEROLE {extra};
                       END IF;
                     END $$;"
                );
                sqlx::raw_sql(AssertSqlSafe(statement))
                    .execute(&mut *lock)
                    .await
                    .expect("create role");
            }

            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
                    .bind(TEMPLATE_DB)
                    .fetch_one(&mut *lock)
                    .await
                    .expect("template lookup");

            if !exists {
                let create = format!("CREATE DATABASE {TEMPLATE_DB} OWNER {MIGRATOR_ROLE}");
                sqlx::raw_sql(AssertSqlSafe(create))
                    .execute(&mut *lock)
                    .await
                    .expect("create template database");

                let migrator =
                    build_pool(&config(port, TEMPLATE_DB, MIGRATOR_ROLE, MIGRATOR_PASSWORD))
                        .await
                        .expect("template migrator pool");
                Migrator::new(migrations_dir())
                    .run(&migrator)
                    .await
                    .expect("migrations up");
                // `CREATE DATABASE … TEMPLATE` refuses to copy a database
                // that has sessions on it, so the pool must be gone first.
                migrator.close().await;

                // `datallowconn = false` keeps it that way: from here nothing
                // can connect to the template, only clone it.
                let mark = format!(
                    "UPDATE pg_database SET datistemplate = true, datallowconn = false
                     WHERE datname = '{TEMPLATE_DB}'"
                );
                sqlx::raw_sql(AssertSqlSafe(mark))
                    .execute(&mut *lock)
                    .await
                    .expect("mark template");
            }

            sqlx::query("SELECT pg_advisory_unlock($1)")
                .bind(BOOTSTRAP_LOCK)
                .execute(&mut *lock)
                .await
                .expect("bootstrap unlock");
            drop(lock);
            admin.close().await;
        })
        .await;
}

/// A database private to one test, dropped by `scripts/test-db.sh` between
/// runs (a container-backed server takes them with it).
pub struct TestDb {
    port: u16,
    name: String,
    app: PgPool,
    migrator: OnceCell<PgPool>,
    admin: OnceCell<PgPool>,
}

impl TestDb {
    /// Pool for the least-privilege application role — RLS applies.
    pub fn app_pool(&self) -> PgPool {
        self.app.clone()
    }

    /// Pool for the schema owner. RLS does not apply to the owner, so use it
    /// only for setup a migration would otherwise do.
    pub async fn migrator_pool(&self) -> PgPool {
        self.migrator
            .get_or_init(|| async {
                build_pool(&config(
                    self.port,
                    &self.name,
                    MIGRATOR_ROLE,
                    MIGRATOR_PASSWORD,
                ))
                .await
                .expect("migrator pool")
            })
            .await
            .clone()
    }

    /// Superuser pool on this database. Bypasses RLS entirely — for reading
    /// rows back out, not for exercising the application's own access.
    pub async fn admin_pool(&self) -> PgPool {
        self.admin
            .get_or_init(|| async {
                build_pool(&config(self.port, &self.name, "postgres", "postgres"))
                    .await
                    .expect("admin pool")
            })
            .await
            .clone()
    }

    /// Connection settings for the application role, for the rare test that
    /// needs a pool shaped differently (a deliberately tiny one, say).
    pub fn app_config(&self) -> DatabaseConfig {
        config(self.port, &self.name, APP_ROLE, APP_PASSWORD)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

fn next_db_name(prefix: &str) -> String {
    // Unique within a process (counter) and across every process that ever
    // used this server (random) — a pid alone repeats across runs, and a
    // shared server outlives them.
    format!(
        "{prefix}{}_{}",
        uuid::Uuid::now_v7().simple(),
        DB_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Create a database, hand back a handle with the application pool open.
///
/// `template` is the pre-migrated database to copy; `None` leaves it empty.
async fn create_db(prefix: &str, template: Option<&str>) -> TestDb {
    let port = pg_port().await;
    bootstrap(port).await;

    let name = next_db_name(prefix);
    let admin = admin_pool(port).await;
    let from = template
        .map(|t| format!(" TEMPLATE {t}"))
        .unwrap_or_default();
    let create = format!("CREATE DATABASE {name}{from} OWNER {MIGRATOR_ROLE}");
    sqlx::raw_sql(AssertSqlSafe(create))
        .execute(&admin)
        .await
        .expect("create database");
    let grant = format!("GRANT CONNECT ON DATABASE {name} TO {APP_ROLE}");
    sqlx::raw_sql(AssertSqlSafe(grant))
        .execute(&admin)
        .await
        .expect("grant connect");
    admin.close().await;

    let app = build_pool(&config(port, &name, APP_ROLE, APP_PASSWORD))
        .await
        .expect("app pool");

    TestDb {
        port,
        name,
        app,
        migrator: OnceCell::new(),
        admin: OnceCell::new(),
    }
}

/// A private, fully migrated database.
///
/// Cloned from the template, so the cost is a file copy in the server rather
/// than a migration run — and no container start at all after the first.
pub async fn test_db() -> TestDb {
    create_db("sanvi_t_", Some(TEMPLATE_DB)).await
}

/// A private database with the roles in place but **no** migrations applied.
///
/// For the suite that exercises `Migrator` itself, which needs to run the
/// migrations up and back down on an empty database.
pub async fn blank_db() -> TestDb {
    create_db("sanvi_b_", None).await
}
