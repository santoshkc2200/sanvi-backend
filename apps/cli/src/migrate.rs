//! `migrate`: apply/revert/list database migrations.

use anyhow::Context;
use sanvi_persistence::Migrator;

use crate::MigrateAction;
use crate::config;

pub async fn run(
    environment: &str,
    action: MigrateAction,
    user: Option<String>,
    password: Option<String>,
) -> anyhow::Result<()> {
    let mut cli_config = config::load(environment)?;
    if let Some(user) = user {
        cli_config.database.user = user;
    }
    if let Some(password) = password {
        cli_config.database.password = password;
    }
    // DDL against scale data (index builds, backfills) legitimately outlives
    // any request-shaped statement budget; migrations run uncapped regardless
    // of what the served processes use (TASK-021, FR-1110).
    cli_config.database.statement_timeout_ms = 0;

    let pool = sanvi_persistence::build_pool(&cli_config.database)
        .await
        .context("failed to connect to postgres")?;
    let migrator = Migrator::new(&cli_config.database.migrations_dir);

    match action {
        MigrateAction::Up => {
            migrator.run(&pool).await?;
            println!("migrations up to date");
        }
        MigrateAction::Down { target } => {
            migrator.undo(&pool, target).await?;
            println!("reverted migrations down to {target}");
        }
        MigrateAction::Status => {
            let applied = migrator.applied(&pool).await?;
            if applied.is_empty() {
                println!("no migrations applied");
            } else {
                for (version, description) in applied {
                    println!("{version:>16}  {description}");
                }
            }
        }
    }

    pool.close().await;
    Ok(())
}
