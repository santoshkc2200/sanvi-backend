//! DNS record value objects: what instructions render and what the
//! provider port applies.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The subset of record types the context works with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DnsRecordType {
    A,
    Aaaa,
    Alias,
    Cname,
    Ns,
    Txt,
}

impl DnsRecordType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::Aaaa => "AAAA",
            Self::Alias => "ALIAS",
            Self::Cname => "CNAME",
            Self::Ns => "NS",
            Self::Txt => "TXT",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_uppercase().as_str() {
            "A" => Some(Self::A),
            "AAAA" => Some(Self::Aaaa),
            "ALIAS" | "ANAME" => Some(Self::Alias),
            "CNAME" => Some(Self::Cname),
            "NS" => Some(Self::Ns),
            "TXT" => Some(Self::Txt),
            _ => None,
        }
    }
}

/// One DNS record: name (relative to the zone, or absolute), type, value, TTL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DnsRecord {
    pub name: String,
    pub record_type: DnsRecordType,
    pub value: String,
    pub ttl: u32,
}

impl DnsRecord {
    pub fn new(
        name: impl Into<String>,
        record_type: DnsRecordType,
        value: impl Into<String>,
        ttl: u32,
    ) -> Self {
        Self {
            name: name.into(),
            record_type,
            value: value.into(),
            ttl,
        }
    }
}

/// The verification token value published as a TXT record.
pub const VERIFICATION_TXT_PREFIX: &str = "sanvi-verify=";
