//! TASK-029 (11.10 Step 3): the DSR paths asserted against the statutory
//! clocks. Every seeded jurisdiction's profile is read from the live
//! configuration and each of its four paths — access, erasure, US opt-out
//! (GPC-sourced) and appeal — runs end to end over the real provider
//! registry. The GPC-on-a-tenant-custom-domain first-request drill lives
//! beside the assembled HTTP app (`apps/api` integration, gpc_storefront);
//! everything else runs here, per jurisdiction, against its own clock.

use sanvi_api::compliance::{ComplianceHarness, DrillKind, run_dsr_drill};
use sanvi_kernel::{NodeId, RawIdGenerator, SnowflakeGenerator};
use std::sync::Arc;

fn ids() -> Arc<dyn RawIdGenerator> {
    Arc::new(SnowflakeGenerator::new(NodeId::new(79).expect("node id")))
}

/// Every jurisdiction in the live configuration, every path, each measured
/// against its own statutory deadline. A path that cannot complete is a
/// failure here — "slow" is not a status a broken erasure path gets.
#[tokio::test]
async fn every_dsr_path_completes_inside_each_jurisdictions_statutory_deadline() {
    let db = sanvi_testsupport::test_db().await;
    let harness = ComplianceHarness::build(db.app_pool().clone(), ids());

    let profiles = harness.profiles().await.expect("live profiles");
    assert!(
        profiles.len() >= 9,
        "expected every seeded jurisdiction to be in the live configuration; got {profiles:?}"
    );

    let mut timings = Vec::new();
    for profile in &profiles {
        for kind in [
            DrillKind::Access,
            DrillKind::Erasure,
            DrillKind::OptOut,
            DrillKind::Appeal,
        ] {
            let timing = run_dsr_drill(&harness, profile.code.as_str(), kind)
                .await
                .unwrap_or_else(|err| {
                    panic!("drill {} {:?} failed: {err}", profile.code.as_str(), kind)
                });
            assert!(
                timing.within_deadline,
                "{} {:?}: {} ms against {}",
                profile.code.as_str(),
                kind,
                timing.measured_ms,
                timing.deadline_source
            );
            timings.push(timing);
        }
    }

    // The committed record: the measured durations land next to the other
    // compliance evidence so a run's numbers are comparable over time.
    let out_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("compliance")
        .to_path_buf();
    let _ = std::fs::create_dir_all(&out_dir);
    let _ = std::fs::write(
        out_dir.join("dsr-paths-timings.json"),
        format!("{}\n", serde_json::to_string_pretty(&timings).unwrap()),
    );

    // The appeal drill is only skipped where the profile grants no appeal
    // right; count the executed ones so a silent regression to "skip
    // everywhere" cannot pass.
    let executed_appeals = timings
        .iter()
        .filter(|t| {
            t.drill == "appeal" && t.detail.get("not_applicable") != Some(&serde_json::json!(true))
        })
        .count();
    assert!(
        executed_appeals >= 4,
        "expected the US appeal drills to execute; only {executed_appeals} did"
    );
}
