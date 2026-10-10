//! TASK-029 (11.10 Step 1): the provider completeness proof.
//!
//! The most likely real-world defect is a context that quietly never
//! joined — payments arrived in phase 09 and advertising in phase 10,
//! both after the privacy centre was written. Three layers catch it:
//!
//! 1. *Static*: the workspace's context crates are enumerated from the
//!    root manifest and must equal `CONTEXT_NAMES` — a new context crate
//!    without a provider name fails here.
//! 2. *Composition*: `build_privacy_state` asserts the registry equals
//!    `CONTEXT_NAMES` exactly — this test proves the assert fires (a
//!    context registered in neither path is a panic at boot, in every
//!    consumer of the shared builder).
//! 3. *End to end*: a real access request and a real erasure request run
//!    the saga over the real fifteen-provider registry; both complete only
//!    when every context reaches a terminal participant state on *that*
//!    path — a provider missing from either path is a failed drill.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use sanvi_api::compliance::{ComplianceHarness, DrillKind, run_dsr_drill};
use sanvi_kernel::{NodeId, RawIdGenerator, SnowflakeGenerator};
use sanvi_privacy::domain::{
    CONTEXT_NAMES, DATA_CLASSES, DataBundle, DataClassDef, EraseMode, EraseReport,
    PersonalDataProvider, PrivacyError, ProviderRegistry, RectifyPatch, SubjectRef,
};

fn ids() -> Arc<dyn RawIdGenerator> {
    Arc::new(SnowflakeGenerator::new(NodeId::new(78).expect("node id")))
}

/// Every context crate member of the workspace, derived from the root
/// manifest so a newly added context is automatically in scope.
fn workspace_context_crates() -> Vec<String> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("Cargo.toml");
    std::fs::read_to_string(manifest)
        .expect("workspace manifest readable")
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("\"crates/contexts/")
                .and_then(|rest| rest.strip_suffix("\","))
                .map(str::to_string)
        })
        .collect()
}

struct StubProvider(&'static str);

#[async_trait]
impl PersonalDataProvider for StubProvider {
    fn context_name(&self) -> &'static str {
        self.0
    }
    fn data_classes(&self) -> Vec<DataClassDef> {
        DATA_CLASSES.to_vec()
    }
    async fn collect(&self, _subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        Ok(DataBundle {
            context: self.0.to_string(),
            entries: Vec::new(),
        })
    }
    async fn erase(
        &self,
        _subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        Ok(EraseReport {
            context: self.0.to_string(),
            erased: 0,
            anonymised: 0,
            retained: Vec::new(),
        })
    }
    async fn rectify(
        &self,
        _subject: &SubjectRef,
        _patch: &RectifyPatch,
    ) -> Result<(), PrivacyError> {
        Ok(())
    }
}

fn registry_without_one_context() -> ProviderRegistry {
    CONTEXT_NAMES
        .iter()
        .skip(1) // drop the first context: the registry is now incomplete
        .map(|name| Arc::new(StubProvider(name)) as Arc<dyn PersonalDataProvider>)
        .collect()
}

#[test]
fn the_registry_names_cover_exactly_the_workspace_context_crates() {
    let contexts = workspace_context_crates();
    assert!(
        contexts.len() >= 15,
        "expected the full context list; found {contexts:?}"
    );
    for name in &CONTEXT_NAMES {
        assert!(
            contexts.iter().any(|c| c == name),
            "CONTEXT_NAMES names `{name}` but no such context crate exists"
        );
    }
    for context in &contexts {
        assert!(
            CONTEXT_NAMES.contains(&context.as_str()),
            "context crate `{context}` is named in no registry — add it to CONTEXT_NAMES and give it a PersonalDataProvider"
        );
    }
}

#[tokio::test]
async fn a_context_registered_in_neither_path_fails_the_composition_root() {
    // `build_privacy_state` is the shared choke point (API binary, CLI
    // drills, this crate): its first act is the completeness assert, so an
    // incomplete registry cannot boot anywhere. (The runtime context is
    // only for the lazy pool's timers; the assert fires before any I/O.)
    let result = std::panic::catch_unwind(|| {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://127.0.0.1:1/never_connected")
            .expect("lazy pool");
        let registry = registry_without_one_context();
        // The audit writer is never called before the registry assert — a
        // noop over the audit crate's own port is enough to type the call.
        let audit: Arc<dyn sanvi_audit::domain::ports::AuditWriter> = Arc::new(NoopAudit);
        let _ = sanvi_api::build_privacy_state(
            pool,
            ids(),
            &Default::default(),
            registry,
            audit,
            None,
            None,
        );
    });
    let message = result
        .as_ref()
        .err()
        .and_then(|payload| {
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        })
        .expect("an incomplete registry must fail the composition root");
    assert!(
        message.contains("provider registry"),
        "the failure must name the registry: {message}"
    );
}

struct NoopAudit;

#[async_trait]
impl sanvi_audit::domain::ports::AuditWriter for NoopAudit {
    async fn append(
        &self,
        _scope: &mut sanvi_persistence::PlatformScope<'_>,
        _data: &sanvi_audit::domain::AuditEntryData,
    ) -> Result<i64, sanvi_audit::domain::AuditError> {
        Ok(0)
    }
}

/// The end-to-end half: both DSR paths fan out over the real registry and
/// complete only when every context participated.
#[tokio::test]
async fn every_registered_context_joins_both_the_access_and_erasure_paths() {
    let db = sanvi_testsupport::test_db().await;
    let harness = ComplianceHarness::build(db.app_pool().clone(), ids());
    for kind in [DrillKind::Access, DrillKind::Erasure] {
        let timing = run_dsr_drill(&harness, "eu", kind)
            .await
            .unwrap_or_else(|err| panic!("{kind:?} drill over the real registry: {err}"));
        assert!(
            timing.within_deadline,
            "{kind:?} drill outside its statutory window: {timing:?}"
        );
    }
}
