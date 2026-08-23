//! The data map: the declarative registry of personal-data classes, their
//! sensitivity and the retention rules published in the notice. Each context
//! declares which classes it holds via its `PersonalDataProvider`; the CI
//! gate proves every context registers a provider, which is what keeps this
//! map from rotting.

use super::types::{DataClass, LegalBasis, RetentionAction, SensitivityClass};

/// One map entry: the class, its sensitivity and the retention rule the
/// notice publishes (the operational rows live in `privacy.retention_rules`;
/// the sweep reads the database so counsel can adjust periods without a
/// release — these are the seeded defaults).
#[derive(Debug, Clone, Copy)]
pub struct DataClassDef {
    pub class: DataClass,
    pub category: &'static str,
    pub sensitivity: SensitivityClass,
    pub basis: LegalBasis,
    pub period_days: i64,
    pub action: RetentionAction,
}

/// The compile-time half of the map. Retention periods are operating
/// defaults for counsel review; the notice's retention table is generated
/// from the database rows, not from this constant.
pub const DATA_CLASSES: &[DataClassDef] = &[
    DataClassDef {
        class: DataClass::IdentityProfile,
        category: "Account",
        sensitivity: SensitivityClass::Standard,
        basis: LegalBasis::Contract,
        period_days: 2555, // 7 years after the account closes
        action: RetentionAction::Delete,
    },
    DataClassDef {
        class: DataClass::Authentication,
        category: "Account",
        sensitivity: SensitivityClass::Sensitive,
        basis: LegalBasis::Contract,
        period_days: 730,
        action: RetentionAction::Delete,
    },
    DataClassDef {
        class: DataClass::Membership,
        category: "Account",
        sensitivity: SensitivityClass::Standard,
        basis: LegalBasis::Contract,
        period_days: 2555,
        action: RetentionAction::Delete,
    },
    DataClassDef {
        class: DataClass::Contact,
        category: "Contact",
        sensitivity: SensitivityClass::Standard,
        basis: LegalBasis::Contract,
        period_days: 2555,
        action: RetentionAction::Delete,
    },
    DataClassDef {
        class: DataClass::Billing,
        category: "Financial",
        sensitivity: SensitivityClass::Sensitive,
        basis: LegalBasis::Contract,
        period_days: 2555,
        action: RetentionAction::Delete,
    },
    DataClassDef {
        class: DataClass::Invoice,
        category: "Financial",
        sensitivity: SensitivityClass::Standard,
        basis: LegalBasis::StatutoryRetention,
        period_days: 3650, // statutory: invoices survive erasure requests
        action: RetentionAction::Archive,
    },
    // Phase-09 statutory override (09.7): payment transactions on a
    // tenant's connected account outlive an erasure request — tax and
    // accounting law requires the transaction record even when the payer
    // is erased around it. The record itself carries no card data and no
    // required PII; the payments provider anonymises what it can and
    // reports the retained remainder explicitly.
    DataClassDef {
        class: DataClass::TransactionRecord,
        category: "Financial",
        sensitivity: SensitivityClass::Sensitive,
        basis: LegalBasis::StatutoryRetention,
        period_days: 3650,
        action: RetentionAction::Archive,
    },
    DataClassDef {
        class: DataClass::AuditLog,
        category: "Security",
        sensitivity: SensitivityClass::Standard,
        basis: LegalBasis::LegalObligation,
        period_days: 3650,
        action: RetentionAction::Anonymise,
    },
    DataClassDef {
        class: DataClass::ConsentRecord,
        category: "Compliance",
        sensitivity: SensitivityClass::Standard,
        basis: LegalBasis::LegalObligation,
        period_days: 3650, // proof of consent must outlive the consent
        action: RetentionAction::Anonymise,
    },
    DataClassDef {
        class: DataClass::Preference,
        category: "Compliance",
        sensitivity: SensitivityClass::Standard,
        basis: LegalBasis::LegalObligation,
        period_days: 365,
        action: RetentionAction::Delete,
    },
    DataClassDef {
        class: DataClass::Communication,
        category: "Contact",
        sensitivity: SensitivityClass::Standard,
        basis: LegalBasis::Contract,
        period_days: 730,
        action: RetentionAction::Delete,
    },
    DataClassDef {
        class: DataClass::Jurisdiction,
        category: "Compliance",
        sensitivity: SensitivityClass::Standard,
        basis: LegalBasis::LegalObligation,
        period_days: 3650,
        action: RetentionAction::Anonymise,
    },
    DataClassDef {
        class: DataClass::RequestRecord,
        category: "Compliance",
        sensitivity: SensitivityClass::Standard,
        basis: LegalBasis::LegalObligation,
        period_days: 3650,
        action: RetentionAction::Anonymise,
    },
];

/// Look up a class definition (used to seed `privacy.retention_rules`).
pub fn class_def(class: DataClass) -> Option<&'static DataClassDef> {
    DATA_CLASSES.iter().find(|def| def.class == class)
}
