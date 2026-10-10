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
mod archive;
mod billing;
mod config;
mod control;
mod dlq;
mod healthcheck;
mod i18n;
mod migrate;
mod offboard;
mod openapi;
mod payments;
mod retention_report;
mod rotate_credentials;
mod seed_scale;
mod tenant;
mod tenant_restore;
mod theme;

use std::{process::ExitCode, sync::Arc};

use clap::{Args, Parser, Subcommand};
use sanvi_kernel::{RawIdGenerator, SnowflakeGenerator, TenantId};
use sanvi_platform_admin::domain::{ApprovalId, ImpersonationGrantId};

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
    /// Rehearse credential rotation using disposable local services and keys.
    RotateCredentials {
        /// Required: this command never rotates deployment credentials.
        #[arg(long, required = true)]
        rehearse: bool,
        /// Write measured, secret-free JSON evidence here.
        #[arg(long, default_value = "benchmarks/dr/rotation-local.json")]
        report: std::path::PathBuf,
    },
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
    /// Phase-04: billing operator commands.
    Billing {
        #[command(subcommand)]
        action: BillingAction,
    },
    /// Phase-09: payments operator commands (Connect webhook queue).
    Payments {
        #[command(subcommand)]
        action: PaymentsAction,
    },
    /// Phase-11 (TASK-023): the platform outbox dead-letter queue —
    /// triage what the dispatcher gave up on, replay after the fix.
    Dlq {
        #[command(subcommand)]
        action: DlqAction,
    },
    /// Phase-11 (TASK-025): restore one tenant's rows from a restored
    /// snapshot into the live system, without touching any other tenant.
    TenantRestore {
        /// The tenant slug to restore.
        #[arg(long)]
        tenant: String,
        /// DSN of the snapshot database (a restored backup) to read from.
        #[arg(long)]
        from_dsn: String,
        /// Print the per-table plan and write nothing.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
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
    /// Phase-06 translation workflow: extract, check or report.
    I18n {
        #[command(subcommand)]
        action: I18nAction,
    },
    /// Phase-07 theme registry: publish artifacts, list, withdraw.
    Theme {
        #[command(subcommand)]
        action: ThemeAction,
    },
    /// Phase-11 (TASK-019): deterministic, production-shaped scale data.
    /// Refuses any database whose name does not match the staging pattern.
    SeedScale {
        /// The generation seed: the same seed produces identical row counts
        /// and checksums.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Print the plan's row counts and checksum without writing.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// TASK-028 (FR-1129): move a range of a table to encrypted cold
    /// storage (migrator role; see docs/runbooks/archival.md).
    Archive {
        /// Target table, `schema.table` (e.g. audit.entries).
        #[arg(long)]
        table: String,
        /// Tenant slug, decimal id, or `platform` for platform-scope rows.
        #[arg(long, default_value = "platform")]
        tenant: String,
        /// Inclusive range start (RFC 3339).
        #[arg(long)]
        from: String,
        /// Exclusive range end (RFC 3339).
        #[arg(long)]
        until: String,
        /// Print the plan (row counts) and write nothing.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Database user override (defaults to config; archival needs the
        /// migrator role).
        #[arg(long)]
        user: Option<String>,
        /// Database password override.
        #[arg(long)]
        password: Option<String>,
    },
    /// TASK-028 (FR-1129): restore an archived range and prove the rows
    /// (count, ordering, checksum) are identical.
    ArchiveRestore {
        /// The archive id to restore.
        #[arg(long)]
        id: i64,
        /// Print the archive's registry row and write nothing.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Database user override (migrator role).
        #[arg(long)]
        user: Option<String>,
        /// Database password override.
        #[arg(long)]
        password: Option<String>,
    },
    /// TASK-028 (FR-1131): tenant offboarding — export-then-purge.
    /// Start with `--dry-run` (the plan); the purge needs a verified
    /// export and `--confirm "offboard <slug>"`.
    Offboard {
        /// The tenant slug being offboarded.
        #[arg(long)]
        tenant: String,
        /// The offboarding record's idempotency key.
        #[arg(long)]
        key: String,
        /// plan | export | purge | scan (omitted + --dry-run = plan).
        #[arg(long)]
        stage: Option<String>,
        /// Typed confirmation naming the tenant; required for purge.
        #[arg(long)]
        confirm: Option<String>,
        /// Print the plan and write nothing.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// A neighbour tenant that must be byte-identical afterwards.
        #[arg(long)]
        neighbour: Option<String>,
        /// Database user override (migrator role).
        #[arg(long)]
        user: Option<String>,
        /// Database password override.
        #[arg(long)]
        password: Option<String>,
    },
    /// TASK-028 (FR-1130): the retention report — every class's rule,
    /// last run, rows affected and oldest surviving row, generated from
    /// live data. Fails loudly on a stalled class.
    RetentionReport {
        /// Also write the JSON report here (evidence-pack input).
        #[arg(long)]
        write: Option<String>,
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
        id: ImpersonationGrantId,
    },
}

#[derive(Subcommand)]
enum AuditAction {
    /// Verify the hash chain (fails on any mismatch).
    Verify,
    /// List recent entries (optionally filtered).
    List {
        #[arg(long)]
        tenant_id: Option<TenantId>,
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
        id: ApprovalId,
    },
    /// Reject a request.
    Reject {
        #[arg(long)]
        id: ApprovalId,
    },
}

#[derive(Subcommand)]
enum BillingAction {
    /// List plans with prices and entitlement grants.
    Plans,
    /// Fail loudly when automatic_tax is enabled without an active Stripe
    /// Tax registration.
    TaxCheck,
    /// One manual Stripe → local reconciliation pass.
    Reconcile,
    /// The webhook store-first queue's backlog.
    Webhooks,
}

#[derive(Subcommand)]
enum PaymentsAction {
    /// The Connect webhook store-first queue's backlog and dead-letter head.
    Webhooks,
    /// Requeue one dead-lettered Connect webhook event after fixing the
    /// root cause.
    Replay {
        /// The Stripe event id (`evt_…`) to requeue.
        event_id: String,
    },
    /// One manual connection-status reconciliation pass (the lost-webhook
    /// safety net).
    Sync,
}

#[derive(Subcommand)]
enum DlqAction {
    /// The platform outbox dead-letter head, newest first.
    List {
        /// Maximum rows to show.
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Requeue one dead-lettered outbox row after fixing the root cause.
    Replay {
        /// The outbox event id to requeue.
        event_id: i64,
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
enum I18nAction {
    /// Scan the source tree for `t!(...)` keys and print the key → file map.
    Extract,
    /// The CI gate: catalog validity, en/ja parity, placeholder parity.
    Check,
    /// Per-domain coverage table.
    Report,
}

#[derive(Subcommand)]
enum ThemeAction {
    /// Validate a theme directory, build its artifact and install the
    /// version (no application deploy).
    Publish {
        /// Directory containing theme.json + tokens.
        dir: String,
        /// Publisher identity (first-party: `sanvi`).
        #[arg(long, default_value = "sanvi")]
        publisher: String,
        /// public | premium.
        #[arg(long, default_value = "public")]
        visibility: String,
        /// For premium themes: the entitlement feature key.
        #[arg(long)]
        required_feature: Option<String>,
        /// Bypass the WCAG AA contrast gate (recorded in the pipeline log).
        #[arg(long)]
        allow_contrast_warnings: bool,
    },
    /// List the theme registry with version history.
    List,
    /// List one theme's version history.
    Versions { key: String },
    /// Withdraw a version (kill switch; pinned tenants keep rendering).
    Yank { key: String, version: String },
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
    let config = match config::load(&cli.env) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("configuration error:\n{err:#}");
            return ExitCode::from(2);
        }
    };
    let node_id = match config.ids.resolve(&cli.env, 3) {
        Ok(node_id) => node_id,
        Err(err) => {
            eprintln!("configuration error:\n{err:#}");
            return ExitCode::from(2);
        }
    };
    let ids: Arc<dyn RawIdGenerator> = Arc::new(SnowflakeGenerator::new(node_id));

    match cli.command {
        Command::RotateCredentials {
            rehearse: _,
            report,
        } => match rotate_credentials::run(&report).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("rotation rehearsal failed: {err:#}");
                ExitCode::FAILURE
            }
        },
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
                        ids.clone(),
                    )
                    .await
                }
                TenantAction::List {
                    after,
                    limit,
                    status,
                } => tenant::list(&cli.env, after, limit, status, ids.clone()).await,
                TenantAction::Get { id } => tenant::get(&cli.env, &id, ids.clone()).await,
                TenantAction::Activate { id } => {
                    tenant::lifecycle(
                        &cli.env,
                        &id,
                        tenant::LifecycleCommand::Activate,
                        None,
                        ids.clone(),
                    )
                    .await
                }
                TenantAction::Suspend { id, reason } => {
                    tenant::lifecycle(
                        &cli.env,
                        &id,
                        tenant::LifecycleCommand::Suspend,
                        reason,
                        ids.clone(),
                    )
                    .await
                }
                TenantAction::Resume { id } => {
                    tenant::lifecycle(
                        &cli.env,
                        &id,
                        tenant::LifecycleCommand::Resume,
                        None,
                        ids.clone(),
                    )
                    .await
                }
                TenantAction::Archive { id } => {
                    tenant::lifecycle(
                        &cli.env,
                        &id,
                        tenant::LifecycleCommand::Archive,
                        None,
                        ids.clone(),
                    )
                    .await
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
            match access::seed(&cli.env, kind, ids.clone()).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("seed failed: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Feature { action } => {
            let result = match action {
                FeatureAction::List => control::feature_list(&cli.env, ids.clone()).await,
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
                        ids.clone(),
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
                        ids.clone(),
                    )
                    .await
                }
                EntitlementAction::Revoke { tenant, feature } => {
                    control::entitlement_revoke(&cli.env, &tenant, &feature, ids.clone()).await
                }
                EntitlementAction::List { tenant } => {
                    control::entitlement_list(&cli.env, &tenant, ids.clone()).await
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
        Command::RoleList => match control::role_list(&cli.env, ids.clone()).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("role list failed: {err:#}");
                ExitCode::FAILURE
            }
        },
        Command::Impersonation { action } => {
            let result = match action {
                ImpersonationAction::List { tenant } => {
                    control::impersonation_list(&cli.env, tenant.as_deref(), ids.clone()).await
                }
                ImpersonationAction::Revoke { id } => {
                    control::impersonation_revoke(&cli.env, id, ids.clone()).await
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
                AuditAction::Verify => control::audit_verify(&cli.env, ids.clone()).await,
                AuditAction::List {
                    tenant_id,
                    action,
                    limit,
                } => control::audit_list(&cli.env, tenant_id, action, limit, ids.clone()).await,
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
                ApprovalAction::List => control::approval_list(&cli.env, ids.clone()).await,
                ApprovalAction::Approve { id } => {
                    control::approval_decide(&cli.env, id, true, ids.clone()).await
                }
                ApprovalAction::Reject { id } => {
                    control::approval_decide(&cli.env, id, false, ids.clone()).await
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
        Command::Billing { action } => {
            let result = match action {
                BillingAction::Plans => billing::plans(&cli.env, ids.clone()).await,
                BillingAction::TaxCheck => billing::tax_check(&cli.env, ids.clone()).await,
                BillingAction::Reconcile => billing::reconcile(&cli.env, ids.clone()).await,
                BillingAction::Webhooks => billing::webhooks(&cli.env, ids.clone()).await,
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("billing command failed: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Payments { action } => {
            let result = match action {
                PaymentsAction::Webhooks => payments::webhooks(&cli.env, ids.clone()).await,
                PaymentsAction::Replay { event_id } => {
                    payments::replay(&cli.env, &event_id, ids.clone()).await
                }
                PaymentsAction::Sync => payments::sync(&cli.env, ids.clone()).await,
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("payments command failed: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Dlq { action } => {
            let result = match action {
                DlqAction::List { limit } => dlq::list(&cli.env, limit).await,
                DlqAction::Replay { event_id } => {
                    dlq::replay(&cli.env, event_id, ids.clone()).await
                }
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("dlq command failed: {err:#}");
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
                ids.clone(),
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
        Command::TenantRestore {
            tenant,
            from_dsn,
            dry_run,
        } => match tenant_restore::run(
            &cli.env,
            tenant_restore::TenantRestoreArgs {
                tenant_slug: tenant,
                from_dsn,
                dry_run,
            },
            ids.clone(),
        )
        .await
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("tenant-restore failed: {err:#}");
                ExitCode::FAILURE
            }
        },
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
        Command::I18n { action } => {
            let action = match action {
                I18nAction::Extract => i18n::I18nAction::Extract,
                I18nAction::Check => i18n::I18nAction::Check,
                I18nAction::Report => i18n::I18nAction::Report,
            };
            match i18n::run(action) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("i18n check failed: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Theme { action } => {
            let result = match action {
                ThemeAction::Publish {
                    dir,
                    publisher,
                    visibility,
                    required_feature,
                    allow_contrast_warnings,
                } => {
                    theme::publish(
                        &cli.env,
                        &dir,
                        &publisher,
                        &visibility,
                        required_feature.as_deref(),
                        allow_contrast_warnings,
                        ids.clone(),
                    )
                    .await
                }
                ThemeAction::List => theme::list(&cli.env).await,
                ThemeAction::Versions { key } => theme::versions(&cli.env, &key).await,
                ThemeAction::Yank { key, version } => {
                    theme::yank(&cli.env, &key, &version, ids.clone()).await
                }
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("theme command failed: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Archive {
            table,
            tenant,
            from,
            until,
            dry_run,
            user,
            password,
        } => match archive::run_archive(
            &cli.env,
            archive::ArchiveArgs {
                table,
                tenant,
                from,
                until,
                dry_run,
                user,
                password,
            },
        )
        .await
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("archive failed: {err:#}");
                ExitCode::FAILURE
            }
        },
        Command::ArchiveRestore {
            id,
            dry_run,
            user,
            password,
        } => match archive::run_archive_restore(
            &cli.env,
            archive::ArchiveRestoreArgs {
                id,
                dry_run,
                user,
                password,
            },
        )
        .await
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("archive-restore failed: {err:#}");
                ExitCode::FAILURE
            }
        },
        Command::Offboard {
            tenant,
            key,
            stage,
            confirm,
            dry_run,
            neighbour,
            user,
            password,
        } => match offboard::run(
            &cli.env,
            offboard::OffboardArgs {
                tenant,
                key,
                stage,
                confirm,
                dry_run,
                neighbour,
                user,
                password,
            },
        )
        .await
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("offboard failed: {err:#}");
                ExitCode::FAILURE
            }
        },
        Command::RetentionReport { write } => match retention_report::run(&cli.env, write).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("retention-report failed: {err:#}");
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
        Command::SeedScale { seed, dry_run } => {
            match seed_scale::run(&cli.env, seed, dry_run, ids.clone()).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("seed-scale failed: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
