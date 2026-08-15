//! Provision a new tenant (operator-initiated until self-service lands in
//! phases 03/04).

use std::sync::Arc;

use sanvi_persistence::TxManager;
use serde::Deserialize;
use tracing::info;
use utoipa::ToSchema;

use crate::application::get_tenant::TenantView;
use crate::domain::tenant::Tenant;
use crate::domain::values::{DataRegion, Locale, TenantName, TenantSlug};
use crate::domain::{
    Clock, EventPublisher, IdGenerator, SlugReservationPort, TenancyError, TenantRepository,
};

use super::storage;

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ProvisionTenantCommand {
    #[schema(example = "acme")]
    pub slug: String,
    #[schema(example = "Acme Corporation")]
    pub display_name: String,
    #[schema(example = "us", value_type = String)]
    pub region: String,
    #[schema(example = "en", value_type = String)]
    #[serde(default = "default_locale")]
    pub default_locale: String,
}

fn default_locale() -> String {
    Locale::DEFAULT.to_string()
}

/// The actor recorded in the platform audit log for this operation.
const ACTOR: &str = "platform-admin";

pub struct ProvisionTenant {
    tx_manager: TxManager,
    tenants: Arc<dyn TenantRepository>,
    slugs: Arc<dyn SlugReservationPort>,
    events: Arc<dyn EventPublisher>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
}

impl ProvisionTenant {
    pub fn new(
        tx_manager: TxManager,
        tenants: Arc<dyn TenantRepository>,
        slugs: Arc<dyn SlugReservationPort>,
        events: Arc<dyn EventPublisher>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            tenants,
            slugs,
            events,
            ids,
            clock,
        }
    }

    pub async fn execute(&self, cmd: ProvisionTenantCommand) -> Result<TenantView, TenancyError> {
        let slug = TenantSlug::parse(&cmd.slug)?;
        let display_name = TenantName::parse(&cmd.display_name)?;
        let region = DataRegion::parse(&cmd.region)?;
        let default_locale = Locale::parse(&cmd.default_locale)?;
        let now = self.clock.now();

        let mut scope = self
            .tx_manager
            .begin_unscoped("tenant_provision", ACTOR)
            .await
            .map_err(storage)?;

        // Reserve first: concurrent provisions of the same slug collide here.
        self.slugs.reserve(&mut scope, &slug).await?;

        let (tenant, events) = Tenant::provision(
            self.ids.next_tenant_id(),
            slug,
            display_name,
            region,
            default_locale,
            now,
        );
        self.tenants.save(&mut scope, &tenant).await?;
        self.events.publish(&mut scope, &events).await?;

        scope.commit().await.map_err(storage)?;
        info!(tenant_id = %tenant.id(), slug = %tenant.slug(), "tenant provisioned");
        Ok(TenantView::from(&tenant))
    }
}

/// Serialization helper re-exported for handlers.
pub type ProvisionResponse = TenantView;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{DateTime, Utc};
    use sanvi_persistence::PgPool;

    use crate::domain::events::TenantEvent;

    use super::*;

    /// Negative-path tests: validation failures happen before any transaction
    /// or port interaction, so a lazy pool that cannot connect proves the
    /// use case rejects bad input without touching storage.
    struct NoopTenants;
    #[async_trait::async_trait]
    impl TenantRepository for NoopTenants {
        async fn save(
            &self,
            _scope: &mut sanvi_persistence::PlatformScope<'_>,
            _tenant: &Tenant,
        ) -> Result<(), TenancyError> {
            unreachable!("input validation must reject before storage")
        }
        async fn find_by_id(
            &self,
            _scope: &mut sanvi_persistence::PlatformScope<'_>,
            _id: crate::domain::TenantId,
        ) -> Result<Option<Tenant>, TenancyError> {
            unreachable!()
        }
        async fn find_by_slug(
            &self,
            _scope: &mut sanvi_persistence::PlatformScope<'_>,
            _slug: &TenantSlug,
        ) -> Result<Option<Tenant>, TenancyError> {
            unreachable!()
        }
        async fn list(
            &self,
            _scope: &mut sanvi_persistence::PlatformScope<'_>,
            _after: Option<crate::domain::TenantId>,
            _limit: u32,
            _status: Option<crate::domain::TenantStatus>,
        ) -> Result<(Vec<Tenant>, Option<crate::domain::TenantId>), TenancyError> {
            unreachable!()
        }
        async fn due_for_purge_request(
            &self,
            _scope: &mut sanvi_persistence::PlatformScope<'_>,
            _before: DateTime<Utc>,
            _limit: u32,
        ) -> Result<Vec<Tenant>, TenancyError> {
            unreachable!()
        }
    }

    struct NoopSlugs;
    #[async_trait::async_trait]
    impl SlugReservationPort for NoopSlugs {
        async fn reserve(
            &self,
            _scope: &mut sanvi_persistence::PlatformScope<'_>,
            _slug: &TenantSlug,
        ) -> Result<(), TenancyError> {
            unreachable!()
        }
        async fn is_available(
            &self,
            _scope: &mut sanvi_persistence::PlatformScope<'_>,
            _slug: &TenantSlug,
        ) -> Result<bool, TenancyError> {
            unreachable!()
        }
    }

    struct NoopEvents;
    #[async_trait::async_trait]
    impl EventPublisher for NoopEvents {
        async fn publish(
            &self,
            _scope: &mut sanvi_persistence::PlatformScope<'_>,
            _events: &[TenantEvent],
        ) -> Result<(), TenancyError> {
            unreachable!()
        }
        async fn publish_scoped(
            &self,
            _scope: &mut sanvi_persistence::ScopedTx<'_>,
            _events: &[TenantEvent],
        ) -> Result<(), TenancyError> {
            unreachable!()
        }
        async fn publish_envelopes_scoped(
            &self,
            _scope: &mut sanvi_persistence::ScopedTx<'_>,
            _envelopes: &[sanvi_kernel::EventEnvelope],
        ) -> Result<(), TenancyError> {
            unreachable!()
        }
    }

    struct FakeIds;
    impl IdGenerator for FakeIds {
        fn next_tenant_id(&self) -> crate::domain::TenantId {
            crate::domain::TenantId::new()
        }
    }

    struct FakeClock;
    impl Clock for FakeClock {
        fn now(&self) -> DateTime<Utc> {
            Utc::now()
        }
    }

    fn use_case() -> ProvisionTenant {
        ProvisionTenant::new(
            TxManager::new(
                PgPool::connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
                    .expect("lazy pool construction never connects"),
            ),
            Arc::new(NoopTenants),
            Arc::new(NoopSlugs),
            Arc::new(NoopEvents),
            Arc::new(FakeIds),
            Arc::new(FakeClock),
        )
    }

    fn valid_cmd() -> ProvisionTenantCommand {
        ProvisionTenantCommand {
            slug: "acme".to_string(),
            display_name: "Acme Corp".to_string(),
            region: "us".to_string(),
            default_locale: "en".to_string(),
        }
    }

    #[tokio::test]
    async fn invalid_slug_is_rejected() {
        let mut cmd = valid_cmd();
        cmd.slug = "!not-a-slug!".to_string();
        assert!(matches!(
            use_case().execute(cmd).await,
            Err(TenancyError::InvalidSlug(_))
        ));
    }

    #[tokio::test]
    async fn reserved_slug_is_rejected() {
        let mut cmd = valid_cmd();
        cmd.slug = "api".to_string();
        assert!(matches!(
            use_case().execute(cmd).await,
            Err(TenancyError::SlugReserved { .. })
        ));
    }

    #[tokio::test]
    async fn invalid_region_is_rejected() {
        let mut cmd = valid_cmd();
        cmd.region = "cn".to_string();
        assert!(matches!(
            use_case().execute(cmd).await,
            Err(TenancyError::InvalidRegion(_))
        ));
    }

    #[tokio::test]
    async fn invalid_name_and_locale_are_rejected() {
        let mut cmd = valid_cmd();
        cmd.display_name = "   ".to_string();
        assert!(matches!(
            use_case().execute(cmd.clone()).await,
            Err(TenancyError::InvalidDisplayName(_))
        ));
        cmd.display_name = "Acme".to_string();
        cmd.default_locale = "-bad-".to_string();
        assert!(matches!(
            use_case().execute(cmd).await,
            Err(TenancyError::InvalidLocale(_))
        ));
    }
}
