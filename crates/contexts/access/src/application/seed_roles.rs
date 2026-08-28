//! Idempotent seeding of the system role catalog.

use std::sync::Arc;

use sanvi_persistence::TxManager;
use tracing::info;

use crate::application::storage;
use crate::domain::roles::{Role, SYSTEM_ROLE_SEEDS};
use crate::domain::{AccessError, Clock, IdGenerator, RoleRepository};

pub struct SeedRoles {
    tx_manager: TxManager,
    roles: Arc<dyn RoleRepository>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
}

impl SeedRoles {
    pub fn new(
        tx_manager: TxManager,
        roles: Arc<dyn RoleRepository>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            roles,
            ids,
            clock,
        }
    }

    /// Ensure every system role exists with its seeded permissions. Returns
    /// the number of roles created/updated.
    pub async fn execute(&self) -> Result<usize, AccessError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("seed_roles", "platform-admin")
            .await
            .map_err(storage)?;
        let now = self.clock.now();
        let mut changed = 0;
        for seed in SYSTEM_ROLE_SEEDS {
            let existing = self
                .roles
                .find_by_key(&mut scope, seed.scope, None, seed.key)
                .await?;
            let permissions: std::collections::BTreeSet<String> =
                seed.permissions.iter().map(|p| p.to_string()).collect();
            match existing {
                None => {
                    let role = Role::new(
                        self.ids.next_role_id()?,
                        seed.scope,
                        None,
                        seed.key.to_string(),
                        seed.name.to_string(),
                        permissions,
                        true,
                        now,
                    )?;
                    self.roles.save(&mut scope, &role).await?;
                    info!(key = seed.key, "seeded system role");
                    changed += 1;
                }
                Some(role) => {
                    // System roles keep exactly the seeded permission set.
                    if role.permissions() != &permissions {
                        let updated = Role::from_parts(
                            role.id(),
                            role.scope(),
                            role.tenant_id(),
                            role.key().to_string(),
                            role.name().to_string(),
                            permissions,
                            true,
                            role.created_at(),
                        );
                        self.roles.save(&mut scope, &updated).await?;
                        info!(key = seed.key, "updated system role permissions");
                        changed += 1;
                    }
                }
            }
        }
        scope.commit().await.map_err(storage)?;
        Ok(changed)
    }
}
