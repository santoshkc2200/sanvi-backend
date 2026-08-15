//! Slug availability check for the public endpoint.
//!
//! Returns a bare boolean — never *why* a slug is unavailable, so the API
//! leaks no enumeration signal beyond "try another slug".

use std::sync::Arc;

use sanvi_persistence::TxManager;
use serde::Serialize;
use utoipa::ToSchema;

use crate::domain::values::TenantSlug;
use crate::domain::{SlugReservationPort, TenancyError};

use super::storage;

const ACTOR: &str = "public";

/// The bare-boolean answer to a slug availability question.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct SlugAvailability {
    pub available: bool,
}

pub struct CheckSlugAvailability {
    tx_manager: TxManager,
    slugs: Arc<dyn SlugReservationPort>,
}

impl CheckSlugAvailability {
    pub fn new(tx_manager: TxManager, slugs: Arc<dyn SlugReservationPort>) -> Self {
        Self { tx_manager, slugs }
    }

    /// `true` when the slug is free to claim. Invalid or reserved slugs
    /// answer `false` — the same signal as "taken", on purpose.
    pub async fn execute(&self, raw: &str) -> Result<SlugAvailability, TenancyError> {
        let slug = match TenantSlug::parse(raw) {
            Ok(slug) => slug,
            Err(_) => return Ok(SlugAvailability { available: false }),
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("slug_availability", ACTOR)
            .await
            .map_err(storage)?;
        let available = self.slugs.is_available(&mut scope, &slug).await?;
        scope.commit().await.map_err(storage)?;
        Ok(SlugAvailability { available })
    }
}
