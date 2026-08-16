//! The permission registry endpoint (drives the admin/UI contracts).

use serde::Serialize;
use utoipa::ToSchema;

use crate::domain::permissions::{PERMISSIONS, PermissionScope};

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PermissionView {
    pub key: String,
    pub scope: PermissionScope,
    pub description: String,
}

pub struct ListPermissions;

impl ListPermissions {
    pub fn new() -> Self {
        Self
    }

    pub fn execute(&self) -> Vec<PermissionView> {
        PERMISSIONS
            .iter()
            .map(|def| PermissionView {
                key: def.key.to_string(),
                scope: def.scope,
                description: def.description.to_string(),
            })
            .collect()
    }
}

impl Default for ListPermissions {
    fn default() -> Self {
        Self::new()
    }
}
