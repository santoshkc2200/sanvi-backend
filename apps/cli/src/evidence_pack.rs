//! TASK-029 (FR-1132/FR-1133/FR-1135): the evidence pack generator.
//!
//! One pack per jurisdiction profile in the **live configuration**, each
//! stamped with the generating build sha and the period it covers. A pack
//! is generated, never authored: Art. 30-style records of processing from
//! the phase-05 data map, the high-risk assessments phases 09/10 added,
//! the sub-processor list with each party's role and transfer mechanism,
//! retention rules as published *and* as executed (from TASK-028's
//! per-class ledger so the two columns can be compared), and the DSR and
//! appeal counters from the request ledger.
//!
//! Determinism is the point: the same configuration and period produce a
//! byte-identical pack. The pack therefore embeds no wall clock — only the
//! period bounds, the build stamp, and facts read from the database.

use std::sync::Arc;

use anyhow::Context;
use chrono::{Datelike, NaiveDate};
use serde::Serialize;
use sha2::{Digest, Sha256};

const PACK_VERSION: u32 = 1;

/// The build stamp every pack carries: a pack that cannot say which build
/// produced it is not evidence.
pub fn build_stamp() -> BuildStamp {
    BuildStamp {
        version: env!("CARGO_PKG_VERSION").to_string(),
        sha: option_env!("GIT_COMMIT")
            .unwrap_or("uncommitted")
            .to_string(),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildStamp {
    pub version: String,
    pub sha: String,
}

/// The covered period: a calendar quarter, bounds inclusive.
#[derive(Debug, Clone, Serialize)]
pub struct Period {
    pub label: String,
    pub year: i32,
    pub quarter: u32,
    pub from: NaiveDate,
    pub to: NaiveDate,
}

/// `last-quarter` resolves against the current wall clock; `YYYY-QN` is
/// explicit (the byte-identical runs use the explicit form).
pub fn resolve_period(input: &str) -> anyhow::Result<Period> {
    let (year, quarter) = if input.eq_ignore_ascii_case("last-quarter") {
        let today = chrono::Utc::now().date_naive();
        let (y, q) = (today.year(), today.month0() / 3);
        if q == 1 { (y - 1, 4u32) } else { (y, q - 1) }
    } else {
        let input = input.trim().to_ascii_uppercase();
        let Some((year_part, quarter_part)) = input.split_once("-Q") else {
            anyhow::bail!(
                "period must be `last-quarter` or `YYYY-QN` (e.g. 2026-Q3), got `{input}`"
            );
        };
        let year: i32 = year_part.parse().context("period year")?;
        let quarter: u32 = quarter_part.parse().context("period quarter")?;
        if !(1..=4).contains(&quarter) {
            anyhow::bail!("period quarter must be 1..=4");
        }
        (year, quarter)
    };
    let from = NaiveDate::from_ymd_opt(year, quarter * 3 - 2, 1).context("period start")?;
    let to = if quarter == 4 {
        NaiveDate::from_ymd_opt(year, 12, 31).context("period end")?
    } else {
        NaiveDate::from_ymd_opt(year, quarter * 3 + 1, 1)
            .context("period end")?
            .pred_opt()
            .context("period end")?
    };
    Ok(Period {
        label: format!("{year}-Q{quarter}"),
        year,
        quarter,
        from,
        to,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct EvidencePack {
    pub pack_version: u32,
    pub build: BuildStamp,
    pub period: PeriodRef,
    pub jurisdiction: JurisdictionSection,
    pub records_of_processing: Vec<ProcessingRecord>,
    pub risk_assessments: Vec<RiskAssessmentRecord>,
    pub sub_processors: Vec<SubProcessorRecord>,
    pub retention: RetentionSection,
    pub dsr_counters: Vec<DsrCounterRow>,
    pub appeal_counters: AppealCounters,
}

#[derive(Debug, Clone, Serialize)]
pub struct PeriodRef {
    pub label: String,
    pub from: NaiveDate,
    pub to: NaiveDate,
}

#[derive(Debug, Clone, Serialize)]
pub struct JurisdictionSection {
    pub code: String,
    pub regime: String,
    pub consent_model: String,
    pub sensitive_model: String,
    pub response_days: i32,
    pub extension_days: i32,
    pub ack_days: Option<i32>,
    pub opt_out_effective_business_days: Option<i32>,
    pub appeal_window_days: Option<i32>,
    pub appeal_response_days: Option<i32>,
    pub honours_universal_opt_out: bool,
    pub risk_assessment_required: bool,
    pub breach_rules: serde_json::Value,
    pub authority: serde_json::Value,
    pub effective_from: NaiveDate,
    pub source_note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessingRecord {
    pub name: String,
    pub purpose: String,
    pub data_classes: Vec<String>,
    pub controller_role: String,
    pub basis: Option<String>,
    pub version: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct RiskAssessmentRecord {
    pub activity: String,
    pub jurisdictions: Vec<String>,
    pub trigger: String,
    pub status: String,
    pub reviewed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub next_review_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubProcessorRecord {
    pub name: String,
    pub purpose: String,
    pub location: String,
    pub role: String,
    pub dpa_url: Option<String>,
    /// The registry's stored mechanism and whether it is the one the
    /// jurisdiction's regime expects. A gap is surfaced here, not smoothed
    /// over: `adequate_for_regime: false` is a defect in the registry.
    pub transfer_mechanism: SubProcessorTransfer,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubProcessorTransfer {
    pub stored: Option<String>,
    pub regime_expects: &'static str,
    pub adequate_for_regime: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetentionSection {
    /// The published rules (the notice's source).
    pub published: Vec<PublishedRule>,
    /// The runs actually recorded, per class (TASK-028's ledger).
    pub executed: Vec<ExecutedClass>,
    pub staleness_window_hours: i64,
    pub stalled_classes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PublishedRule {
    pub data_class: String,
    pub sensitivity: String,
    pub basis: Option<String>,
    pub period_days: i64,
    pub action: String,
    pub disclosed_in_notice: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecutedClass {
    pub data_class: String,
    pub enforcement: String,
    pub period_days: i64,
    pub action: String,
    pub last_run: Option<crate::retention_report::LastRun>,
    pub oldest_surviving_row: Option<chrono::DateTime<chrono::Utc>>,
    pub stalled: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DsrCounterRow {
    pub kind: String,
    pub received: i32,
    pub complied: i32,
    pub denied: i32,
    pub median_days: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AppealCounters {
    pub received: i64,
    pub upheld: i64,
    pub denied: i64,
    pub open: i64,
    pub median_decision_days: Option<f64>,
}

/// The transfer mechanism each regime expects; a stored mechanism that
/// differs is a registry gap the pack surfaces.
fn regime_expects_transfer(regime: &str) -> &'static str {
    match regime {
        "gdpr" => "sccs",
        "uk_gdpr" => "uk_addendum",
        "appi" => "appi_equivalent",
        "us_state" => "dpf",
        _ => "undocumented",
    }
}

/// Build one pack per jurisdiction in the live configuration (or only
/// `only`, when named). Everything is read through one pool; ordering is
/// canonical so two runs against the same data are byte-identical.
pub async fn build_packs(
    pool: &sqlx::PgPool,
    period: &Period,
    only: Option<&str>,
) -> anyhow::Result<Vec<EvidencePack>> {
    let build = build_stamp();
    let tx = sanvi_persistence::TxManager::new(
        pool.clone(),
        Arc::new(sanvi_kernel::SnowflakeGenerator::new(
            sanvi_kernel::NodeId::new(24)?,
        )),
    );
    let mut scope = tx
        .begin_unscoped("evidence_pack", "cli:evidence-pack")
        .await?;
    let conn = &mut **scope.tx_mut().as_inner();

    type ProfileRow = (
        String,
        String,
        String,
        String,
        i16,
        i16,
        Option<i16>,
        Option<i16>,
        Option<i16>,
        Option<i16>,
        bool,
        bool,
        serde_json::Value,
        serde_json::Value,
        NaiveDate,
        Option<String>,
    );
    let profiles: Vec<ProfileRow> = sqlx::query_as(
        "SELECT code, regime, consent_model, sensitive_model, response_days, extension_days,
                ack_days, opt_out_effective_business_days, appeal_window_days, appeal_response_days,
                honours_universal_opt_out, risk_assessment_required, breach_rules, authority,
                effective_from, source_note
         FROM privacy.jurisdiction_profiles ORDER BY code",
    )
    .fetch_all(&mut *conn)
    .await?;

    let activities: Vec<(String, String, Vec<String>, String, Option<String>, i32)> =
        sqlx::query_as(
            "SELECT name, purpose, data_classes, controller_role, basis, version
             FROM privacy.processing_activities WHERE active ORDER BY name, version",
        )
        .fetch_all(&mut *conn)
        .await?;

    type AssessmentRow = (
        String,
        Vec<String>,
        String,
        String,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    );
    let assessments: Vec<AssessmentRow> = sqlx::query_as(
            "SELECT a.name, r.jurisdictions, r.trigger::text, r.status::text, r.reviewed_at, r.next_review_at
             FROM privacy.risk_assessments r
             JOIN privacy.processing_activities a ON a.id = r.activity_id
             WHERE r.status <> 'superseded'
             ORDER BY a.name, r.trigger",
        )
        .fetch_all(&mut *conn)
        .await?;

    let processors: Vec<(
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT name, purpose, location, role, dpa_url, transfer_mechanism
             FROM privacy.subprocessors WHERE removed_at IS NULL ORDER BY name",
    )
    .fetch_all(&mut *conn)
    .await?;

    let rules: Vec<(String, String, Option<String>, i64, String, bool)> = sqlx::query_as(
        "SELECT data_class, sensitivity, basis::text,
                (EXTRACT(EPOCH FROM period) / 86400)::bigint AS days, action, disclosed_in_notice
         FROM privacy.retention_rules ORDER BY data_class",
    )
    .fetch_all(&mut *conn)
    .await?;

    let metrics: Vec<(String, String, i32, i32, i32, Option<f64>)> = sqlx::query_as(
        "SELECT jurisdiction, kind, received, complied, denied, median_days
         FROM privacy.request_metrics_yearly WHERE year = $1
         ORDER BY jurisdiction, kind",
    )
    .bind(period.year)
    .fetch_all(&mut *conn)
    .await?;

    scope.commit().await?;

    // TASK-028's per-class executed report (the pack drops the report's own
    // generation timestamp — a pack carries the period, never a clock).
    let executed_report = crate::retention_report::build(pool).await?;

    let mut packs = Vec::new();
    for (
        code,
        regime,
        consent_model,
        sensitive_model,
        response_days,
        extension_days,
        ack_days,
        opt_out_days,
        appeal_window,
        appeal_response,
        honours_uoo,
        ra_required,
        breach_rules,
        authority,
        effective_from,
        source_note,
    ) in profiles
    {
        if let Some(only) = only
            && only != code
        {
            continue;
        }
        let expects = regime_expects_transfer(&regime);
        let jurisdiction = JurisdictionSection {
            code: code.clone(),
            regime,
            consent_model,
            sensitive_model,
            response_days: i32::from(response_days),
            extension_days: i32::from(extension_days),
            ack_days: ack_days.map(i32::from),
            opt_out_effective_business_days: opt_out_days.map(i32::from),
            appeal_window_days: appeal_window.map(i32::from),
            appeal_response_days: appeal_response.map(i32::from),
            honours_universal_opt_out: honours_uoo,
            risk_assessment_required: ra_required,
            breach_rules,
            authority,
            effective_from,
            source_note,
        };

        let records_of_processing = activities
            .iter()
            .map(
                |(name, purpose, data_classes, controller_role, basis, version)| ProcessingRecord {
                    name: name.clone(),
                    purpose: purpose.clone(),
                    data_classes: {
                        let mut classes = data_classes.clone();
                        classes.sort();
                        classes
                    },
                    controller_role: controller_role.clone(),
                    basis: basis.clone(),
                    version: *version,
                },
            )
            .collect();

        let risk_assessments = assessments
            .iter()
            .map(
                |(activity, jurisdictions, trigger, status, reviewed_at, next_review_at)| {
                    RiskAssessmentRecord {
                        activity: activity.clone(),
                        jurisdictions: {
                            let mut js = jurisdictions.clone();
                            js.sort();
                            js
                        },
                        trigger: trigger.clone(),
                        status: status.clone(),
                        reviewed_at: *reviewed_at,
                        next_review_at: *next_review_at,
                    }
                },
            )
            .collect();

        let sub_processors = processors
            .iter()
            .map(
                |(name, purpose, location, role, dpa_url, transfer_mechanism)| SubProcessorRecord {
                    name: name.clone(),
                    purpose: purpose.clone(),
                    location: location.clone(),
                    role: role.clone(),
                    dpa_url: dpa_url.clone(),
                    transfer_mechanism: SubProcessorTransfer {
                        stored: transfer_mechanism.clone(),
                        regime_expects: expects,
                        adequate_for_regime: transfer_mechanism
                            .as_deref()
                            .is_some_and(|m| m == expects),
                    },
                },
            )
            .collect();

        let published = rules
            .iter()
            .map(
                |(data_class, sensitivity, basis, period_days, action, disclosed)| PublishedRule {
                    data_class: data_class.clone(),
                    sensitivity: sensitivity.clone(),
                    basis: basis.clone(),
                    period_days: *period_days,
                    action: action.clone(),
                    disclosed_in_notice: *disclosed,
                },
            )
            .collect();

        let executed = executed_report
            .classes
            .iter()
            .map(|class| ExecutedClass {
                data_class: class.data_class.clone(),
                enforcement: class.enforcement.clone(),
                period_days: class.period_days,
                action: class.action.clone(),
                last_run: class.last_run.clone(),
                oldest_surviving_row: class.oldest_surviving_row,
                stalled: class.stalled,
            })
            .collect();

        let dsr_counters = metrics
            .iter()
            .filter(|(jurisdiction, _, _, _, _, _)| jurisdiction == &code)
            .map(
                |(_, kind, received, complied, denied, median_days)| DsrCounterRow {
                    kind: kind.clone(),
                    received: *received,
                    complied: *complied,
                    denied: *denied,
                    median_days: *median_days,
                },
            )
            .collect();

        packs.push(EvidencePack {
            pack_version: PACK_VERSION,
            build: build.clone(),
            period: PeriodRef {
                label: period.label.clone(),
                from: period.from,
                to: period.to,
            },
            jurisdiction,
            records_of_processing,
            risk_assessments,
            sub_processors,
            retention: RetentionSection {
                published,
                executed,
                staleness_window_hours: executed_report.staleness_window_hours,
                stalled_classes: executed_report.stalled_classes.clone(),
            },
            dsr_counters,
            appeal_counters: appeal_counters(pool, &code, period).await?,
        });
    }
    Ok(packs)
}

/// Appeal counters for one jurisdiction inside the period, read from the
/// appeals ledger joined to the request's jurisdiction.
async fn appeal_counters(
    pool: &sqlx::PgPool,
    jurisdiction: &str,
    period: &Period,
) -> anyhow::Result<AppealCounters> {
    let mut counters = AppealCounters::default();
    let rows: Vec<(
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        "SELECT a.outcome, a.decided_at, a.received_at
             FROM privacy.request_appeals a
             JOIN privacy.data_subject_requests r ON r.id = a.request_id
             WHERE r.jurisdiction = $1
               AND a.received_at >= $2 AND a.received_at < $3",
    )
    .bind(jurisdiction)
    .bind(period.from.and_hms_opt(0, 0, 0).context("period start")?)
    .bind(
        period
            .to
            .succ_opt()
            .context("period end")?
            .and_hms_opt(0, 0, 0)
            .context("period end")?,
    )
    .fetch_all(pool)
    .await?;
    counters.received = rows.len() as i64;
    let mut decision_days: Vec<f64> = Vec::new();
    for (outcome, decided_at, received_at) in rows {
        match outcome.as_deref() {
            Some("upheld") => counters.upheld += 1,
            Some("denied") => counters.denied += 1,
            _ => {}
        }
        if decided_at.is_none() {
            counters.open += 1;
        }
        if let Some(decided_at) = decided_at {
            decision_days.push((decided_at - received_at).num_milliseconds() as f64 / 86_400_000.0);
        }
    }
    if !decision_days.is_empty() {
        decision_days.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        counters.median_decision_days = Some(decision_days[decision_days.len() / 2]);
    }
    Ok(counters)
}

/// Render the packs to `out/<label>/`, one file per jurisdiction plus a
/// checksum manifest, and return the manifest path.
pub fn write_packs(
    out_root: &std::path::Path,
    packs: &[EvidencePack],
) -> anyhow::Result<std::path::PathBuf> {
    let dir = out_root.join(&packs[0].period.label);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let mut files = Vec::new();
    for pack in packs {
        let name = format!("evidence-{}.json", pack.jurisdiction.code);
        let json = serde_json::to_string_pretty(pack)?;
        let path = dir.join(&name);
        std::fs::write(&path, format!("{json}\n"))?;
        files.push((name, sha256_hex(json.as_bytes())));
    }
    // The manifest: per-file sha256 so a hand-edited pack is detectable —
    // editing a generated pack destroys the only property that made it
    // worth generating.
    let manifest = serde_json::json!({
        "pack_version": PACK_VERSION,
        "period": packs.first().map(|p| &p.period),
        "build": packs.first().map(|p| &p.build),
        "files": files,
    });
    let manifest_path = dir.join("manifest.json");
    std::fs::write(
        &manifest_path,
        format!("{}\n", serde_json::to_string_pretty(&manifest)?),
    )?;
    Ok(manifest_path)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub async fn run(
    environment: &str,
    jurisdiction: String,
    period: String,
    out: Option<String>,
) -> anyhow::Result<()> {
    let cli_config = crate::config::load(environment)?;
    let pool = sanvi_persistence::build_pool(&cli_config.database).await?;
    let period = resolve_period(&period)?;
    let only = if jurisdiction == "all" {
        None
    } else {
        Some(jurisdiction.as_str())
    };
    let packs = build_packs(&pool, &period, only).await?;
    if packs.is_empty() {
        anyhow::bail!("no jurisdiction profile matched `{jurisdiction}`");
    }
    let out_root =
        std::path::PathBuf::from(out.unwrap_or_else(|| "docs/compliance/evidence".to_string()));
    let manifest = write_packs(&out_root, &packs)?;
    for pack in &packs {
        println!(
            "evidence-pack: {} {} — {} processing record(s), {} sub-processor(s), {} retention class(es), {} DSR counter row(s)",
            pack.jurisdiction.code,
            pack.period.label,
            pack.records_of_processing.len(),
            pack.sub_processors.len(),
            pack.retention.executed.len(),
            pack.dsr_counters.len(),
        );
    }
    println!("evidence-pack: manifest at {}", manifest.display());
    Ok(())
}
