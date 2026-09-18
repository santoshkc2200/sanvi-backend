//! Tenant-facing queries: their subscription and their invoices. Reads run
//! under the audited platform scope with the tenant id as an explicit
//! parameter (same pattern the access context uses for its platform-scoped
//! entitlement reads).

use std::sync::Arc;

use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;
use serde::Serialize;
use utoipa::ToSchema;

use crate::application::storage;
use crate::domain::{
    BillingError, CollectionState, InvoiceId, InvoiceRef, InvoiceRepository, Period,
    PlanRepository, Subscription, SubscriptionId, SubscriptionRepository, SubscriptionSource,
    SubscriptionStatus,
};

/// The tenant's subscription as served to the billing UI.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SubscriptionView {
    #[schema(value_type = String)]
    pub subscription_id: SubscriptionId,
    pub plan_key: String,
    pub plan_name: String,
    pub source: SubscriptionSource,
    pub status: SubscriptionStatus,
    pub collection_state: CollectionState,
    pub current_period: Option<Period>,
    pub cancel_at_period_end: bool,
    pub trial_end: Option<chrono::DateTime<chrono::Utc>>,
}

pub struct GetSubscription {
    tx_manager: TxManager,
    subscriptions: Arc<dyn SubscriptionRepository>,
    plans: Arc<dyn PlanRepository>,
}

impl GetSubscription {
    pub fn new(
        tx_manager: TxManager,
        subscriptions: Arc<dyn SubscriptionRepository>,
        plans: Arc<dyn PlanRepository>,
    ) -> Self {
        Self {
            tx_manager,
            subscriptions,
            plans,
        }
    }

    #[tracing::instrument(name = "billing.get_subscription", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
    ) -> Result<Option<SubscriptionView>, BillingError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("get_subscription", "tenant")
            .await
            .map_err(storage)?;
        let Some(subscription) = self
            .subscriptions
            .find_by_tenant_platform(&mut scope, tenant_id)
            .await?
        else {
            scope.commit().await.map_err(storage)?;
            return Ok(None);
        };
        let plan = self
            .plans
            .find_by_id(&mut scope, subscription.plan_id())
            .await?
            .ok_or_else(|| BillingError::PlanNotFound(subscription.plan_id().to_string()))?;
        scope.commit().await.map_err(storage)?;
        Ok(Some(to_view(&subscription, &plan)))
    }
}

fn to_view(subscription: &Subscription, plan: &crate::domain::Plan) -> SubscriptionView {
    SubscriptionView {
        subscription_id: subscription.id(),
        plan_key: plan.key().to_string(),
        plan_name: plan.name().to_string(),
        source: subscription.source(),
        status: subscription.status(),
        collection_state: subscription.collection_state(),
        current_period: subscription.current_period(),
        cancel_at_period_end: subscription.cancel_at_period_end(),
        trial_end: subscription.trial_end(),
    }
}

/// An invoice row as served to the billing UI.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct InvoiceView {
    #[schema(value_type = String)]
    pub invoice_id: InvoiceId,
    pub number: Option<String>,
    pub status: crate::domain::InvoiceStatus,
    pub total_minor: i64,
    pub currency: String,
    pub hosted_url: Option<String>,
    pub pdf_url: Option<String>,
    pub period: Option<Period>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub paid_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<&InvoiceRef> for InvoiceView {
    fn from(invoice: &InvoiceRef) -> Self {
        Self {
            invoice_id: invoice.id(),
            number: invoice.number().map(str::to_string),
            status: invoice.status(),
            total_minor: invoice.total_minor(),
            currency: invoice.currency().to_string(),
            hosted_url: invoice.hosted_url().map(str::to_string),
            pdf_url: invoice.pdf_url().map(str::to_string),
            period: invoice.period(),
            created_at: invoice.created_at(),
            paid_at: invoice.paid_at(),
        }
    }
}

pub struct ListInvoices {
    tx_manager: TxManager,
    invoices: Arc<dyn InvoiceRepository>,
}

impl ListInvoices {
    pub fn new(tx_manager: TxManager, invoices: Arc<dyn InvoiceRepository>) -> Self {
        Self {
            tx_manager,
            invoices,
        }
    }

    #[tracing::instrument(name = "billing.list_invoices", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        limit: u32,
    ) -> Result<Vec<InvoiceView>, BillingError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("list_invoices", "tenant")
            .await
            .map_err(storage)?;
        let invoices = self
            .invoices
            .list_for_tenant_platform(&mut scope, tenant_id, limit)
            .await?;
        scope.commit().await.map_err(storage)?;
        Ok(invoices.iter().map(InvoiceView::from).collect())
    }
}
