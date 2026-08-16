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
mod control;
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
    /// Phase-03 control plane: feature catalog and entitlement overrides.
    Feature {
        #[command(subcommand)]
        action: FeatureAction,
    },
    /// Phase-03 control plane: entitlement overrides (operator grants).
    Entitlement {
        #[command(subcommand)]
        action: EntitlementAction,
    },
    /// Phase-03: role catalog listing.
    RoleList,
    /// Phase-03: impersonation grant review.
    Impersonation {
        #[command(subcommand)]
        action: ImpersonationAction,
    },
    /// Phase-03: audit chain verification and queries.
    Audit {
        #[command(subcommand)]
        action: AuditAction,
    },
    /// Phase-03: four-eyes approval review.
    Approval {
        #[command(subcommand)]
        action: ApprovalAction,
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
    /// Seed the feature catalog (idempotent).
    Features,
}

#[derive(Subcommand)]
enum FeatureAction {
    /// List the feature catalog.
    List,
    /// Define a new feature.
    Define {
        #[arg(long)]
        key: String,
        #[arg(long)]
        name: String,
        /// boolean | quota
        #[arg(long)]
        kind: String,
        #[arg(long, default_value_t = false)]
        default_enabled: bool,
        #[arg(long)]
        default_limit: Option<i64>,
        /// public | hidden
        #[arg(long, default_value = "public")]
        visibility: String,
    },
}

#[derive(Subcommand)]
enum EntitlementAction {
    /// Grant an override to a tenant.
    Grant {
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        feature: String,
        #[arg(long, default_value_t = true)]
        enabled: bool,
        #[arg(long)]
        limit: Option<i64>,
        #[arg(long)]
        expires_at: Option<String>,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Revoke an override.
    Revoke {
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        feature: String,
    },
    /// List a tenant's stored override rows.
    List {
        #[arg(long)]
        tenant: String,
    },
}

#[derive(Subcommand)]
enum ImpersonationAction {
    /// List active grants (optionally for one tenant).
    List {
        #[arg(long)]
        tenant: Option<String>,
    },
    /// Revoke a grant (immediate).
    Revoke {
        #[arg(long)]
        id: uuid::Uuid,
    },
}

#[derive(Subcommand)]
enum AuditAction {
    /// Verify the hash chain (fails on any mismatch).
    Verify,
    /// List recent entries (optionally filtered).
    List {
        #[arg(long)]
        tenant_id: Option<uuid::Uuid>,
        #[arg(long)]
        action: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
}

#[derive(Subcommand)]
enum ApprovalAction {
    /// List pending four-eyes requests.
    List,
    /// Approve a request (executes the recorded action).
    Approve {
        #[arg(long)]
        id: uuid::Uuid,
    },
    /// Reject a request.
    Reject {
        #[arg(long)]
        id: uuid::Uuid,
    },
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
                SeedKind::Features => access::SeedKind::Features,
            };
            match access::seed(&cli.env, kind).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("seed failed: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Feature { action } => {
            let result = match action {
                FeatureAction::List => control::feature_list(&cli.env).await,
                FeatureAction::Define {
                    key,
                    name,
                    kind,
                    default_enabled,
                    default_limit,
                    visibility,
                } => {
                    control::feature_define(
                        &cli.env,
                        control::FeatureDefineArgs {
                            key: &key,
                            name: &name,
                            kind: &kind,
                            default_enabled,
                            default_limit,
                            visibility: &visibility,
                        },
                    )
                    .await
                }
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("feature command failed: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Entitlement { action } => {
            let result = match action {
                EntitlementAction::Grant {
                    tenant,
                    feature,
                    enabled,
                    limit,
                    expires_at,
                    reason,
                } => {
                    let expires_at = match expires_at {
                        Some(raw) => match chrono::DateTime::parse_from_rfc3339(&raw) {
                            Ok(parsed) => Some(parsed.with_timezone(&chrono::Utc)),
                            Err(err) => {
                                eprintln!("invalid --expires-at value: {err:#}");
                                return ExitCode::FAILURE;
                            }
                        },
                        None => None,
                    };
                    control::entitlement_grant(
                        &cli.env,
                        control::EntitlementGrantArgs {
                            tenant_slug: tenant,
                            feature,
                            enabled,
                            limit,
                            expires_at,
                            reason,
                        },
                    )
                    .await
                }
                EntitlementAction::Revoke { tenant, feature } => {
                    control::entitlement_revoke(&cli.env, &tenant, &feature).await
                }
                EntitlementAction::List { tenant } => {
                    control::entitlement_list(&cli.env, &tenant).await
                }
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("entitlement command failed: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::RoleList => match control::role_list(&cli.env).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("role list failed: {err:#}");
                ExitCode::FAILURE
            }
        },
        Command::Impersonation { action } => {
            let result = match action {
                ImpersonationAction::List { tenant } => {
                    control::impersonation_list(&cli.env, tenant.as_deref()).await
                }
                ImpersonationAction::Revoke { id } => {
                    control::impersonation_revoke(&cli.env, id).await
                }
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("impersonation command failed: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Audit { action } => {
            let result = match action {
                AuditAction::Verify => control::audit_verify(&cli.env).await,
                AuditAction::List {
                    tenant_id,
                    action,
                    limit,
                } => control::audit_list(&cli.env, tenant_id, action, limit).await,
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("audit command failed: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Approval { action } => {
            let result = match action {
                ApprovalAction::List => control::approval_list(&cli.env).await,
                ApprovalAction::Approve { id } => {
                    control::approval_decide(&cli.env, id, true).await
                }
                ApprovalAction::Reject { id } => {
                    control::approval_decide(&cli.env, id, false).await
                }
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("approval command failed: {err:#}");
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
