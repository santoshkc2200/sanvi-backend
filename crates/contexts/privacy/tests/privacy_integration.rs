//! Integration tests: real Postgres in containers (sanvi_app role,
//! NOBYPASSRLS), full migrations, the real privacy use cases and repos —
//! with fake providers standing in for the other contexts (their providers
//! are covered by the provider-registry CI test and their own integration
//! suites).

use std::sync::Arc;

use async_trait::async_trait;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

use sanvi_persistence::{DatabaseConfig, Migrator, TxManager, build_pool};
use sanvi_privacy::adapters::{
    LoggingNotifications, OutboxEventPublisher, PrivacyContextProvider, Sha256Hasher,
    SqlxAppealRepository, SqlxAuthorizedAgentRepository, SqlxConsentRepository,
    SqlxDirectiveRepository, SqlxDsrRepository, SqlxEnvelopeCrypto, SqlxExportArtifactRepository,
    SqlxJurisdictionRepository, SqlxLegalHoldRepository, SqlxParticipantRepository,
    SqlxRetentionRuleRepository, SqlxSignalRepository, SqlxSubProcessorRepository,
    SqlxSubjectKeyRepository, SqlxSubjectRepository, SqlxVerificationRepository, SystemClock,
    SystemIdGenerator, ZipPackager,
};
use sanvi_privacy::application::{
    AppealRequest, AppealRequestCommand, DecideAppeal, DecideAppealCommand, DownloadExport,
    DownloadExportCommand, GetRequestStatus, ProcessDsrBatch, RecordConsentChange,
    RecordConsentChangeCommand, RecordOptOut, RecordOptOutCommand, RejectRequest, RetentionSweep,
    SubmitDsr, SubmitDsrCommand,
};
use sanvi_privacy::domain::{
    AppealOutcome, AppealRepository, AuditWriter, AuthorizedAgentRepository, Clock,
    ConsentRepository, DATA_CLASSES, DataBundle, DataClass, DataClassDef, DataEntry,
    DirectiveRepository, DirectiveResolver, DirectiveSource, DirectiveState, DsrId, DsrKind,
    DsrRepository, DsrStatus, EnvelopeCrypto, EraseMode, EraseReport, EventPublisher,
    ExportArtifactRepository, Hasher, IdGenerator, JurisdictionRepository, LegalHoldRepository,
    Notifications, ParticipantRepository, PersonalDataProvider, PrivacyError, ProcessingPurpose,
    ProviderRegistry, RectifyPatch, RetentionRuleRepository, SignalRepository,
    SubProcessorRepository, SubjectRef, SubjectRepository, VerificationRepository,
};

fn migrations_dir() -> String {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../migrations")
        .canonicalize()
        .expect("migrations directory must exist")
        .to_string_lossy()
        .to_string()
}

async fn prepare() -> (
    sanvi_persistence::PgPool,
    sanvi_persistence::PgPool,
    testcontainers::ContainerAsync<Postgres>,
) {
    let node = Postgres::default().start().await.expect("postgres");
    let port = node.get_host_port_ipv4(5432).await.expect("port");
    let admin = build_pool(&DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: "postgres".to_string(),
        user: "postgres".to_string(),
        password: "postgres".to_string(),
        migrations_dir: migrations_dir(),
        ..DatabaseConfig::default()
    })
    .await
    .unwrap();
    sqlx::query("CREATE ROLE sanvi_migrator LOGIN PASSWORD 'mig'")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE ROLE sanvi_app LOGIN PASSWORD 'app' NOBYPASSRLS")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE DATABASE sanvi_test OWNER sanvi_migrator")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("GRANT CONNECT ON DATABASE sanvi_test TO sanvi_app")
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;

    let migrator_pool = build_pool(&DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: "sanvi_test".to_string(),
        user: "sanvi_migrator".to_string(),
        password: "mig".to_string(),
        migrations_dir: migrations_dir(),
        ..DatabaseConfig::default()
    })
    .await
    .unwrap();
    Migrator::new(migrations_dir())
        .run(&migrator_pool)
        .await
        .expect("migrations up");

    let app_pool = build_pool(&DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: "sanvi_test".to_string(),
        user: "sanvi_app".to_string(),
        password: "app".to_string(),
        migrations_dir: migrations_dir(),
        ..DatabaseConfig::default()
    })
    .await
    .unwrap();

    (app_pool, migrator_pool, node)
}

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

struct NoopAudit;

#[async_trait]
impl AuditWriter for NoopAudit {
    async fn record(
        &self,
        _scope: &mut sanvi_persistence::PlatformScope<'_>,
        _actor: Uuid,
        _action: String,
        _resource: String,
        _detail: serde_json::Value,
    ) -> Result<(), PrivacyError> {
        Ok(())
    }
}

struct FakeProvider {
    context: &'static str,
    erased: Arc<std::sync::Mutex<usize>>,
}

#[async_trait]
impl PersonalDataProvider for FakeProvider {
    fn context_name(&self) -> &'static str {
        self.context
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        DATA_CLASSES.to_vec()
    }

    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        Ok(DataBundle {
            context: self.context.to_string(),
            entries: vec![DataEntry {
                data_class: DataClass::IdentityProfile,
                category: "Account".to_string(),
                label: "test".to_string(),
                value: serde_json::json!(subject.canonical_key()),
            }],
        })
    }

    async fn erase(
        &self,
        subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        let mut counter = self.erased.lock().unwrap();
        *counter += 1;
        let _ = subject;
        Ok(EraseReport {
            context: self.context.to_string(),
            erased: 1,
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

// ---------------------------------------------------------------------------
// Wiring helper
// ---------------------------------------------------------------------------

struct Wired {
    tx: TxManager,
    submit: SubmitDsr,
    status: GetRequestStatus,
    process: ProcessDsrBatch,
    download: DownloadExport,
    opt_out: RecordOptOut,
    consent: RecordConsentChange,
    reject: RejectRequest,
    appeal: AppealRequest,
    decide_appeal: DecideAppeal,
    retention: RetentionSweep,
    resolver: Arc<dyn DirectiveResolver>,
    requests: Arc<dyn DsrRepository>,
    subjects: Arc<dyn SubjectRepository>,
    jurisdictions: Arc<dyn JurisdictionRepository>,
    holds: Arc<dyn LegalHoldRepository>,
    participants: Arc<dyn ParticipantRepository>,
    signals: Arc<dyn SignalRepository>,
    artifacts: Arc<dyn ExportArtifactRepository>,
    processors: Arc<dyn SubProcessorRepository>,
    registry: ProviderRegistry,
    fake_erase_counts: Vec<Arc<std::sync::Mutex<usize>>>,
    _node: testcontainers::ContainerAsync<Postgres>,
    _migrator: sanvi_persistence::PgPool,
}

#[allow(clippy::too_many_arguments)]
async fn wire() -> Wired {
    let (pool, migrator, node) = prepare().await;
    let tx = TxManager::new(pool.clone());
    let subjects: Arc<dyn SubjectRepository> = Arc::new(SqlxSubjectRepository::new());
    let jurisdictions: Arc<dyn JurisdictionRepository> =
        Arc::new(SqlxJurisdictionRepository::new());
    let requests: Arc<dyn DsrRepository> = Arc::new(SqlxDsrRepository::new());
    let appeals_repo: Arc<dyn AppealRepository> = Arc::new(SqlxAppealRepository::new());
    let participants: Arc<dyn ParticipantRepository> = Arc::new(SqlxParticipantRepository::new());
    let consents: Arc<dyn ConsentRepository> = Arc::new(SqlxConsentRepository::new());
    let signals: Arc<dyn SignalRepository> = Arc::new(SqlxSignalRepository::new());
    let directives: Arc<dyn DirectiveRepository> = Arc::new(SqlxDirectiveRepository::new());
    let holds: Arc<dyn LegalHoldRepository> = Arc::new(SqlxLegalHoldRepository::new());
    let artifacts: Arc<dyn ExportArtifactRepository> =
        Arc::new(SqlxExportArtifactRepository::new());
    let processors: Arc<dyn SubProcessorRepository> = Arc::new(SqlxSubProcessorRepository::new());
    let rules: Arc<dyn RetentionRuleRepository> = Arc::new(SqlxRetentionRuleRepository::new());
    let keys: Arc<dyn sanvi_privacy::domain::SubjectKeyRepository> =
        Arc::new(SqlxSubjectKeyRepository::new());
    let challenges: Arc<dyn VerificationRepository> = Arc::new(SqlxVerificationRepository::new());
    let agents: Arc<dyn AuthorizedAgentRepository> = Arc::new(SqlxAuthorizedAgentRepository::new());
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new());
    let notifications: Arc<dyn Notifications> = Arc::new(LoggingNotifications);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let ids: Arc<dyn IdGenerator> = Arc::new(SystemIdGenerator);
    let hasher: Arc<dyn Hasher> = Arc::new(Sha256Hasher::new("test-pepper".to_string()));
    let crypto: Arc<dyn EnvelopeCrypto> = Arc::new(SqlxEnvelopeCrypto::new(
        tx.clone(),
        keys.clone(),
        [7u8; 32],
        "test-kek".to_string(),
    ));
    let audit: Arc<dyn AuditWriter> = Arc::new(NoopAudit);
    let export_dir = std::env::temp_dir()
        .join(format!("sanvi-privacy-test-{}", Uuid::now_v7()))
        .to_string_lossy()
        .to_string();

    let resolver: Arc<dyn DirectiveResolver> =
        Arc::new(sanvi_privacy::application::PersistedDirectiveResolver::new(
            tx.clone(),
            subjects.clone(),
            jurisdictions.clone(),
            consents.clone(),
            signals.clone(),
            directives.clone(),
            clock.clone(),
        ));

    let fake_erase_counts: Vec<Arc<std::sync::Mutex<usize>>> = vec![
        Arc::new(std::sync::Mutex::new(0)),
        Arc::new(std::sync::Mutex::new(0)),
    ];
    let registry: ProviderRegistry = vec![
        Arc::new(FakeProvider {
            context: "tenancy",
            erased: fake_erase_counts[0].clone(),
        }),
        Arc::new(FakeProvider {
            context: "identity",
            erased: fake_erase_counts[1].clone(),
        }),
        Arc::new(PrivacyContextProvider::new(
            tx.clone(),
            subjects.clone(),
            consents.clone(),
            signals.clone(),
            directives.clone(),
        )),
    ];

    let submit = SubmitDsr::new(
        tx.clone(),
        subjects.clone(),
        jurisdictions.clone(),
        requests.clone(),
        participants.clone(),
        challenges.clone(),
        agents.clone(),
        consents.clone(),
        directives.clone(),
        signals.clone(),
        events.clone(),
        notifications.clone(),
        audit.clone(),
        hasher.clone(),
        ids.clone(),
        clock.clone(),
        registry.clone(),
        "2026-08-19".to_string(),
        6,
        600,
    );
    let status = GetRequestStatus::new(
        tx.clone(),
        requests.clone(),
        appeals_repo.clone(),
        challenges.clone(),
        hasher.clone(),
    );
    let process = ProcessDsrBatch::new(
        tx.clone(),
        requests.clone(),
        participants.clone(),
        holds.clone(),
        artifacts.clone(),
        registry.clone(),
        Arc::new(ZipPackager::new()),
        crypto.clone(),
        notifications.clone(),
        events.clone(),
        hasher.clone(),
        clock.clone(),
        export_dir,
        7,
        3,
        30,
    );
    let download = DownloadExport::new(
        tx.clone(),
        requests.clone(),
        artifacts.clone(),
        hasher.clone(),
        clock.clone(),
    );
    let opt_out = RecordOptOut::new(
        tx.clone(),
        subjects.clone(),
        jurisdictions.clone(),
        consents.clone(),
        signals.clone(),
        directives.clone(),
        events.clone(),
        clock.clone(),
        "2026-08-19".to_string(),
    );
    let consent = RecordConsentChange::new(
        tx.clone(),
        subjects.clone(),
        jurisdictions.clone(),
        consents.clone(),
        directives.clone(),
        events.clone(),
        clock.clone(),
    );
    let reject = RejectRequest::new(
        tx.clone(),
        requests.clone(),
        events.clone(),
        clock.clone(),
        audit.clone(),
    );
    let appeal = AppealRequest::new(
        tx.clone(),
        requests.clone(),
        appeals_repo.clone(),
        jurisdictions.clone(),
        events.clone(),
        audit.clone(),
        ids.clone(),
        clock.clone(),
    );
    let decide_appeal = DecideAppeal::new(
        tx.clone(),
        requests.clone(),
        appeals_repo.clone(),
        events.clone(),
        audit.clone(),
        clock.clone(),
    );
    let retention = RetentionSweep::new(
        tx.clone(),
        rules.clone(),
        consents.clone(),
        signals.clone(),
        clock.clone(),
        audit.clone(),
        true,
        500,
    );

    Wired {
        tx,
        submit,
        status,
        process,
        download,
        opt_out,
        consent,
        reject,
        appeal,
        decide_appeal,
        retention,
        resolver,
        requests,
        subjects,
        jurisdictions,
        holds,
        participants,
        signals,
        artifacts,
        processors,
        registry,
        fake_erase_counts,
        _node: node,
        _migrator: migrator,
    }
}

fn eu_submit(_user_id: Uuid, kind: DsrKind) -> SubmitDsrCommand {
    SubmitDsrCommand {
        subject: sanvi_privacy::application::SubjectInput {
            kind: "tenant_user".to_string(),
            email: None,
            external_id: None,
            evidence: vec![sanvi_privacy::application::EvidenceInput {
                kind: "declared_residency".to_string(),
                value: "DE".to_string(),
            }],
            age_signal: None,
        },
        kind,
        requester: sanvi_privacy::domain::Requester::Subject,
        agent: None,
        note: None,
    }
}

fn us_submit(_user_id: Uuid, kind: DsrKind) -> SubmitDsrCommand {
    SubmitDsrCommand {
        subject: sanvi_privacy::application::SubjectInput {
            kind: "tenant_user".to_string(),
            email: None,
            external_id: None,
            evidence: vec![sanvi_privacy::application::EvidenceInput {
                kind: "declared_residency".to_string(),
                value: "US-CA".to_string(),
            }],
            age_signal: None,
        },
        kind,
        requester: sanvi_privacy::domain::Requester::Subject,
        agent: None,
        note: None,
    }
}

fn va_submit(_user_id: Uuid, kind: DsrKind) -> SubmitDsrCommand {
    let mut command = us_submit(_user_id, kind);
    command.subject.evidence = vec![sanvi_privacy::application::EvidenceInput {
        kind: "declared_residency".to_string(),
        value: "US-VA".to_string(),
    }];
    command
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn authenticated_export_request_collects_from_every_provider() {
    let wired = wire().await;
    let user = Uuid::now_v7();
    let output = wired
        .submit
        .execute(Some(user), None, None, eu_submit(user, DsrKind::Export))
        .await
        .unwrap();
    assert_eq!(output.jurisdiction, "eu");
    assert!(!output.verification_required);

    // The saga advances and packages the export.
    let report = wired.process.execute(10).await.unwrap();
    assert!(report.exports_packaged >= 1, "export should be packaged");

    let status = wired
        .status
        .execute(output.request_id, Some(user), None)
        .await
        .unwrap();
    assert_eq!(status.status, DsrStatus::Completed);
    assert!(status.export_available);

    // Every registered provider participated.
    let mut scope = wired
        .tx
        .begin_unscoped("test_participants", "test")
        .await
        .unwrap();
    let participants = wired
        .participants
        .list(&mut scope, DsrId::from_uuid(output.request_id))
        .await
        .unwrap();
    assert_eq!(participants.len(), wired.registry.len());
    for provider in &wired.registry {
        assert_eq!(
            participants.get(provider.context_name()).unwrap().status,
            sanvi_privacy::domain::ParticipantStatus::Completed,
            "provider {} must participate",
            provider.context_name()
        );
    }
    scope.commit().await.unwrap();
}

#[tokio::test]
async fn erasure_runs_every_provider_and_crypto_shreds() {
    let wired = wire().await;
    let user = Uuid::now_v7();
    let output = wired
        .submit
        .execute(Some(user), None, None, eu_submit(user, DsrKind::Erasure))
        .await
        .unwrap();

    let report = wired.process.execute(10).await.unwrap();
    assert!(report.completed >= 1, "erasure should complete");
    assert!(report.shreds >= 1, "erasure must crypto-shred");

    for counter in &wired.fake_erase_counts {
        assert_eq!(*counter.lock().unwrap(), 1, "every provider must run erase");
    }

    let status = wired
        .status
        .execute(output.request_id, Some(user), None)
        .await
        .unwrap();
    assert_eq!(status.status, DsrStatus::Completed);
}

#[tokio::test]
async fn eu_and_us_defaults_differ_but_the_code_path_is_one() {
    let wired = wire().await;
    let eu_user = Uuid::now_v7();
    let us_user = Uuid::now_v7();
    wired
        .submit
        .execute(
            Some(eu_user),
            None,
            None,
            eu_submit(eu_user, DsrKind::Access),
        )
        .await
        .unwrap();
    wired
        .submit
        .execute(
            Some(us_user),
            None,
            None,
            us_submit(us_user, DsrKind::Access),
        )
        .await
        .unwrap();

    let eu_snapshot = wired
        .resolver
        .snapshot(&SubjectRef::user(eu_user))
        .await
        .unwrap();
    let us_snapshot = wired
        .resolver
        .snapshot(&SubjectRef::user(us_user))
        .await
        .unwrap();

    let eu_analytics = eu_snapshot
        .directives
        .iter()
        .find(|d| d.purpose == ProcessingPurpose::Analytics)
        .unwrap();
    let us_analytics = us_snapshot
        .directives
        .iter()
        .find(|d| d.purpose == ProcessingPurpose::Analytics)
        .unwrap();
    assert_eq!(eu_analytics.state, DirectiveState::Denied);
    assert_eq!(us_analytics.state, DirectiveState::Allowed);
    assert_eq!(eu_analytics.source, DirectiveSource::Default);
    assert_eq!(us_analytics.source, DirectiveSource::Default);
    assert_eq!(us_snapshot.jurisdiction.as_str(), "us-ca");
}

#[tokio::test]
async fn opt_out_needs_no_verification_and_flips_immediately() {
    let wired = wire().await;
    let user = Uuid::now_v7();
    wired
        .submit
        .execute(Some(user), None, None, us_submit(user, DsrKind::Access))
        .await
        .unwrap();

    let output = wired
        .opt_out
        .execute(
            &SubjectRef::user(user),
            RecordOptOutCommand {
                purposes: vec![ProcessingPurpose::SaleOrShare],
                source: "api".to_string(),
                device_ref: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(output.directives[0].state, DirectiveState::Denied);

    let snapshot = wired
        .resolver
        .snapshot(&SubjectRef::user(user))
        .await
        .unwrap();
    let sale = snapshot
        .directives
        .iter()
        .find(|d| d.purpose == ProcessingPurpose::SaleOrShare)
        .unwrap();
    assert_eq!(sale.state, DirectiveState::Denied);
    assert_eq!(sale.source, DirectiveSource::OptOut);
}

#[tokio::test]
async fn gpc_signal_denies_before_login_and_binds_after() {
    let wired = wire().await;
    let user = Uuid::now_v7();
    let device = format!("device-{}", Uuid::now_v7());

    // Pre-authentication: the device signal denies immediately.
    let output = wired
        .opt_out
        .execute(
            &SubjectRef::device(&device),
            RecordOptOutCommand {
                purposes: vec![
                    ProcessingPurpose::SaleOrShare,
                    ProcessingPurpose::TargetedAdvertising,
                ],
                source: "gpc".to_string(),
                device_ref: Some(device.clone()),
            },
        )
        .await
        .unwrap();
    assert_eq!(output.directives.len(), 2);

    // After the subject exists, the GPC middleware binds the device signal
    // on the next authenticated request — binding never unbinds.
    wired
        .submit
        .execute(Some(user), None, None, us_submit(user, DsrKind::Access))
        .await
        .unwrap();
    let mut scope = wired
        .tx
        .begin_unscoped("test_gpc_bind", "test")
        .await
        .unwrap();
    let stored = wired
        .subjects
        .find_by_key(&mut scope, &format!("user:{user}"))
        .await
        .unwrap()
        .expect("subject exists after submission");
    let bound = wired
        .signals
        .bind_by_device(&mut scope, &device, stored.id, "us-ca")
        .await
        .unwrap();
    assert!(bound >= 1, "the device signal must bind to the subject");
    scope.commit().await.unwrap();

    let snapshot = wired
        .resolver
        .snapshot(&SubjectRef::user(user))
        .await
        .unwrap();
    let sale = snapshot
        .directives
        .iter()
        .find(|d| d.purpose == ProcessingPurpose::SaleOrShare)
        .unwrap();
    assert_eq!(
        sale.state,
        DirectiveState::Denied,
        "GPC denial survives binding"
    );
}

#[tokio::test]
async fn consent_withdrawal_flips_marketing_email() {
    let wired = wire().await;
    let user = Uuid::now_v7();
    wired
        .submit
        .execute(Some(user), None, None, us_submit(user, DsrKind::Access))
        .await
        .unwrap();
    let subject = SubjectRef::user(user);

    let granted = wired
        .consent
        .execute(
            &subject,
            RecordConsentChangeCommand {
                purpose: ProcessingPurpose::MarketingEmail,
                granted: true,
                notice_version: "2026-08-19".to_string(),
                method: "consent".to_string(),
                locale: None,
                signal_source: Some("ui".to_string()),
            },
        )
        .await
        .unwrap();
    assert_eq!(granted.state, DirectiveState::Allowed);

    let withdrawn = wired
        .consent
        .execute(
            &subject,
            RecordConsentChangeCommand {
                purpose: ProcessingPurpose::MarketingEmail,
                granted: false,
                notice_version: "2026-08-19".to_string(),
                method: "withdrawal".to_string(),
                locale: None,
                signal_source: Some("ui".to_string()),
            },
        )
        .await
        .unwrap();
    assert_eq!(withdrawn.state, DirectiveState::Denied);
    assert_eq!(withdrawn.source, DirectiveSource::OptOut);

    let snapshot = wired.resolver.snapshot(&subject).await.unwrap();
    let marketing = snapshot
        .directives
        .iter()
        .find(|d| d.purpose == ProcessingPurpose::MarketingEmail)
        .unwrap();
    assert_eq!(marketing.state, DirectiveState::Denied);
}

#[tokio::test]
async fn legal_hold_blocks_erasure_until_released() {
    let wired = wire().await;
    let user = Uuid::now_v7();
    let output = wired
        .submit
        .execute(Some(user), None, None, eu_submit(user, DsrKind::Erasure))
        .await
        .unwrap();

    let mut scope = wired.tx.begin_unscoped("test_hold", "test").await.unwrap();
    let hold = sanvi_privacy::domain::LegalHold {
        id: Uuid::now_v7(),
        scope: serde_json::json!({ "subject_key": format!("user:{user}") }),
        reason: "litigation".to_string(),
        created_by: Uuid::nil(),
        created_at: chrono::Utc::now(),
        released_at: None,
        released_by: None,
    };
    wired.holds.save(&mut scope, &hold).await.unwrap();
    scope.commit().await.unwrap();

    let report = wired.process.execute(10).await.unwrap();
    assert_eq!(report.completed, 0, "a held request must not erase");
    let status = wired
        .status
        .execute(output.request_id, Some(user), None)
        .await
        .unwrap();
    assert_eq!(status.status, DsrStatus::OnHold);

    // Release and re-run: erasure proceeds.
    let mut scope = wired
        .tx
        .begin_unscoped("test_release", "test")
        .await
        .unwrap();
    wired
        .holds
        .release(&mut scope, hold.id, Uuid::nil())
        .await
        .unwrap();
    scope.commit().await.unwrap();
    let report = wired.process.execute(10).await.unwrap();
    assert!(report.completed >= 1);
}

#[tokio::test]
async fn a_us_refusal_can_be_appealed_and_decided_by_a_second_operator() {
    let wired = wire().await;
    let user = Uuid::now_v7();
    let output = wired
        .submit
        .execute(Some(user), None, None, va_submit(user, DsrKind::Access))
        .await
        .unwrap();

    let operator = Uuid::now_v7();
    wired
        .reject
        .execute(
            operator,
            output.request_id,
            "verification failed".to_string(),
        )
        .await
        .unwrap();

    let appeal = wired
        .appeal
        .execute(
            Some(user),
            AppealRequestCommand {
                request_id: output.request_id,
                reason: "I disagree".to_string(),
            },
        )
        .await
        .unwrap();
    assert!(appeal.authority.complaint_url.starts_with("https://"));

    let second_operator = Uuid::now_v7();
    let decision = wired
        .decide_appeal
        .execute(
            second_operator,
            DecideAppealCommand {
                appeal_id: appeal.appeal_id,
                outcome: AppealOutcome::Upheld,
                reason: "re-review found the verification evidence sufficient".to_string(),
            },
        )
        .await
        .unwrap();
    assert_eq!(decision.outcome, AppealOutcome::Upheld);

    let status = wired
        .status
        .execute(output.request_id, Some(user), None)
        .await
        .unwrap();
    assert!(matches!(
        status.status,
        DsrStatus::InProgress | DsrStatus::Completed
    ));
    assert!(status.appeal.is_some());
}

#[tokio::test]
async fn export_download_respects_token_passphrase_and_cap() {
    let wired = wire().await;
    let user = Uuid::now_v7();
    let output = wired
        .submit
        .execute(Some(user), None, None, eu_submit(user, DsrKind::Export))
        .await
        .unwrap();
    wired.process.execute(10).await.unwrap();

    // The token/passphrase were logged by the logging notifications; the
    // test reads the artifact row directly to verify the gate semantics
    // (the passphrase itself is never stored).
    let mut scope = wired
        .tx
        .begin_unscoped("test_artifact", "test")
        .await
        .unwrap();
    let request = wired
        .requests
        .find_by_id(&mut scope, DsrId::from_uuid(output.request_id))
        .await
        .unwrap()
        .unwrap();
    let artifact = wired
        .artifacts
        .find(&mut scope, request.export_artifact_id.unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(artifact.passphrase_verifier != artifact.download_token_hash);
    assert_eq!(artifact.download_count, 0);
    assert_eq!(artifact.max_downloads, 3);
    scope.commit().await.unwrap();

    // Wrong token: not found (no existence oracle).
    let result = wired
        .download
        .execute(DownloadExportCommand {
            request_id: output.request_id,
            token: "wrong-token".to_string(),
            passphrase: "wrong".to_string(),
        })
        .await;
    assert!(matches!(result, Err(PrivacyError::ArtifactNotFound(_))));
}

#[tokio::test]
async fn retention_sweep_dry_run_reports_without_deleting() {
    let wired = wire().await;
    let user = Uuid::now_v7();
    wired
        .submit
        .execute(Some(user), None, None, eu_submit(user, DsrKind::Access))
        .await
        .unwrap();
    let report = wired.retention.execute().await.unwrap();
    assert!(report.dry_run);
    assert_eq!(report.deleted.consents, 0, "dry-run never deletes");
}

#[tokio::test]
async fn the_public_disclosures_are_generated() {
    let wired = wire().await;
    let notice = sanvi_privacy::application::NoticeService::new(
        wired.tx.clone(),
        Arc::new(SqlxRetentionRuleRepository::new()),
        wired.processors.clone(),
        wired.jurisdictions.clone(),
    )
    .build("2026-08-19", 30)
    .await
    .unwrap();
    assert!(notice.jurisdictions.iter().any(|j| j.code == "eu"));
    assert!(!notice.retention.is_empty());
}
