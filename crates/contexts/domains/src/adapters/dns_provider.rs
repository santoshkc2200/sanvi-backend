//! The reference DNS provider adapter: zones and records for purchased
//! domains live in the `domains.dns_records` table (the sandbox deployment's
//! "managed DNS"). A production adapter (Cloudflare, Route 53, …) implements
//! the same port; the verification worker treats this store as the
//! authoritative source for purchased domains.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use sanvi_persistence::{PlatformScope, ScopedTx};

use crate::domain::{DnsProviderPort, DnsRecord, DnsRecordRepository, DomainsError, Hostname};

#[derive(Clone)]
pub struct SqlxDnsProvider {
    records: Arc<dyn DnsRecordRepository>,
}

impl SqlxDnsProvider {
    pub fn new(records: Arc<dyn DnsRecordRepository>) -> Self {
        Self { records }
    }
}

#[async_trait]
impl DnsProviderPort for SqlxDnsProvider {
    async fn create_zone(
        &self,
        _scope: &mut ScopedTx<'_>,
        _hostname: &Hostname,
    ) -> Result<(), DomainsError> {
        // The sandbox provider has no zone registry: the records table is
        // the zone.
        Ok(())
    }

    async fn apply_records(
        &self,
        scope: &mut ScopedTx<'_>,
        domain_id: Uuid,
        records: &[DnsRecord],
    ) -> Result<(), DomainsError> {
        self.records.upsert(scope, domain_id, records).await
    }

    async fn records(
        &self,
        scope: &mut ScopedTx<'_>,
        domain_id: Uuid,
    ) -> Result<Vec<DnsRecord>, DomainsError> {
        // The tenant scope filters rows; the tenant id argument is a
        // placeholder kept for parity with the platform variant.
        self.records.list(scope, Uuid::nil(), domain_id).await
    }

    async fn records_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        domain_id: Uuid,
    ) -> Result<Vec<DnsRecord>, DomainsError> {
        self.records.list_platform(scope, domain_id).await
    }

    async fn remove_zone(
        &self,
        scope: &mut ScopedTx<'_>,
        domain_id: Uuid,
    ) -> Result<(), DomainsError> {
        self.records.remove_zone(scope, domain_id).await
    }

    async fn remove_zone_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        domain_id: Uuid,
    ) -> Result<(), DomainsError> {
        self.records.remove_zone_platform(scope, domain_id).await
    }
}
