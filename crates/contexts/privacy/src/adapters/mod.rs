//! Driven adapters: sqlx repositories, envelope crypto, export packaging,
//! event publishing, GPC middleware, notifications/hashing and the privacy
//! context's own provider.

pub mod crypto;
pub mod events;
pub mod gpc;
pub mod http;
pub mod misc;
pub mod notification_intent;
pub mod packager;
pub mod providers;
pub mod repos;

pub use crypto::{DisabledEnvelopeCrypto, SqlxEnvelopeCrypto};
pub use events::OutboxEventPublisher;
pub use gpc::GpcSignalMiddleware;
pub use misc::{
    LoggingNotifications, LoggingSubProcessorNotifier, RuleBasedObligationCalculator, Sha256Hasher,
    SystemClock, SystemIdGenerator,
};
pub use notification_intent::notification_intent;
pub use packager::{ZipPackager, archive_sha256};
pub use providers::{PrivacyContextProvider, anonymised_subject_ref, is_pseudonym, pseudonym};
pub use repos::{
    SqlxAppealRepository, SqlxAuthorizedAgentRepository, SqlxBreachRepository,
    SqlxConsentRepository, SqlxDirectiveRepository, SqlxDsrRepository,
    SqlxExportArtifactRepository, SqlxJurisdictionRepository, SqlxLegalHoldRepository,
    SqlxParticipantRepository, SqlxProcessingActivityRepository, SqlxRequestMetricsRepository,
    SqlxRetentionRuleRepository, SqlxRiskAssessmentRepository, SqlxSignalRepository,
    SqlxSubProcessorRepository, SqlxSubjectKeyRepository, SqlxSubjectRepository,
    SqlxVerificationRepository,
};
