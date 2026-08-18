//! The order sweep (worker job): the purchase saga's safety net.
//!
//! Anything the request path could not finish resumes here, idempotently:
//! charged-but-not-registered orders register (keyed on the order id),
//! registered/configuring orders retry DNS application, and failed
//! charges with a pending refund retry the refund until it lands.

use std::sync::Arc;

use sanvi_persistence::TxManager;

use crate::domain::{
    DomainOrder, DomainOrderRepository, DomainRegistrarPort, DomainsError, PurchaseCharger, storage,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OrderSweepReport {
    pub resumed: u32,
    pub configured: u32,
    pub refunded: u32,
    pub failed: u32,
}

pub struct RunOrderSweep {
    orders: Arc<dyn DomainOrderRepository>,
    registrar: Arc<dyn DomainRegistrarPort>,
    charger: Arc<dyn PurchaseCharger>,
    configure: Arc<crate::application::PlaceDomainOrder>,
    tx: TxManager,
}

impl RunOrderSweep {
    pub fn new(
        orders: Arc<dyn DomainOrderRepository>,
        registrar: Arc<dyn DomainRegistrarPort>,
        charger: Arc<dyn PurchaseCharger>,
        configure: Arc<crate::application::PlaceDomainOrder>,
        tx: TxManager,
    ) -> Self {
        Self {
            orders,
            registrar,
            charger,
            configure,
            tx,
        }
    }

    pub async fn execute(&self, limit: u32) -> Result<OrderSweepReport, DomainsError> {
        let mut scope = self
            .tx
            .begin_unscoped("domains.order_sweep", "domains-worker")
            .await
            .map_err(storage)?;
        let due = self.orders.due_for_advancement(&mut scope, limit).await?;
        scope.commit().await.map_err(storage)?;

        let mut report = OrderSweepReport::default();
        for order in due {
            match self.advance(&order).await {
                Ok(advanced) => {
                    if advanced.resumed_registration {
                        report.resumed += 1;
                    }
                    if advanced.configured {
                        report.configured += 1;
                    }
                }
                Err(err) => {
                    report.failed += 1;
                    tracing::warn!(
                        order_id = %order.id,
                        hostname = order.hostname.as_str(),
                        error = %err,
                        "order sweep could not advance the order; it stays queued for the next sweep"
                    );
                }
            }
        }
        // Refund retries for the "charged, registration failed, refund
        // pending" corner case.
        let mut scope = self
            .tx
            .begin_unscoped("domains.order_sweep.refunds", "domains-worker")
            .await
            .map_err(storage)?;
        let refund_due = self.orders.due_for_refund_retry(&mut scope, limit).await?;
        scope.commit().await.map_err(storage)?;
        for mut order in refund_due {
            let Some(charge_id) = order.charge_id.clone() else {
                continue;
            };
            match self
                .charger
                .refund(order.tenant_id, order.id, &charge_id)
                .await
            {
                Ok(()) => {
                    order.mark_refunded(
                        "registration failed; refunded on retry".to_string(),
                        chrono::Utc::now(),
                    )?;
                    let mut scope = self
                        .tx
                        .begin_unscoped("domains.order_sweep.refunded", "domains-worker")
                        .await
                        .map_err(storage)?;
                    self.orders.update_platform(&mut scope, &order).await?;
                    scope.commit().await.map_err(storage)?;
                    report.refunded += 1;
                }
                Err(err) => {
                    tracing::error!(
                        order_id = %order.id,
                        error = %err,
                        "refund retry failed; escalating to platform operations"
                    );
                    report.failed += 1;
                }
            }
        }
        Ok(report)
    }

    /// Returns what the sweep completed for this order.
    async fn advance(&self, order: &DomainOrder) -> Result<Advanced, DomainsError> {
        let mut advanced = Advanced::default();

        // Charged without an external ref: the registrar call never
        // confirmed — resume it (idempotent on the order id).
        let order = if order.status == crate::domain::OrderStatus::Charged
            && order.external_order_ref.is_none()
        {
            match self
                .registrar
                .register(
                    order.id,
                    &order.hostname,
                    order.term_years,
                    order.whois_privacy,
                )
                .await
            {
                Ok(registration) => {
                    let mut order = order.clone();
                    order.mark_registered(
                        registration.external_ref,
                        registration.registered_at,
                        registration.expires_at,
                        chrono::Utc::now(),
                    )?;
                    let mut scope = self
                        .tx
                        .begin_unscoped("domains.order_sweep.registered", "domains-worker")
                        .await
                        .map_err(storage)?;
                    self.orders.update_platform(&mut scope, &order).await?;
                    scope.commit().await.map_err(storage)?;
                    advanced.resumed_registration = true;
                    order
                }
                Err(err) => return Err(err),
            }
        } else {
            order.clone()
        };

        // Registered/configuring without an active claim: retry DNS.
        if matches!(
            order.status,
            crate::domain::OrderStatus::Registered | crate::domain::OrderStatus::Configuring
        ) {
            self.configure
                .configure_dns(order.tenant_id, &order, &order.hostname)
                .await?;
            advanced.configured = true;
        }

        Ok(advanced)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Advanced {
    resumed_registration: bool,
    configured: bool,
}
