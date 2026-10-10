//! TASK-029 (FR-1132): the DSR statutory-clock drills.
//!
//! For every jurisdiction in the **live configuration**, run access,
//! erasure, US opt-out and (where the regime provides one) the appeal
//! path end to end over the real provider registry, and time each against
//! that jurisdiction's *statutory* deadline — not the internal SLA. An
//! internal 14-day target tells you nothing about the jurisdiction with a
//! 15-day one; this drill reads the deadline from the profile.
//!
//! Every measured duration is written to the committed evidence directory;
//! a drill that cannot complete, or that lands outside its deadline,
//! fails the command.

use std::sync::Arc;

use serde::Serialize;

use sanvi_api::compliance::{ComplianceHarness, DrillKind, DrillTiming, run_dsr_drill};
use sanvi_kernel::{RawIdGenerator, SnowflakeGenerator};

#[derive(Debug, Serialize)]
struct DsrCheckReport {
    run_at: chrono::DateTime<chrono::Utc>,
    environment: String,
    /// The generating build sha — the timings are evidence too.
    build_sha: String,
    jurisdictions: usize,
    drills: Vec<DrillTiming>,
    breaches: Vec<String>,
}

pub async fn run(environment: &str, jurisdiction: String) -> anyhow::Result<()> {
    let cli_config = crate::config::load(environment)?;
    let pool = sanvi_persistence::build_pool(&cli_config.database).await?;
    let ids: Arc<dyn RawIdGenerator> = Arc::new(SnowflakeGenerator::new(
        cli_config.ids.resolve(environment, 3)?,
    ));
    let harness = ComplianceHarness::build(pool, ids);

    let codes: Vec<String> = if jurisdiction == "all" {
        harness
            .profiles()
            .await?
            .into_iter()
            .map(|p| p.code.as_str().to_string())
            .collect()
    } else {
        // Fails loudly when the code is not in the live configuration.
        harness.profile(&jurisdiction).await?;
        vec![jurisdiction.clone()]
    };

    let mut drills = Vec::new();
    for code in &codes {
        for kind in [
            DrillKind::Access,
            DrillKind::Erasure,
            DrillKind::OptOut,
            DrillKind::Appeal,
        ] {
            let timing = run_dsr_drill(&harness, code, kind).await?;
            println!(
                "dsr-check: {:>6} {:>8} — {:>6} ms (deadline: {})",
                timing.jurisdiction, timing.drill, timing.measured_ms, timing.deadline_source,
            );
            drills.push(timing);
        }
    }

    let breaches: Vec<String> = drills
        .iter()
        .filter(|t| !t.within_deadline)
        .map(|t| {
            format!(
                "{} {}: {} ms exceeds {}",
                t.jurisdiction, t.drill, t.measured_ms, t.deadline_source
            )
        })
        .collect();

    let report = DsrCheckReport {
        run_at: chrono::Utc::now(),
        environment: environment.to_string(),
        build_sha: option_env!("GIT_COMMIT")
            .unwrap_or("uncommitted")
            .to_string(),
        jurisdictions: codes.len(),
        drills,
        breaches: breaches.clone(),
    };

    // The measured durations are committed evidence: docs/compliance/
    // evidence/dsr-timings/, one file per run.
    let dir = std::path::PathBuf::from("docs/compliance/evidence/dsr-timings");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        "dsr-timings-{}-{}.json",
        environment,
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    println!("dsr-check: timings written to {}", path.display());

    if !breaches.is_empty() {
        for breach in &breaches {
            eprintln!("dsr-check: STATUTORY BREACH: {breach}");
        }
        anyhow::bail!(
            "{} drill(s) outside their statutory deadline",
            breaches.len()
        );
    }
    Ok(())
}
