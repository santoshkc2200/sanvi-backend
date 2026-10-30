use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = AccountingProvider)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Freee,
    MoneyForward,
    Xero,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Freee => "freee",
            Self::MoneyForward => "money_forward",
            Self::Xero => "xero",
        }
    }
}

/// Provider account mappings are immutable once a destination is used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountMappings {
    pub revenue_account: String,
    pub refund_account: String,
    pub clearing_account: String,
    pub tax_codes: BTreeMap<String, String>,
}
