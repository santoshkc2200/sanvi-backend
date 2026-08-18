//! The domains context's HTTP state: one `Arc` per use case, assembled in
//! the composition root.

use std::sync::Arc;

use sanvi_persistence::PgPool;

use crate::adapters::{DomainsMetrics, LiveDomainCache};
use crate::application::{
    AuthorizeTls, ClaimCustomDomain, GetDomainInstructions, ListCustomDomains, ListDomainOrders,
    LiveDomainLookupService, PlaceDomainOrder, PlatformListDomains, PlatformReleaseDomain,
    PromotePrimaryDomain, RemoveCustomDomain, RequestVerification, SearchDomains,
    SetOrderAutoRenew,
};
use crate::config::DomainsConfig;

#[derive(Clone)]
pub struct DomainsState {
    pub pool: PgPool,
    pub config: DomainsConfig,
    pub metrics: Arc<DomainsMetrics>,
    pub lookup: Arc<LiveDomainLookupService>,
    pub cache: Arc<LiveDomainCache>,
    pub claim: Arc<ClaimCustomDomain>,
    pub list_domains: Arc<ListCustomDomains>,
    pub list_orders: Arc<ListDomainOrders>,
    pub instructions: Arc<GetDomainInstructions>,
    pub verify: Arc<RequestVerification>,
    pub remove: Arc<RemoveCustomDomain>,
    pub promote: Arc<PromotePrimaryDomain>,
    pub search: Arc<SearchDomains>,
    pub place_order: Arc<PlaceDomainOrder>,
    pub set_auto_renew_on: Arc<SetOrderAutoRenew>,
    pub set_auto_renew_off: Arc<SetOrderAutoRenew>,
    pub authorize_tls: Arc<AuthorizeTls>,
    pub platform_list: Arc<PlatformListDomains>,
    pub platform_release: Arc<PlatformReleaseDomain>,
}
