//! Application: use cases. Plain structs over `Arc<dyn Port>` fields with
//! `execute` methods; commands and views are `ToSchema` and live here.

pub mod certs_job;
pub mod claim;
pub mod health_job;
pub mod instructions;
pub mod list;
pub mod lookup;
pub mod orders;
pub mod orders_job;
pub mod platform;
pub mod promote;
pub mod remove;
pub mod renewal_job;
pub mod search;
pub mod tls_authorize;
pub mod verification_job;
pub mod verify;
pub mod views;

pub use certs_job::{CertIssuanceReport, RunCertIssuanceSweep};
pub use claim::{ClaimCustomDomain, ClaimCustomDomainCommand};
pub use health_job::{HealthReport, RunHealthSweep};
pub use instructions::{GetDomainInstructions, InstructionsView};
pub use list::{ListCustomDomains, ListDomainOrders};
pub use lookup::LiveDomainLookupService;
pub use orders::{PlaceDomainOrder, PlaceDomainOrderCommand, SetOrderAutoRenew};
pub use orders_job::{OrderSweepReport, RunOrderSweep};
pub use platform::{PlatformListDomains, PlatformReleaseDomain};
pub use promote::PromotePrimaryDomain;
pub use remove::RemoveCustomDomain;
pub use renewal_job::{RenewalReport, RunRenewalSweep};
pub use search::{DomainSearchView, SearchDomains};
pub use tls_authorize::{AuthorizeDecision, AuthorizeTls};
pub use verification_job::{RunVerificationSweep, VerificationReport};
pub use verify::RequestVerification;
pub use views::{CustomDomainView, GuideView, InstructionRecord, OrderView, SearchView};
