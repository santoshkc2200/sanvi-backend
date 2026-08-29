//! The directive gate (TASK-014, slice 10.5): every conversion event asks
//! the phase-05 `DirectiveResolver` for `ads_measurement` and, for any
//! platform whose sub-processor role is `ThirdParty`, also
//! `sale_or_share`/`targeted_advertising`, before the event is ever
//! stored. The answer is computed once here and persisted on the event —
//! nothing downstream re-derives it.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use sanvi_kernel::TenantId;
use sanvi_persistence::{ScopedTx, TxManager};

use sanvi_privacy::domain::{
    DirectiveResolver, DirectiveState, ProcessingPurpose, SubjectKind, SubjectRef,
};

use crate::domain::{
    AdvertisingError, ClickIds, ConsentSnapshot, ConversionEvent, ConversionEventRepository, Money,
    PaymentLookupPort, PlatformKey, SubjectKeyRef, ValueSource, VendorRole, VendorRolePort,
    hashing::{hash_email, hash_phone},
};

/// Extracts the non-PII key material a later withdrawal re-check needs to
/// rebuild this exact `SubjectRef` — never the raw email a `Contact`
/// subject carries in its `identifiers` (TASK-014 forbids persisting that
/// here). A missing identifier for the expected kind is a resolver
/// contract violation, not a value to guess at, so it also falls back to
/// `Contact` (refuse-on-retry) rather than fabricate a key.
fn subject_key_ref(subject: &SubjectRef) -> SubjectKeyRef {
    let find = |id_type: &str| {
        subject
            .identifiers
            .iter()
            .find(|identifier| identifier.id_type == id_type)
            .map(|identifier| identifier.value.clone())
    };
    match subject.kind {
        SubjectKind::Device => match find("device_ref") {
            Some(device_ref) => SubjectKeyRef::TenantDevice { device_ref },
            None => SubjectKeyRef::Contact,
        },
        SubjectKind::EndUser => match find("external_id") {
            Some(external_id) => SubjectKeyRef::EndUser { external_id },
            None => SubjectKeyRef::Contact,
        },
        SubjectKind::TenantUser => match find("user_id").and_then(|v| v.parse::<i64>().ok()) {
            Some(user_id) => SubjectKeyRef::TenantUser { user_id },
            None => SubjectKeyRef::Contact,
        },
        SubjectKind::Contact => SubjectKeyRef::Contact,
    }
}

/// Versions this gate's own purpose-mapping logic (which purposes a
/// platform's sub-processor role requires), not the phase-05 resolver
/// itself — the resolver has no version of its own to quote. Bump this
/// when the mapping rules change, so a stored event's proof states which
/// rules decided it.
const CONSENT_GATE_VERSION: &str = "v1";

/// Stable dedup key: (tenant_id, order_ref, event_name) when an order
/// reference exists, otherwise (tenant_id, event_id, event_name). An order
/// reference is immutable across checkout lifecycle transitions, unlike the
/// presence of a completed payment record; event name keeps independent
/// lifecycle events distinct.
fn conversion_dedupe_key(
    tenant_id: TenantId,
    order_ref: Option<&str>,
    event_name: &str,
    event_id: &str,
) -> String {
    let (kind, identity) = match order_ref {
        Some(order_ref) => ("order", order_ref),
        None => ("client_event", event_id),
    };
    format!("{tenant_id}:{kind}:{identity}:event:{event_name}")
}

pub struct CaptureConversion {
    tx: TxManager,
    conversion_repo: Arc<dyn ConversionEventRepository>,
    payment_lookup: Arc<dyn PaymentLookupPort>,
    vendor_role: Arc<dyn VendorRolePort>,
    directive_resolver: Arc<dyn DirectiveResolver>,
    ids: Arc<dyn crate::domain::IdGenerator>,
}

pub struct TrackConversionCommand {
    pub tenant_id: TenantId,
    pub event_name: String,
    pub event_id: String,
    pub order_ref: Option<String>,
    pub click_ids: ClickIds,
    pub value: Option<i64>,
    pub currency: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub subject: SubjectRef,
    /// The platforms this event name maps to, per the tenant's tracking
    /// settings — resolved by the caller before invoking capture, since it
    /// decides which extra purposes the gate must ask for.
    pub platforms: Vec<PlatformKey>,
    /// `true` when that list cannot be trusted as the complete set of
    /// vendors this event will reach: no mapping for the event name yet,
    /// or a mapped platform key this build doesn't recognise. The proof is
    /// frozen onto the stored event and nothing downstream re-derives it,
    /// so an incomplete list must widen the gate rather than narrow it.
    pub platforms_incomplete: bool,
}

impl CaptureConversion {
    pub fn new(
        tx: TxManager,
        conversion_repo: Arc<dyn ConversionEventRepository>,
        payment_lookup: Arc<dyn PaymentLookupPort>,
        vendor_role: Arc<dyn VendorRolePort>,
        directive_resolver: Arc<dyn DirectiveResolver>,
        ids: Arc<dyn crate::domain::IdGenerator>,
    ) -> Self {
        Self {
            tx,
            conversion_repo,
            payment_lookup,
            vendor_role,
            directive_resolver,
            ids,
        }
    }

    /// Captures and persists a conversion event. Used by `/public/track`.
    pub async fn execute(&self, command: TrackConversionCommand) -> Result<(), AdvertisingError> {
        // The gate runs *before* this context's transaction opens: the
        // phase-05 resolver takes a connection of its own, once per
        // purpose, from the same pool. Holding one here across that call
        // lets a burst of concurrent beacons occupy every connection
        // waiting for a connection, and the pool deadlocks until acquire
        // timeouts unwind it.
        let consent = self.resolve_gate(&command).await?;

        let mut scope = self
            .tx
            .begin_for(command.tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let event = self.build_event(&mut scope, command, consent).await?;
        self.conversion_repo
            .insert_idempotent(&mut scope, &event)
            .await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(())
    }

    /// Runs the same capture pipeline without persisting anything — the
    /// synthetic test-event endpoint, so a tenant can see what the gate
    /// would decide without polluting real metrics. The transaction is
    /// dropped, never committed.
    pub async fn dry_run(
        &self,
        command: TrackConversionCommand,
    ) -> Result<ConversionEvent, AdvertisingError> {
        let consent = self.resolve_gate(&command).await?;
        let mut scope = self
            .tx
            .begin_for(command.tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        self.build_event(&mut scope, command, consent).await
    }

    async fn build_event(
        &self,
        scope: &mut ScopedTx<'_>,
        command: TrackConversionCommand,
        consent: ConsentSnapshot,
    ) -> Result<ConversionEvent, AdvertisingError> {
        let mut hashed_identifiers = BTreeMap::new();
        if let Some(ref email) = command.email {
            hashed_identifiers.insert("email".to_string(), hash_email(email));
        }
        if let Some(ref phone) = command.phone {
            hashed_identifiers.insert("phone".to_string(), hash_phone(phone));
        }

        let subject_key = subject_key_ref(&command.subject);
        let (value, value_source, payment_event_id) = self.resolve_value(scope, &command).await?;
        let event_id = payment_event_id.clone().unwrap_or(command.event_id.clone());
        let dedupe_key = conversion_dedupe_key(
            command.tenant_id,
            command.order_ref.as_deref(),
            &command.event_name,
            &event_id,
        );

        Ok(ConversionEvent {
            id: self.ids.next_conversion_id()?,
            tenant_id: command.tenant_id,
            event_id,
            dedupe_key,
            name: command.event_name,
            occurred_at: Utc::now(),
            value,
            value_source,
            order_ref: command.order_ref,
            click_ids: command.click_ids,
            hashed_identifiers,
            consent,
            upload_states: BTreeMap::new(),
            subject_key,
        })
    }

    /// The payment record is the trustworthy value wherever one exists; a
    /// client-supplied value is only ever a fallback hint, and is marked
    /// as such (NFR-1010).
    async fn resolve_value(
        &self,
        scope: &mut ScopedTx<'_>,
        command: &TrackConversionCommand,
    ) -> Result<(Option<Money>, Option<ValueSource>, Option<String>), AdvertisingError> {
        if let Some(order_ref) = command.order_ref.as_deref() {
            // A failed lookup is not "no payment record". Falling through
            // to the client's hint here would label an untrusted value as
            // the event's value on a transient database error — exactly
            // the substitution NFR-1010 forbids — and the failed statement
            // has already aborted this transaction, so the insert that
            // follows could not have succeeded anyway.
            if let Some(payment) = self
                .payment_lookup
                .lookup_payment_value(scope, command.tenant_id, order_ref)
                .await?
            {
                return Ok((
                    Some(payment.value),
                    Some(ValueSource::PaymentRecord),
                    Some(payment.conversion_event_id),
                ));
            }
        }

        Ok(match (command.value, command.currency.as_deref()) {
            (Some(amount_minor), Some(currency)) => match Money::new(amount_minor, currency) {
                Ok(money) => (Some(money), Some(ValueSource::ClientReported), None),
                Err(_) => (None, None, None),
            },
            _ => (None, None, None),
        })
    }

    /// Every event asks `ads_measurement`, plus `sale_or_share` and
    /// `targeted_advertising` for any mapped platform whose sub-processor
    /// role is `ThirdParty` rather than `ServiceProvider`. The first
    /// denial found suppresses the event; the denying purpose is recorded
    /// in `answers` and its directive's signal source in `signal_source`,
    /// so the frozen snapshot alone says what suppressed the event and why
    /// — nothing has to re-ask the resolver to find out.
    ///
    /// Where no denial is found, `signal_source` is the *first* asked
    /// purpose's source (`ads_measurement`), not the last.
    ///
    /// A `platforms_incomplete` command asks for those extra purposes
    /// regardless. The decision is frozen onto the event here and never
    /// re-derived, so an event captured before its mapping existed would
    /// otherwise carry a proof that never asked about the vendor it later
    /// gets uploaded to.
    async fn resolve_gate(
        &self,
        command: &TrackConversionCommand,
    ) -> Result<ConsentSnapshot, AdvertisingError> {
        let mut required_purposes = vec![ProcessingPurpose::AdsMeasurement];
        let mut reaches_third_party = command.platforms_incomplete;
        for platform in &command.platforms {
            if self.vendor_role.lookup_role(platform).await? == VendorRole::ThirdParty {
                reaches_third_party = true;
            }
        }
        if reaches_third_party {
            required_purposes.push(ProcessingPurpose::SaleOrShare);
            required_purposes.push(ProcessingPurpose::TargetedAdvertising);
        }

        let mut answers = BTreeMap::new();
        let mut jurisdiction = String::new();
        let mut signal_source = String::new();
        let mut first_source = true;
        let mut denied_found = false;

        for purpose in &required_purposes {
            let directive = self
                .directive_resolver
                .resolve(&command.subject, *purpose)
                .await
                .map_err(|err| {
                    AdvertisingError::InvalidInput(format!("privacy resolver error: {err:?}"))
                })?;

            answers.insert(
                purpose.as_str().to_string(),
                directive.state.as_str().to_string(),
            );
            jurisdiction = directive.jurisdiction.as_str().to_string();

            if !denied_found {
                if first_source {
                    signal_source = directive.source.as_str().to_string();
                    first_source = false;
                }
                if directive.state == DirectiveState::Denied {
                    signal_source = directive.source.as_str().to_string();
                    denied_found = true;
                }
            }
        }

        let consent = ConsentSnapshot {
            resolver_version: CONSENT_GATE_VERSION.to_string(),
            jurisdiction,
            purposes_asked: required_purposes
                .iter()
                .map(|p| p.as_str().to_string())
                .collect(),
            answers,
            signal_source,
        };

        Ok(consent)
    }
}
