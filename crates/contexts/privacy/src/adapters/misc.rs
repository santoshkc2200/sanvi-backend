//! Small driven adapters: clock, ids, hashing, notifications, the
//! sub-processor notifier and the breach obligation calculator.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

use crate::domain::jurisdiction::add_business_days;
use crate::domain::{
    AffectedSet, AuthorityContact, BreachIncident, BreachObligationCalculator, BreachRuleSet,
    Clock, ConsentRecord, Hasher, IdGenerator, JurisdictionProfile, NotificationObligation,
    Notifications, ObligationAudience, PrivacyError, SubProcessor, SubProcessorNotifier,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemIdGenerator;

impl IdGenerator for SystemIdGenerator {
    fn next(&self) -> Uuid {
        Uuid::now_v7()
    }
}

/// SHA-256 token/code hashing with an HMAC pepper derived from the KEK so
/// hashed verification values cannot be brute-forced from the database
/// alone.
#[derive(Debug, Clone)]
pub struct Sha256Hasher {
    pepper: String,
}

impl Sha256Hasher {
    pub fn new(pepper: String) -> Self {
        Self { pepper }
    }
}

impl Hasher for Sha256Hasher {
    fn hash(&self, value: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.pepper.as_bytes())
            .expect("HMAC accepts any key length");
        mac.update(value.as_bytes());
        format!("{:x}", mac.finalize().into_bytes())
    }

    fn verify(&self, value: &str, hash: &str) -> bool {
        let candidate = self.hash(value);
        // Constant-time comparison.
        let a = candidate.as_bytes();
        let b = hash.as_bytes();
        a.len() == b.len() && {
            let mut acc = 0u8;
            for (x, y) in a.iter().zip(b.iter()) {
                acc |= x ^ y;
            }
            acc == 0
        }
    }
}

/// Logging notifications: verification codes and passphrases go to the
/// structured log only (production wiring replaces this with email/SMS).
#[derive(Debug, Clone, Copy, Default)]
pub struct LoggingNotifications;

#[async_trait]
impl Notifications for LoggingNotifications {
    async fn send_verification_code(&self, to: &str, code: &str) -> Result<(), PrivacyError> {
        tracing::info!(to, code, "verification code generated (logging adapter)");
        Ok(())
    }

    async fn send_export_credentials(
        &self,
        subject: &crate::domain::SubjectRef,
        token: &str,
        passphrase: &str,
        request_id: &str,
    ) -> Result<(), PrivacyError> {
        tracing::info!(
            subject = subject.canonical_key(),
            token,
            passphrase,
            request_id,
            "export credentials generated (logging adapter)"
        );
        Ok(())
    }

    async fn send_request_status(
        &self,
        to: &str,
        subject: &str,
        status: &str,
    ) -> Result<(), PrivacyError> {
        tracing::info!(
            to,
            subject,
            status,
            "request status notification (logging adapter)"
        );
        Ok(())
    }
}

/// Development notifier. It succeeds only when there is no downstream work;
/// configured processors/admins require a real delivery adapter so the
/// ledger can never claim that a log line was propagation evidence.
#[derive(Debug, Clone, Copy, Default)]
pub struct LoggingSubProcessorNotifier;

#[async_trait]
impl SubProcessorNotifier for LoggingSubProcessorNotifier {
    async fn propagate_opt_out(
        &self,
        record: &ConsentRecord,
        processors: &[SubProcessor],
    ) -> Result<(), PrivacyError> {
        if processors.is_empty() {
            return Ok(());
        }
        Err(PrivacyError::Upstream(format!(
            "sub-processor delivery adapter is not configured for {} processor(s), purpose {}, subject {}",
            processors.len(),
            record.purpose.as_str(),
            record.subject_ref.canonical_key(),
        )))
    }

    async fn notify_removal(
        &self,
        processor: &SubProcessor,
        tenant_admins: Vec<String>,
    ) -> Result<(), PrivacyError> {
        if tenant_admins.is_empty() {
            return Ok(());
        }
        Err(PrivacyError::Upstream(format!(
            "tenant-admin delivery adapter is not configured for sub-processor {}",
            processor.name
        )))
    }
}

/// The breach obligation calculator: one incident → one obligation per
/// affected jurisdiction per audience, with deadlines from the profile's
/// breach rules. Encryption-at-rest safe harbours suppress obligations the
/// rule set marks encryption-exempt, while still recording the assessment.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuleBasedObligationCalculator;

impl RuleBasedObligationCalculator {
    pub fn new() -> Self {
        Self
    }
}

impl BreachObligationCalculator for RuleBasedObligationCalculator {
    fn compute(
        &self,
        incident: &BreachIncident,
        profiles: &[JurisdictionProfile],
        _now: DateTime<Utc>,
    ) -> Vec<NotificationObligation> {
        let mut obligations = Vec::new();
        let deadline_base = incident.discovered_at;
        for jurisdiction in &incident.affected.jurisdictions {
            let Some(profile) = profiles.iter().find(|p| &p.code == jurisdiction) else {
                continue;
            };
            let rules = &profile.breach_rules;
            let encryption_suppresses = incident.encrypted_at_rest && rules.encryption_safe_harbour;

            // Authority obligation: computed when the rule set has a
            // deadline and the affected count reaches the threshold. Even
            // when the encryption safe harbour suppresses notification the
            // assessment is recorded (suppressed flag stays true).
            let authority_due = rules
                .authority_due_hours
                .map(|h| deadline_base + chrono::Duration::hours(i64::from(h)))
                .or_else(|| {
                    rules
                        .authority_due_days
                        .map(|d| deadline_base + chrono::Duration::days(i64::from(d)))
                });
            if let Some(due_at) = authority_due {
                let below_threshold = rules
                    .authority_threshold
                    .map(|threshold| incident.affected.subjects < u64::from(threshold))
                    .unwrap_or(false);
                if !below_threshold {
                    obligations.push(NotificationObligation {
                        jurisdiction: jurisdiction.clone(),
                        audience: ObligationAudience::Authority,
                        due_at,
                        notified_at: None,
                        suppressed_by_encryption: encryption_suppresses,
                        artifact: None,
                    });
                }
            }

            // Consumer obligation.
            if let Some(days) = rules.consumer_due_days {
                let below_threshold = rules
                    .consumer_threshold
                    .map(|threshold| incident.affected.subjects < u64::from(threshold))
                    .unwrap_or(false);
                if !below_threshold {
                    obligations.push(NotificationObligation {
                        jurisdiction: jurisdiction.clone(),
                        audience: ObligationAudience::Consumer,
                        due_at: deadline_base + chrono::Duration::days(i64::from(days)),
                        notified_at: None,
                        suppressed_by_encryption: encryption_suppresses,
                        artifact: None,
                    });
                }
            }

            // Tenant obligation: processors inform their controllers
            // promptly (every regime).
            if incident.affected.tenants > 0 {
                obligations.push(NotificationObligation {
                    jurisdiction: jurisdiction.clone(),
                    audience: ObligationAudience::Tenant,
                    due_at: add_business_days(deadline_base, 2),
                    notified_at: None,
                    suppressed_by_encryption: false,
                    artifact: None,
                });
            }
        }
        obligations
    }
}

/// Helpers shared by handlers and tests.
pub fn authority_notice(profile: &JurisdictionProfile) -> AuthorityContact {
    profile.authority.clone()
}

/// Compute the affected set from raw counts (used by the incident use case).
pub fn affected_set(
    subjects: u64,
    tenants: u64,
    jurisdictions: Vec<crate::domain::JurisdictionRef>,
) -> AffectedSet {
    AffectedSet {
        subjects,
        tenants,
        jurisdictions,
    }
}

/// Breach rule set default (an empty rule set produces no obligations —
/// profiles that carry no rules never notify anyone, which is the
/// fail-safe the calculator can implement).
pub fn empty_rule_set() -> BreachRuleSet {
    BreachRuleSet::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ProcessingPurpose, SignalSource, SubjectRef};

    #[test]
    fn hasher_verifies_and_rejects() {
        let hasher = Sha256Hasher::new("pepper".to_string());
        let hash = hasher.hash("123456");
        assert!(hasher.verify("123456", &hash));
        assert!(!hasher.verify("654321", &hash));
    }

    #[test]
    fn breach_obligations_are_computed_per_jurisdiction() {
        use crate::domain::{AffectedSet, DataClass, JurisdictionRef};
        let calculator = RuleBasedObligationCalculator::new();
        let now = chrono::Utc::now();
        let eu = crate::domain::seeded_profile(&JurisdictionRef::parse("eu").unwrap()).unwrap();
        let ca = crate::domain::seeded_profile(&JurisdictionRef::parse("us-ca").unwrap()).unwrap();
        let incident = BreachIncident {
            id: Uuid::now_v7(),
            discovered_at: now,
            contained_at: None,
            affected: AffectedSet {
                subjects: 1_000,
                tenants: 3,
                jurisdictions: vec![
                    JurisdictionRef::parse("eu").unwrap(),
                    JurisdictionRef::parse("us-ca").unwrap(),
                ],
            },
            data_classes: vec![DataClass::IdentityProfile],
            encrypted_at_rest: false,
            notes: None,
            created_by: None,
        };
        let recorded_late = now + chrono::Duration::hours(10);
        let obligations = calculator.compute(&incident, &[eu, ca], recorded_late);
        // eu: authority 72h + tenant; us-ca: authority (threshold 500 met,
        // no fixed deadline → none since both fields are NULL) + consumer
        // (no fixed consumer deadline) + tenant.
        let eu_authority = obligations
            .iter()
            .find(|o| {
                o.jurisdiction.as_str() == "eu" && o.audience == ObligationAudience::Authority
            })
            .expect("eu authority obligation");
        let expected = now + chrono::Duration::hours(72);
        let delta = (eu_authority.due_at - expected).num_seconds().abs();
        assert!(
            delta <= 1,
            "GDPR authority deadline is 72 hours from discovery, not recording"
        );
        assert_eq!(
            obligations
                .iter()
                .filter(|o| o.audience == ObligationAudience::Tenant)
                .count(),
            2,
            "one tenant obligation per affected jurisdiction"
        );
    }

    #[test]
    fn encryption_safe_harbour_suppresses_but_records() {
        use crate::domain::{AffectedSet, DataClass, JurisdictionRef};
        let calculator = RuleBasedObligationCalculator::new();
        let now = chrono::Utc::now();
        let co = crate::domain::seeded_profile(&JurisdictionRef::parse("us-co").unwrap()).unwrap();
        let incident = BreachIncident {
            id: Uuid::now_v7(),
            discovered_at: now,
            contained_at: None,
            affected: AffectedSet {
                subjects: 10,
                tenants: 0,
                jurisdictions: vec![JurisdictionRef::parse("us-co").unwrap()],
            },
            data_classes: vec![DataClass::IdentityProfile],
            // CO's rule set has no encryption safe harbour: obligations
            // remain unsuppressed.
            encrypted_at_rest: true,
            notes: None,
            created_by: None,
        };
        let obligations = calculator.compute(&incident, &[co], now);
        assert!(
            obligations.iter().all(|o| !o.suppressed_by_encryption),
            "CO has no encryption safe harbour"
        );

        // EU safe harbour: encrypted at rest suppresses the authority
        // obligation but the assessment row still exists.
        let eu = crate::domain::seeded_profile(&JurisdictionRef::parse("eu").unwrap()).unwrap();
        let mut eu_incident = incident.clone();
        eu_incident.affected.jurisdictions = vec![JurisdictionRef::parse("eu").unwrap()];
        let obligations = calculator.compute(&eu_incident, &[eu], now);
        assert!(
            obligations
                .iter()
                .any(|o| o.suppressed_by_encryption && o.audience == ObligationAudience::Authority)
        );
    }

    #[test]
    fn subject_ref_helper_types_work() {
        let subject = SubjectRef::contact("test@example.com");
        assert_eq!(subject.canonical_key(), "email:test@example.com");
        assert!(matches!(SignalSource::parse("gpc"), Ok(SignalSource::Gpc)));
        assert!(matches!(
            ProcessingPurpose::parse("marketing_email"),
            Ok(ProcessingPurpose::MarketingEmail)
        ));
    }
}
