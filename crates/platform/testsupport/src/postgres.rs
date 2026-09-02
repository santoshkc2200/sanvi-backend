use std::sync::atomic::{AtomicU64, Ordering};

use sanvi_persistence::{DatabaseConfig, Migrator, PgPool, build_pool};
use sha2::{Digest, Sha256};
use sqlx::{AssertSqlSafe, Connection, PgConnection, postgres::PgConnectOptions};
use testcontainers::{ImageExt, ReuseDirective, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;
use tokio::sync::OnceCell;

/// Migration owner. Matches the production split: a role that owns the schema
/// and a separate least-privilege role the application connects with.
const MIGRATOR_ROLE: &str = "sanvi_migrator";
const MIGRATOR_PASSWORD: &str = "mig";
const APP_ROLE: &str = "sanvi_app";
const APP_PASSWORD: &str = "app";

/// Migrated once, never connected to, cloned by every test.
///
/// The name carries a fingerprint of the migration set it was built from. A
/// bare `sanvi_template` made "does it exist?" the only question asked, so a
/// server that had one kept handing it out after the migrations on disk had
/// moved on — the suite then passed against a schema the repository could no
/// longer produce. Keying by fingerprint turns that into a miss.
const TEMPLATE_PREFIX: &str = "sanvi_template_";

/// Guards template creation across the *processes* that share one server —
/// `cargo nextest` runs each test in its own process, so a per-process
/// `OnceCell` alone is not enough.
const BOOTSTRAP_LOCK: i64 = 0x5AE7_1DB0;

/// Test pools stay small: with the whole workspace in flight there can be a
/// hundred of them against one server.
const POOL_MAX_CONNECTIONS: u32 = 8;

static PG_PORT: OnceCell<u16> = OnceCell::const_new();
static BOOTSTRAPPED: OnceCell<String> = OnceCell::const_new();
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

/// Name every test process converges on, so a bare `cargo nextest run` ends up
/// with one server rather than one per process. `nextest` runs each test in its
/// own process, so an anonymous container here meant ~one Postgres per
/// DB-touching test — enough of them, concurrently, to starve the daemon and
/// drop connections mid-migration.
const SHARED_CONTAINER_NAME: &str = "sanvi_test_pg";

/// Port of the Postgres every test in this process talks to.
///
/// `SANVI_TEST_PG_PORT` points at a server started outside the test run
/// (`scripts/test-db.sh up`) and shared by every test *process* — the fast
/// path, and the one that leaves nothing for this crate to manage.
///
/// Without it, processes converge on [`SHARED_CONTAINER_NAME`] through
/// testcontainers' reuse directive: the first to arrive starts it, the rest
/// attach to it. Reuse implies keep, so the container outlives the run by
/// design — `scripts/test-db.sh prune` is still how it is reclaimed.
async fn pg_port() -> u16 {
    *PG_PORT
        .get_or_init(|| async {
            if let Ok(port) = std::env::var("SANVI_TEST_PG_PORT") {
                return port
                    .parse()
                    .expect("SANVI_TEST_PG_PORT must be a port number");
            }
            let node = start_shared_postgres().await;
            let port = await_ready_postgres(&node).await;
            // The container must outlive every test in this process. Dropping
            // it here would remove the server the other tests are still using,
            // and a `static` would never run `Drop` anyway — so the handle is
            // released deliberately rather than by accident. With reuse, this
            // leaks one named container per machine, not one per process.
            std::mem::forget(node);
            port
        })
        .await
}

/// Start the shared server, or attach to the one another process started.
///
/// `ReuseDirective::Always` de-duplicates by name but does not serialize the
/// creation itself: `nextest` starts many processes at once, they all look for
/// `sanvi_test_pg`, all miss, and all try to create it. One wins and the rest
/// get `409 Conflict`. Losing that race is not an error — the container the
/// winner created is exactly what the loser wanted — so a conflict is retried,
/// and the retry's reuse lookup finds it.
async fn start_shared_postgres() -> testcontainers::ContainerAsync<Postgres> {
    const CONFLICT_RETRIES: u32 = 40;
    const CONFLICT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);

    for attempt in 0..=CONFLICT_RETRIES {
        match shared_postgres_request().start().await {
            Ok(node) => return node,
            Err(error) if attempt < CONFLICT_RETRIES && is_name_conflict(&error) => {
                tokio::time::sleep(CONFLICT_BACKOFF).await;
            }
            Err(error) => panic!("failed to start postgres: {error}"),
        }
    }
    unreachable!("the loop either returns a container or panics")
}

/// Wait until the shared server actually answers, and return its port.
///
/// Only the process that *created* the container ran testcontainers' readiness
/// wait. Every process that attached to it instead skipped that wait entirely,
/// so it must establish readiness itself — and readiness here means one thing,
/// "a connection succeeds", not a sequence of separate conditions. Polling the
/// connection subsumes the earlier stages (the port not yet published, the
/// server listening but still starting up) rather than racing each one.
async fn await_ready_postgres(node: &testcontainers::ContainerAsync<Postgres>) -> u16 {
    const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    const READY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);

    let deadline = std::time::Instant::now() + READY_TIMEOUT;
    let mut last_error = String::new();
    while std::time::Instant::now() < deadline {
        match node.get_host_port_ipv4(5432).await {
            Ok(port) => match admin_conn(port).await {
                Ok(conn) => {
                    // Hand back a server that has proven it accepts work, not
                    // merely one whose port is mapped.
                    drop(conn);
                    return port;
                }
                Err(error) => last_error = error.to_string(),
            },
            Err(error) => last_error = error.to_string(),
        }
        tokio::time::sleep(READY_BACKOFF).await;
    }
    panic!("shared postgres never became ready within {READY_TIMEOUT:?}: {last_error}");
}

/// A 409 from Docker means another process created the container between our
/// reuse lookup and our create. Matched on the message because `bollard`'s
/// error carries the status only as part of the server response.
fn is_name_conflict(error: &testcontainers::TestcontainersError) -> bool {
    let rendered = error.to_string();
    rendered.contains("already in use") || rendered.contains("Conflict")
}

fn shared_postgres_request() -> testcontainers::ContainerRequest<Postgres> {
    Postgres::default()
                // One server for the whole run: every process converges on this
                // name instead of starting an anonymous container of its own.
                .with_container_name(SHARED_CONTAINER_NAME)
                .with_reuse(ReuseDirective::Always)
                // Pinned to the same image `scripts/test-db.sh` starts. The
                // module default is `postgres:11-alpine`, which predates the
                // `STRATEGY` option `create_database` relies on, so leaving it
                // unpinned fails every test with `option "strategy" not
                // recognized` — after starting a container per test.
                .with_tag("18-alpine")
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
}

/// A superuser connection to the `postgres` database, for issuing
/// `CREATE DATABASE` and `DROP DATABASE`.
///
/// Deliberately **not** cached in a `static`: sqlx binds a handle's background
/// work to the runtime that created it, and every `#[tokio::test]` builds and
/// then tears down its own. A shared handle starts failing with "a Tokio 1.x
/// context ... is being shutdown" the moment the first test finishes. Opening
/// one per test is the price of that.
///
/// One bare connection rather than a pool: every caller issues a couple of
/// statements and hangs up, so a pool's spare connections and background
/// reaper were setup cost with nothing to amortise it over — paid twice per
/// test, once creating the database and once dropping it.
async fn admin_conn(port: u16) -> Result<PgConnection, sqlx::Error> {
    PgConnection::connect_with(
        &PgConnectOptions::new()
            .host("127.0.0.1")
            .port(port)
            .database("postgres")
            .username("postgres")
            .password("postgres")
            .application_name("sanvi-testsupport"),
    )
    .await
}

/// Name of the template for the migrations currently on disk.
///
/// Built from the same per-file checksums sqlx itself validates with, so the
/// fingerprint moves exactly when sqlx would start rejecting an already-applied
/// migration as modified.
async fn template_name() -> String {
    let migrations = sqlx::migrate::Migrator::new(std::path::Path::new(&migrations_dir()))
        .await
        .expect("load migrations");
    let mut fingerprint = Sha256::new();
    for migration in migrations.iter() {
        fingerprint.update(migration.version.to_le_bytes());
        fingerprint.update(&migration.checksum);
    }
    format!(
        "{TEMPLATE_PREFIX}{}",
        hex::encode(&fingerprint.finalize()[..8])
    )
}

/// Drop a database that may be flagged as a template.
///
/// `DROP DATABASE` refuses a `datistemplate` database, so the flag has to come
/// off first. Errors are ignored throughout: losing a race to another process
/// that is reaping the same leftover is not a failure.
async fn drop_template(lock: &mut sqlx::PgConnection, name: &str) {
    let _ = sqlx::query("UPDATE pg_database SET datistemplate = false WHERE datname = $1")
        .bind(name)
        .execute(&mut *lock)
        .await;
    let statement = format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)");
    let _ = sqlx::raw_sql(AssertSqlSafe(statement))
        .execute(&mut *lock)
        .await;
}

/// Create the two roles and the migrated template database, once per server.
///
/// Returns the template's name, which depends on the migrations on disk.
async fn bootstrap(port: u16) -> String {
    BOOTSTRAPPED
        .get_or_init(|| async {
            let template = template_name().await;
            // Advisory locks are session-scoped, so lock and unlock must run
            // on one connection — which a bare connection guarantees for free,
            // where a pool could hand the unlock a different one.
            let mut lock = admin_conn(port).await.expect("bootstrap connection");
            sqlx::query("SELECT pg_advisory_lock($1)")
                .bind(BOOTSTRAP_LOCK)
                .execute(&mut lock)
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
                    .execute(&mut lock)
                    .await
                    .expect("create role");
            }

            // `datistemplate` is set below only once the migrations have
            // succeeded, so it — not the database's existence — is what marks a
            // template as finished. A previous run that panicked part-way
            // through the migrations leaves an unflagged database behind; it
            // has to be rebuilt rather than cloned, or every test in every
            // later run inherits its half-applied schema.
            let ready: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                   SELECT 1 FROM pg_database WHERE datname = $1 AND datistemplate
                 )",
            )
            .bind(&template)
            .fetch_one(&mut lock)
            .await
            .expect("template lookup");

            if !ready {
                drop_template(&mut lock, &template).await;

                let create = format!("CREATE DATABASE {template} OWNER {MIGRATOR_ROLE}");
                sqlx::raw_sql(AssertSqlSafe(create))
                    .execute(&mut lock)
                    .await
                    .expect("create template database");

                let migrator =
                    build_pool(&config(port, &template, MIGRATOR_ROLE, MIGRATOR_PASSWORD))
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
                     WHERE datname = '{template}'"
                );
                sqlx::raw_sql(AssertSqlSafe(mark))
                    .execute(&mut lock)
                    .await
                    .expect("mark template");
            }

            // Templates for migration sets this checkout has moved past are
            // 12 MB each on a 2 GB tmpfs. Editing a migration would otherwise
            // strand one per edit. The pattern escapes its underscores — they
            // are single-character wildcards to `LIKE` — and is deliberately
            // wider than `TEMPLATE_PREFIX` so the unfingerprinted
            // `sanvi_template` earlier revisions left behind is reaped too.
            let stale: Vec<String> = sqlx::query_scalar(
                r"SELECT datname FROM pg_database
                  WHERE datname LIKE 'sanvi\_template%' AND datname <> $1",
            )
            .bind(&template)
            .fetch_all(&mut lock)
            .await
            .unwrap_or_default();
            for name in &stale {
                drop_template(&mut lock, name).await;
            }

            sqlx::query("SELECT pg_advisory_unlock($1)")
                .bind(BOOTSTRAP_LOCK)
                .execute(&mut lock)
                .await
                .expect("bootstrap unlock");
            let _ = lock.close().await;
            template
        })
        .await
        .clone()
}

/// A database private to one test, dropped when the handle is.
///
/// Anything holding a pool from this handle — `app_pool()` and friends hand out
/// clones that outlive it — has to keep the handle alive for as long as it uses
/// them, or it will be querying a database that no longer exists.
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

impl Drop for TestDb {
    fn drop(&mut self) {
        // A clone of the template costs ~12 MB and the shared server keeps its
        // data directory on a 4 GB tmpfs, so a run that reclaims nothing until
        // it ends fills the disk part-way through the suite — the workspace
        // makes 164 of these. Reclaiming here bounds the server by how many
        // tests are in flight at once rather than how many run at all, and
        // leaves `scripts/test-db.sh` a handful of strays instead of all of
        // them.
        //
        // `Drop` cannot await, and the test's own runtime is being torn down
        // around it, so the work borrows a thread with a runtime of its own.
        let port = self.port;
        let name = std::mem::take(&mut self.name);
        let _ = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async move {
                let Ok(mut admin) = admin_conn(port).await else {
                    // The server is already gone — a container-per-process run
                    // taking its databases with it. Nothing left to reclaim.
                    return;
                };
                // This process still holds pools on the database — the fields
                // below are dropped only after this body returns — and FORCE
                // closes those sessions server-side rather than making us
                // terminate them by hand first.
                let statement = format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)");
                let _ = sqlx::raw_sql(AssertSqlSafe(statement))
                    .execute(&mut admin)
                    .await;
                let _ = admin.close().await;
            });
        })
        .join();
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
/// `migrated` copies the pre-migrated template; otherwise the database starts
/// empty.
async fn create_db(prefix: &str, migrated: bool) -> TestDb {
    let port = pg_port().await;
    let template = bootstrap(port).await;

    let name = next_db_name(prefix);
    let mut admin = admin_conn(port).await.expect("admin connection");
    let from = if migrated {
        format!(" TEMPLATE {template} STRATEGY = wal_log")
    } else {
        String::new()
    };
    let create = format!("CREATE DATABASE {name}{from} OWNER {MIGRATOR_ROLE}");
    sqlx::raw_sql(AssertSqlSafe(create))
        .execute(&mut admin)
        .await
        .expect("create database");
    let grant = format!("GRANT CONNECT ON DATABASE {name} TO {APP_ROLE}");
    sqlx::raw_sql(AssertSqlSafe(grant))
        .execute(&mut admin)
        .await
        .expect("grant connect");
    let _ = admin.close().await;

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
/// Cloned from the template, so the clone is WAL-logged rather than a file copy
/// — and no container start at all after the first.
pub async fn test_db() -> TestDb {
    create_db("sanvi_t_", true).await
}

/// A private database with the roles in place but **no** migrations applied.
///
/// For the suite that exercises `Migrator` itself, which needs to run the
/// migrations up and back down on an empty database.
pub async fn blank_db() -> TestDb {
    create_db("sanvi_b_", false).await
}
