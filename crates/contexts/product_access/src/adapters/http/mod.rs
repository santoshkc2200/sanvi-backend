pub mod handlers;
pub mod routes;
pub mod shopper_handlers;

use std::sync::Arc;

use crate::application::{ProxyImportRequest, ProxyRequest, ShopperScope};

#[derive(Clone)]
pub struct ProductAccessState {
    pub ecommerce: Arc<ProxyRequest>,
    pub imports: Arc<ProxyImportRequest>,
    /// The public shopper orchestrations (phase 14).
    pub shopper: Arc<ShopperScope>,
}
