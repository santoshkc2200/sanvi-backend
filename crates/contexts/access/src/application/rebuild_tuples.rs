//! The full Keto tuple rebuild: roles → permission grants, memberships →
//! role memberships + tenant containment. Runs from the CLI and as the
//! reconciliation path after events fall behind (idempotent: write is a
//! set-operation per tuple).

use std::sync::Arc;

use sanvi_identity::domain::MembershipSource;
use sanvi_persistence::TxManager;
use tracing::{info, warn};

use crate::application::storage;
use crate::domain::AccessError;
use crate::domain::ports::{PermissionStore, RoleRepository, Tuple};

pub struct RebuildAccessTuples {
    tx_manager: TxManager,
    roles: Arc<dyn RoleRepository>,
    store: Arc<dyn PermissionStore>,
    memberships: Arc<dyn MembershipSource>,
}

impl RebuildAccessTuples {
    pub fn new(
        tx_manager: TxManager,
        roles: Arc<dyn RoleRepository>,
        store: Arc<dyn PermissionStore>,
        memberships: Arc<dyn MembershipSource>,
    ) -> Self {
        Self {
            tx_manager,
            roles,
            store,
            memberships,
        }
    }

    /// Reconcile Keto with the role catalog and every membership: clears the
    /// `role`, `tenant` and `permission` namespaces first, then rewrites
    /// everything (a full, idempotent rebuild). NOTE: platform role
    /// *assignments* (Keto-only tuples from `cli membership grant
    /// --platform`) live in the `role` namespace and are wiped too — re-run
    /// the grant after a rebuild (phase 03 gives them a database home).
    #[tracing::instrument(name = "access.rebuild_access_tuples", skip_all)]
    pub async fn execute(&self) -> Result<usize, AccessError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("rebuild_access_tuples", "platform-admin")
            .await
            .map_err(storage)?;
        let roles = self.roles.list_all(&mut scope).await?;
        scope.commit().await.map_err(storage)?;

        let memberships = self
            .memberships
            .all_memberships()
            .await
            .map_err(|err| AccessError::Storage(format!("membership source failure: {err}")))?;

        for namespace in ["role", "tenant", "permission"] {
            self.store.clear_namespace(namespace).await?;
        }

        // 1. Role → permission grants. System tenant templates carry no
        //    tenant id and are materialized per tenant in step 2.
        let mut tuples: Vec<Tuple> = Vec::new();
        for role in &roles {
            if role.scope() == crate::domain::roles::RoleScope::Tenant && role.tenant_id().is_none()
            {
                continue;
            }
            for permission in role.permissions() {
                tuples.push(Tuple::permission_grant(permission, &role.tuple_object()));
            }
        }

        // 2. Memberships → role membership + tenant containment.
        //    System tenant templates materialize for every tenant that has
        //    members, plus platform role memberships exist only when granted
        //    (platform memberships are stored like tenant ones by the CLI).
        let mut tenants: std::collections::BTreeSet<sanvi_kernel::TenantId> =
            std::collections::BTreeSet::new();
        for membership in &memberships {
            tenants.insert(membership.tenant_id());
            tuples.push(Tuple::tenant_membership(
                membership.tenant_id(),
                membership.user_id(),
            ));
            for role_id in membership.role_ids() {
                if let Some(role) = roles.iter().find(|role| role.id() == *role_id) {
                    // System tenant templates materialize per tenant.
                    let object = if role.tenant_id().is_none() {
                        format!("{}:{}", membership.tenant_id(), role.key())
                    } else {
                        role.tuple_object()
                    };
                    tuples.push(Tuple::role_membership(&object, membership.user_id()));
                } else {
                    warn!(role_id = %role_id, "membership references a deleted role; skipping tuple");
                }
            }
        }
        for tenant_id in tenants {
            for role in roles.iter().filter(|role| {
                role.is_system()
                    && role.scope() == crate::domain::roles::RoleScope::Tenant
                    && role.tenant_id().is_none()
            }) {
                let object = format!("{tenant_id}:{}", role.key());
                for permission in role.permissions() {
                    tuples.push(Tuple::permission_grant(permission, &object));
                }
            }
        }

        // Platform role → permission grants already covered in step 1 via
        // their tuple_object(); nothing more to do there.
        let count = tuples.len();
        if !tuples.is_empty() {
            self.store.write(&tuples).await?;
        }
        info!(tuples = count, "access tuples rebuilt");
        Ok(count)
    }
}
