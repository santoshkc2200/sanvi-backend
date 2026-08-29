//! Transactional outbox registration for advertising events.

use std::sync::Arc;

use async_trait::async_trait;
use sanvi_kernel::{EventId, RawIdGenerator, TenantId};
use sanvi_messaging::OutboxWriter;
use sanvi_persistence::ScopedTx;

use crate::domain::{AdEvent, AdvertisingError, EventPublisher};

#[derive(Clone)]
pub struct OutboxEventPublisher {
    ids: Arc<dyn RawIdGenerator>,
}

impl OutboxEventPublisher {
    pub fn new(ids: Arc<dyn RawIdGenerator>) -> Self {
        Self { ids }
    }
}

#[async_trait]
impl EventPublisher for OutboxEventPublisher {
    async fn publish_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        events: &[AdEvent],
    ) -> Result<(), AdvertisingError> {
        let writer = OutboxWriter;
        for event in events {
            let id = self
                .ids
                .next_id()
                .map(EventId::from_snowflake)
                .map_err(|err| AdvertisingError::IdGeneration(err.to_string()))?;
            writer
                .append(scope.tx_mut(), &event.envelope(tenant_id, id))
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct SystemIdGenerator {
    ids: Arc<dyn RawIdGenerator>,
}

impl SystemIdGenerator {
    pub fn new(ids: Arc<dyn RawIdGenerator>) -> Self {
        Self { ids }
    }

    fn next(&self) -> Result<sanvi_kernel::SnowflakeId, AdvertisingError> {
        self.ids
            .next_id()
            .map_err(|err| AdvertisingError::IdGeneration(err.to_string()))
    }
}

impl crate::domain::IdGenerator for SystemIdGenerator {
    fn next_connection_id(&self) -> Result<crate::domain::model::ConnectionId, AdvertisingError> {
        self.next()
            .map(crate::domain::model::ConnectionId::from_snowflake)
    }

    fn next_campaign_id(&self) -> Result<crate::domain::model::CampaignId, AdvertisingError> {
        self.next()
            .map(crate::domain::model::CampaignId::from_snowflake)
    }

    fn next_campaign_change_id(
        &self,
    ) -> Result<crate::domain::model::CampaignChangeId, AdvertisingError> {
        self.next()
            .map(crate::domain::model::CampaignChangeId::from_snowflake)
    }

    fn next_conversion_id(&self) -> Result<crate::domain::model::ConversionId, AdvertisingError> {
        self.next()
            .map(crate::domain::model::ConversionId::from_snowflake)
    }

    fn next_creative_id(&self) -> Result<crate::domain::model::CreativeId, AdvertisingError> {
        self.next()
            .map(crate::domain::model::CreativeId::from_snowflake)
    }

    fn next_ad_group_id(&self) -> Result<crate::domain::model::AdGroupId, AdvertisingError> {
        self.next()
            .map(crate::domain::model::AdGroupId::from_snowflake)
    }

    fn next_ad_id(&self) -> Result<crate::domain::model::AdId, AdvertisingError> {
        self.next().map(crate::domain::model::AdId::from_snowflake)
    }

    fn next_audience_id(&self) -> Result<crate::domain::model::AudienceId, AdvertisingError> {
        self.next()
            .map(crate::domain::model::AudienceId::from_snowflake)
    }
}
