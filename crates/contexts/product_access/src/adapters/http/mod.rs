pub mod handlers;
pub mod routes;

use std::sync::Arc;

use crate::application::ProxyRequest;

#[derive(Clone)]
pub struct ProductAccessState {
    pub ecommerce: Arc<ProxyRequest>,
}
