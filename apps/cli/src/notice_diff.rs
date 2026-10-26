//! TASK-029 (FR-1133): the notice diff.
//!
//! The generators produce the current notice text from live configuration;
//! the diff against the last approved version is produced *by the
//! generator*, not by someone reading two documents side by side. The
//! approval step itself waits on counsel (parked in
//! `docs/release/needs-humans.md`); the diff does not.
//!
//! - `notice-diff` — renders the current notice canonically and prints the
//!   unified diff against `docs/compliance/evidence/notice/approved.json`.
//!   Exit status 1 when they differ (CI can gate on it), 2 when no
//!   approved version is stored yet.
//! - `notice-diff --write-approved` — records the current render as the
//!   approved baseline. That is a human decision expressed as a command,
//!   never a hand edit: the file is the generator's own output.

use std::collections::BTreeMap;

use anyhow::Context;
use serde::Serialize;

use sanvi_persistence::TxManager;
use sanvi_privacy::adapters::{
    SqlxJurisdictionRepository, SqlxRetentionRuleRepository, SqlxSubProcessorRepository,
};
use sanvi_privacy::application::NoticeService;

/// The canonical notice: the same facts the public endpoint serves, in a
/// deterministic shape (maps, sorted vectors, no timestamps).
#[derive(Debug, Clone, Serialize)]
pub struct CanonicalNotice {
    pub disclosures: sanvi_privacy::config::PrivacyDisclosures,
    pub notice_version: String,
    pub backup_retention_days: i64,
    pub jurisdictions: BTreeMap<String, CanonicalJurisdiction>,
    pub retention: Vec<CanonicalRetentionRow>,
    pub subprocessors: BTreeMap<String, CanonicalSubProcessor>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CanonicalJurisdiction {
    pub regime: String,
    pub consent_model: String,
    pub response_days: u16,
    pub appeal_window_days: Option<u16>,
    pub honours_universal_opt_out: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CanonicalRetentionRow {
    pub category: String,
    pub period_days: i64,
    pub action: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CanonicalSubProcessor {
    pub purpose: String,
    pub location: String,
    pub role: String,
    pub transfer_mechanism: Option<String>,
}

pub async fn render_canonical_notice(
    pool: &sqlx::PgPool,
    notice_version: &str,
    backup_retention_days: i64,
    disclosures: &sanvi_privacy::config::PrivacyDisclosures,
) -> anyhow::Result<CanonicalNotice> {
    let tx = TxManager::new(
        pool.clone(),
        std::sync::Arc::new(sanvi_kernel::SnowflakeGenerator::new(
            sanvi_kernel::NodeId::new(24)?,
        )),
    );
    let service = NoticeService::new(
        tx.clone(),
        std::sync::Arc::new(SqlxRetentionRuleRepository::new()),
        std::sync::Arc::new(SqlxSubProcessorRepository::new()),
        std::sync::Arc::new(SqlxJurisdictionRepository::new()),
    )
    .with_disclosures(disclosures.clone());
    let notice = service
        .build(notice_version, backup_retention_days)
        .await
        .map_err(|err| anyhow::anyhow!("notice build: {err}"))?;

    let mut jurisdictions = BTreeMap::new();
    for j in &notice.jurisdictions {
        jurisdictions.insert(
            j.code.clone(),
            CanonicalJurisdiction {
                regime: j.regime.clone(),
                consent_model: j.consent_model.clone(),
                response_days: j.response_days,
                appeal_window_days: j.appeal_window_days,
                honours_universal_opt_out: j.honours_universal_opt_out,
            },
        );
    }
    let mut retention: Vec<CanonicalRetentionRow> = notice
        .retention
        .iter()
        .map(|row| CanonicalRetentionRow {
            category: row.category.clone(),
            period_days: row.period_days,
            action: row.action.clone(),
        })
        .collect();
    retention.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then(a.period_days.cmp(&b.period_days))
            .then(a.action.cmp(&b.action))
    });

    let mut subprocessors = BTreeMap::new();
    for s in &notice.subprocessors {
        subprocessors.insert(
            s.name.clone(),
            CanonicalSubProcessor {
                purpose: s.purpose.clone(),
                location: s.location.clone(),
                role: s.role.clone(),
                transfer_mechanism: s.transfer_mechanism.clone(),
            },
        );
    }

    Ok(CanonicalNotice {
        disclosures: notice.disclosures,
        notice_version: notice.notice_version,
        backup_retention_days: notice.backup_retention_days,
        jurisdictions,
        retention,
        subprocessors,
    })
}

/// Render deterministically: map ordering is serde_json's (sorted for
/// BTreeMap), pretty-printed with a trailing newline.
pub fn render_text(notice: &CanonicalNotice) -> anyhow::Result<String> {
    Ok(format!("{}\n", serde_json::to_string_pretty(notice)?))
}

/// A minimal unified diff over lines (LCS), stable for identical inputs.
pub fn unified_diff(before: &str, after: &str, before_label: &str, after_label: &str) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let mut out = format!("--- {before_label}\n+++ {after_label}\n");
    if a == b {
        return out;
    }
    // LCS table (the documents are small: notice text, not logs).
    let n = a.len();
    let m = b.len();
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut hunks: Vec<String> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    let mut removals = 0usize;
    let mut additions = 0usize;
    while i < n && j < m {
        if a[i] == b[j] {
            hunks.push(format!(" {}", a[i]));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            hunks.push(format!("-{}", a[i]));
            removals += 1;
            i += 1;
        } else {
            hunks.push(format!("+{}", b[j]));
            additions += 1;
            j += 1;
        }
    }
    while i < n {
        hunks.push(format!("-{}", a[i]));
        removals += 1;
        i += 1;
    }
    while j < m {
        hunks.push(format!("+{}", b[j]));
        additions += 1;
        j += 1;
    }
    out.push_str(&format!(
        "@@ -1,{n} +1,{m} @@ ({removals} removed, {additions} added)\n"
    ));
    for line in hunks {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub async fn run(
    environment: &str,
    approved: Option<String>,
    write_approved: bool,
) -> anyhow::Result<()> {
    let cli_config = crate::config::load(environment)?;
    let pool = sanvi_persistence::build_pool(&cli_config.database).await?;
    let current = render_canonical_notice(
        &pool,
        &cli_config.privacy.notice_version,
        cli_config.privacy.backup_retention_days,
        &cli_config.privacy.disclosures,
    )
    .await?;
    let current_text = render_text(&current)?;

    let approved_path =
        approved.unwrap_or_else(|| "docs/compliance/evidence/notice/approved.json".to_string());

    if write_approved {
        if std::path::Path::new(&approved_path).exists() {
            anyhow::bail!(
                "{approved_path} already exists — re-approval is counsel's decision, recorded in git; refusing to overwrite"
            );
        }
        if let Some(parent) = std::path::Path::new(&approved_path).parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&approved_path, &current_text)?;
        println!("notice-diff: approved baseline written to {approved_path}");
        return Ok(());
    }

    let Ok(baseline_text) = std::fs::read_to_string(&approved_path) else {
        eprintln!(
            "notice-diff: no approved baseline at {approved_path} — run `just notice-diff -- --write-approved` once counsel has approved the current text"
        );
        print!("{current_text}");
        std::process::exit(2);
    };

    let diff = unified_diff(
        &baseline_text,
        &current_text,
        &format!("{approved_path} (approved)"),
        &format!("{approved_path} (current, generated from live configuration)"),
    );
    if baseline_text == current_text {
        println!("notice-diff: no drift — the live notice matches the approved baseline");
        Ok(())
    } else {
        print!("{diff}");
        eprintln!(
            "notice-diff: the generated notice drifted from the approved baseline — counsel review required before release"
        );
        std::process::exit(1);
    }
}
