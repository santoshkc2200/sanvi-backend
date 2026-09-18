//! The renewal sweep (worker job): three independent concerns —
//!
//! 1. **Certificate renewal**: certs approaching `not_after` renew through
//!    the CA port; failures record, escalate via events and notify.
//! 2. **Expiry warnings**: T-30/T-14/T-7/T-1 events fire exactly once per
//!    threshold per certificate.
//! 3. **Registry auto-renew**: orders with `auto_renew` charge and renew
//!    through the registrar, with retry windows and escalation on
//!    exhaustion.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use sanvi_persistence::TxManager;

use crate::domain::{
    CertificateAuthorityPort, CertificateRepository, CustomDomainRepository, DomainOrderRepository,
    DomainRegistrarPort, DomainsError, DomainsEvent, EventPublisher, PurchaseCharger, storage,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenewalReport {
    pub certs_renewed: u32,
    pub cert_renewal_failures: u32,
    pub expiry_warnings: u32,
    pub orders_renewed: u32,
    pub order_renewal_failures: u32,
    pub order_renewals_escalated: u32,
}

pub struct RunRenewalSweep {
    domains: Arc<dyn CustomDomainRepository>,
    certs: Arc<dyn CertificateRepository>,
    orders: Arc<dyn DomainOrderRepository>,
    ca: Arc<dyn CertificateAuthorityPort>,
    registrar: Arc<dyn DomainRegistrarPort>,
    charger: Arc<dyn PurchaseCharger>,
    events: Arc<dyn EventPublisher>,
    tx: TxManager,
    renew_before_days: u32,
    expiry_warning_days: Vec<u32>,
    renewal_lead_days: u32,
    renewal_retry_secs: u64,
    renewal_max_attempts: u32,
}

impl RunRenewalSweep {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        domains: Arc<dyn CustomDomainRepository>,
        certs: Arc<dyn CertificateRepository>,
        orders: Arc<dyn DomainOrderRepository>,
        ca: Arc<dyn CertificateAuthorityPort>,
        registrar: Arc<dyn DomainRegistrarPort>,
        charger: Arc<dyn PurchaseCharger>,
        events: Arc<dyn EventPublisher>,
        tx: TxManager,
        renew_before_days: u32,
        expiry_warning_days: Vec<u32>,
        renewal_lead_days: u32,
        renewal_retry_secs: u64,
        renewal_max_attempts: u32,
    ) -> Self {
        Self {
            domains,
            certs,
            orders,
            ca,
            registrar,
            charger,
            events,
            tx,
            renew_before_days,
            expiry_warning_days,
            renewal_lead_days,
            renewal_retry_secs,
            renewal_max_attempts,
        }
    }

    #[tracing::instrument(name = "domains.run_renewal_sweep", skip_all)]
    pub async fn execute(
        &self,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<RenewalReport, DomainsError> {
        let mut report = RenewalReport::default();
        self.renew_certificates(now, limit, &mut report).await?;
        self.warn_expiring(now, limit, &mut report).await?;
        self.renew_orders(now, limit, &mut report).await?;
        Ok(report)
    }

    // -- certificates -------------------------------------------------------

    async fn renew_certificates(
        &self,
        now: DateTime<Utc>,
        limit: u32,
        report: &mut RenewalReport,
    ) -> Result<(), DomainsError> {
        let mut scope = self
            .tx
            .begin_unscoped("domains.renewal_sweep.certs", "domains-worker")
            .await
            .map_err(storage)?;
        let due = self
            .certs
            .due_for_renewal(&mut scope, now, self.renew_before_days, limit)
            .await?;
        scope.commit().await.map_err(storage)?;

        for mut cert in due {
            let mut scope = self
                .tx
                .begin_unscoped("domains.renewal_sweep.domain", "domains-worker")
                .await
                .map_err(storage)?;
            let domain = self
                .domains
                .platform_find(&mut scope, cert.domain_id)
                .await?;
            scope.commit().await.map_err(storage)?;

            let Some(domain) = domain else {
                // Domain gone (removed): revoke the orphaned cert.
                cert.revoke();
                let mut scope = self
                    .tx
                    .begin_unscoped("domains.renewal_sweep.orphan", "domains-worker")
                    .await
                    .map_err(storage)?;
                self.certs.update_platform(&mut scope, &cert).await?;
                scope.commit().await.map_err(storage)?;
                continue;
            };

            match self.ca.renew(&cert, &domain).await {
                Ok(renewed) => {
                    let mut scope = self
                        .tx
                        .begin_unscoped("domains.renewal_sweep.renewed", "domains-worker")
                        .await
                        .map_err(storage)?;
                    self.certs.update_platform(&mut scope, &renewed).await?;
                    self.events
                        .publish(
                            &mut scope,
                            cert.tenant_id,
                            &[DomainsEvent::CertificateRenewed {
                                domain_id: cert.domain_id,
                                hostname: cert.hostname.as_str().to_string(),
                                not_after: renewed.not_after,
                            }],
                        )
                        .await?;
                    scope.commit().await.map_err(storage)?;
                    report.certs_renewed += 1;
                }
                Err(err) => {
                    cert.mark_renewal_error(err.to_string());
                    let mut scope = self
                        .tx
                        .begin_unscoped("domains.renewal_sweep.failed", "domains-worker")
                        .await
                        .map_err(storage)?;
                    self.certs.update_platform(&mut scope, &cert).await?;
                    self.events
                        .publish(
                            &mut scope,
                            cert.tenant_id,
                            &[DomainsEvent::CertificateRenewalFailed {
                                domain_id: cert.domain_id,
                                hostname: cert.hostname.as_str().to_string(),
                                not_after: cert.not_after,
                                detail: err.to_string(),
                            }],
                        )
                        .await?;
                    scope.commit().await.map_err(storage)?;
                    report.cert_renewal_failures += 1;
                }
            }
        }
        Ok(())
    }

    async fn warn_expiring(
        &self,
        now: DateTime<Utc>,
        limit: u32,
        report: &mut RenewalReport,
    ) -> Result<(), DomainsError> {
        let mut scope = self
            .tx
            .begin_unscoped("domains.renewal_sweep.expiry", "domains-worker")
            .await
            .map_err(storage)?;
        let active = self.certs.active(&mut scope, limit).await?;
        scope.commit().await.map_err(storage)?;

        for mut cert in active {
            let days = cert.days_to_expiry(now);
            let threshold = self
                .expiry_warning_days
                .iter()
                .copied()
                .filter(|threshold| days <= *threshold)
                .max();
            let Some(threshold) = threshold else {
                continue;
            };
            if cert.expiry_warned_days == Some(threshold) {
                continue;
            }
            cert.expiry_warned_days = Some(threshold);
            let mut scope = self
                .tx
                .begin_unscoped("domains.renewal_sweep.warn", "domains-worker")
                .await
                .map_err(storage)?;
            self.certs.update_platform(&mut scope, &cert).await?;
            self.events
                .publish(
                    &mut scope,
                    cert.tenant_id,
                    &[DomainsEvent::DomainExpiringSoon {
                        domain_id: cert.domain_id,
                        hostname: cert.hostname.as_str().to_string(),
                        days_left: days,
                    }],
                )
                .await?;
            scope.commit().await.map_err(storage)?;
            report.expiry_warnings += 1;
        }
        Ok(())
    }

    // -- registry orders ----------------------------------------------------

    async fn renew_orders(
        &self,
        now: DateTime<Utc>,
        limit: u32,
        report: &mut RenewalReport,
    ) -> Result<(), DomainsError> {
        let mut scope = self
            .tx
            .begin_unscoped("domains.renewal_sweep.orders", "domains-worker")
            .await
            .map_err(storage)?;
        let due = self.orders.due_for_renewal(&mut scope, now, limit).await?;
        scope.commit().await.map_err(storage)?;

        for mut order in due {
            let Some(expires_at) = order.expires_at else {
                continue;
            };
            let lead = Duration::days(self.renewal_lead_days as i64);
            if expires_at - now > lead {
                continue;
            }
            let Some(external_ref) = order.external_order_ref.clone() else {
                continue;
            };
            let renew_price = self
                .registrar
                .catalog()
                .into_iter()
                .find(|price| {
                    order
                        .hostname
                        .as_str()
                        .ends_with(&format!(".{}", price.tld))
                })
                .map(|price| price.renew)
                .unwrap_or_else(|| order.price.clone());

            // Charge (idempotent on order id + attempt) then renew.
            match self
                .charger
                .charge(
                    order.tenant_id,
                    order.id,
                    renew_price.minor,
                    &renew_price.currency,
                    &format!("domain renewal: {}", order.hostname.as_str()),
                )
                .await
            {
                Ok(_charge) => {}
                Err(err) => {
                    self.fail_order_renewal(now, &mut order, &err.to_string(), report)
                        .await?;
                    continue;
                }
            }

            match self
                .registrar
                .renew(&external_ref, &order.hostname, order.term_years)
                .await
            {
                Ok(renewal) => {
                    order.mark_renewed(renewal.expires_at, now)?;
                    let mut scope = self
                        .tx
                        .begin_unscoped("domains.renewal_sweep.order_ok", "domains-worker")
                        .await
                        .map_err(storage)?;
                    self.orders.update_platform(&mut scope, &order).await?;
                    scope.commit().await.map_err(storage)?;
                    report.orders_renewed += 1;
                }
                Err(err) => {
                    self.fail_order_renewal(now, &mut order, &err.to_string(), report)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn fail_order_renewal(
        &self,
        now: DateTime<Utc>,
        order: &mut crate::domain::DomainOrder,
        detail: &str,
        report: &mut RenewalReport,
    ) -> Result<(), DomainsError> {
        order.mark_renewal_failed(detail.to_string(), now, self.renewal_retry_secs)?;
        let mut scope = self
            .tx
            .begin_unscoped("domains.renewal_sweep.order_fail", "domains-worker")
            .await
            .map_err(storage)?;
        self.orders.update_platform(&mut scope, order).await?;
        scope.commit().await.map_err(storage)?;
        report.order_renewal_failures += 1;

        if order.renewal_attempts >= self.renewal_max_attempts {
            self.events_platform(
                order.tenant_id,
                &[DomainsEvent::DomainOrderFailed {
                    order_id: order.id,
                    hostname: order.hostname.as_str().to_string(),
                    detail: format!(
                        "auto-renewal exhausted after {} attempts: {detail}",
                        order.renewal_attempts
                    ),
                }],
            )
            .await?;
            report.order_renewals_escalated += 1;
        }
        Ok(())
    }

    async fn events_platform(
        &self,
        tenant_id: sanvi_kernel::TenantId,
        events: &[DomainsEvent],
    ) -> Result<(), DomainsError> {
        let mut scope = self
            .tx
            .begin_unscoped("domains.renewal_sweep.events", "domains-worker")
            .await
            .map_err(storage)?;
        self.events.publish(&mut scope, tenant_id, events).await?;
        scope.commit().await.map_err(storage)
    }
}
