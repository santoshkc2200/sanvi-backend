//! `sanvi-cli` — operator commands: migrations, seed, config, contract.
//!
//! - `migrate up|down|status` — runs under the migrator role (flags override
//!   the configured user; production sets them from the secret manager).
//! - `seed` — placeholder (no seeds exist in phase 00).
//! - `config check` — loads and validates the full configuration, printing
//!   the resolved values with secrets redacted.
//! - `openapi [--check]` — prints the merged OpenAPI document; `--check`
//!   fails when the committed per-context specs in `api/` have drifted.

mod config;
mod migrate;
mod openapi;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "sanvi", version, about = "Sanvi operator CLI")]
struct Cli {
    /// Configuration environment (`local`, `staging`, `production`).
    #[arg(long, env = "SANVI_ENV", default_value = "local")]
    env: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run database migrations (migrator role).
    Migrate {
        #[command(subcommand)]
        action: MigrateAction,
        /// Database user override (defaults to config value).
        #[arg(long)]
        user: Option<String>,
        /// Database password override (from the secret manager).
        #[arg(long)]
        password: Option<String>,
    },
    /// Seed reference data (nothing to seed in phase 00).
    Seed,
    /// Load, validate and print the configuration.
    Config,
    /// Print or verify the OpenAPI contract.
    Openapi {
        /// Fail when `api/*.yaml` has drifted from the generated document.
        #[arg(long)]
        check: bool,
        /// (Re)generate the committed `api/{context}.yaml` specs.
        #[arg(long)]
        write: bool,
    },
}

#[derive(Subcommand)]
enum MigrateAction {
    /// Apply all pending migrations.
    Up,
    /// Revert to `--target` (0 = revert everything).
    Down {
        #[arg(long, default_value = "0")]
        target: i64,
    },
    /// List applied migrations.
    Status,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Migrate {
            action,
            user,
            password,
        } => match migrate::run(&cli.env, action, user, password).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("migrate failed: {err:#}");
                ExitCode::FAILURE
            }
        },
        Command::Seed => {
            println!("no seeds are defined in phase 00");
            ExitCode::SUCCESS
        }
        Command::Config => match config::check(&cli.env) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("config check failed: {err:#}");
                ExitCode::from(2)
            }
        },
        Command::Openapi { check, write } => match openapi::run(check, write) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("openapi check failed: {err:#}");
                ExitCode::FAILURE
            }
        },
    }
}
