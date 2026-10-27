pub mod handlers;
pub mod routes;

use std::sync::Arc;

use crate::application::{ProxyImportRequest, ProxyRequest};

#[derive(Clone)]
pub struct ProductAccessState {
    pub ecommerce: Arc<ProxyRequest>,
    pub imports: Arc<ProxyImportRequest>,
}
