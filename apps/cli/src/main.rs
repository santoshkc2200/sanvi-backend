//! `sanvi-cli` — operator commands: migrations, seed, config, contract.
//!
//! - `migrate up|down|status` — runs under the migrator role (flags override
//!   the configured user; production sets them from the secret manager).
//! - `seed` — placeholder (no seeds exist in phase 00).
//! - `config check` — loads and validates the full configuration, printing
//!   the resolved values with secrets redacted.
//! - `openapi [--check]` — prints the merged OpenAPI document; `--check`
//!   fails when the committed per-context specs in `api/` have drifted.

mod access;
mod config;
mod healthcheck;
mod migrate;
mod openapi;
mod tenant;

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

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
    /// Provision and manage tenants (operator use until self-service lands).
    Tenant {
        #[command(subcommand)]
        action: TenantAction,
    },
    /// Seed reference data: the role catalog (`roles`) and the Keto tuple
    /// rebuild (`access`).
    Seed {
        #[command(subcommand)]
        kind: SeedKind,
    },
    /// Grant platform/tenant memberships to registered users.
    Membership {
        #[command(subcommand)]
        action: MembershipAction,
    },
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
    /// Probe an HTTP endpoint without a shell or `curl` (container HEALTHCHECK).
    Healthcheck {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 8080)]
        port: u16,
        #[arg(long, default_value = "/healthz")]
        path: String,
        #[arg(long, default_value_t = 3)]
        timeout_secs: u64,
    },
}

#[derive(Subcommand)]
enum SeedKind {
    /// Seed the system role catalog (idempotent).
    Roles,
    /// Rebuild every Keto relation tuple from the catalog + memberships.
    Access,
}

#[derive(Subcommand)]
enum MembershipAction {
    /// Make a registered user a tenant member (or assign a platform role).
    Grant {
        /// The user's email (they must have registered already).
        #[arg(long)]
        email: String,
        /// Role key (e.g. `tenant_owner`, `platform_owner`).
        #[arg(long)]
        role: String,
        /// Tenant slug for tenant roles.
        #[arg(long)]
        tenant: Option<String>,
        /// Assign a platform role (Keto tuple; phase 03 gives these a DB
        /// home).
        #[arg(long)]
        platform: bool,
    },
}

#[derive(Subcommand)]
enum TenantAction {
    /// Provision a new tenant.
    Create {
        #[command(flatten)]
        args: TenantCreateArgs,
        /// Move straight to `active` instead of leaving it `provisioning`.
        #[arg(long)]
        activate: bool,
    },
    /// List tenants (cursor-paginated).
    List {
        /// Cursor: tenant id of the last item of the previous page.
        #[arg(long)]
        after: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        status: Option<String>,
    },
    /// Show one tenant.
    Get { id: String },
    /// Activate a provisioning tenant.
    Activate { id: String },
    /// Suspend a tenant (requires --reason).
    Suspend {
        id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Resume a suspended tenant.
    Resume { id: String },
    /// Archive a tenant (read-only; schedules purge).
    Archive { id: String },
}

/// Shared arguments for `tenant create`.
#[derive(Args)]
struct TenantCreateArgs {
    /// DNS-safe subdomain slug (lowercase; immutable).
    #[arg(long)]
    slug: String,
    /// Human-facing tenant name.
    #[arg(long)]
    display_name: String,
    /// Data residency region: eu | jp | us.
    #[arg(long, default_value = "us")]
    region: String,
    /// Default locale (BCP 47-ish tag, e.g. `en`, `ja-JP`).
    #[arg(long, default_value = "en")]
    default_locale: String,
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
        Command::Tenant { action } => {
            let result = match action {
                TenantAction::Create { args, activate } => {
                    tenant::create(
                        &cli.env,
                        &tenant::TenantCreateArgs {
                            slug: args.slug,
                            display_name: args.display_name,
                            region: args.region,
                            default_locale: args.default_locale,
                        },
                        activate,
                    )
                    .await
                }
                TenantAction::List {
                    after,
                    limit,
                    status,
                } => tenant::list(&cli.env, after, limit, status).await,
                TenantAction::Get { id } => tenant::get(&cli.env, &id).await,
                TenantAction::Activate { id } => {
                    tenant::lifecycle(&cli.env, &id, tenant::LifecycleCommand::Activate, None).await
                }
                TenantAction::Suspend { id, reason } => {
                    tenant::lifecycle(&cli.env, &id, tenant::LifecycleCommand::Suspend, reason)
                        .await
                }
                TenantAction::Resume { id } => {
                    tenant::lifecycle(&cli.env, &id, tenant::LifecycleCommand::Resume, None).await
                }
                TenantAction::Archive { id } => {
                    tenant::lifecycle(&cli.env, &id, tenant::LifecycleCommand::Archive, None).await
                }
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("tenant command failed: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Seed { kind } => {
            let kind = match kind {
                SeedKind::Roles => access::SeedKind::Roles,
                SeedKind::Access => access::SeedKind::Access,
            };
            match access::seed(&cli.env, kind).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("seed failed: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Membership { action } => {
            let MembershipAction::Grant {
                email,
                role,
                tenant,
                platform,
            } = action;
            match access::membership_grant(
                &cli.env,
                access::MembershipGrantArgs {
                    email,
                    role,
                    tenant_slug: tenant,
                    platform,
                },
            )
            .await
            {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("membership grant failed: {err:#}");
                    ExitCode::FAILURE
                }
            }
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
        Command::Healthcheck {
            host,
            port,
            path,
            timeout_secs,
        } => match healthcheck::run(&host, port, &path, timeout_secs).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("healthcheck failed: {err:#}");
                ExitCode::FAILURE
            }
        },
    }
}
