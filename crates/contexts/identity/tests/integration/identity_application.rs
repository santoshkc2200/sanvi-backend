//! Application tests: real use cases over in-memory ports. Covers the
//! account-linking matrix, invitation lifecycle, the linking challenge and
//! membership guards — the phase-02 security-critical logic — without any
//! external infrastructure.

use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use sanvi_identity::application::{
    AcceptInvitation, AcceptInvitationCommand, CompleteLinkChallenge, Flow, GrantMembership,
    GrantMembershipCommand, HandleLogoutWebhook, InviteMember, InviteMemberCommand,
    LinkChallengeResult, RemoveMember, StartLinkChallenge, StartLinkChallengeCommand,
    SyncIdentityFromKratos, SyncIdentityInput, UpdateMemberRoles, UpdateRolesCommand,
};
use sanvi_identity::domain::link_intent::LinkIntent;
use sanvi_identity::domain::user::LinkedVia;
use sanvi_identity::domain::values::{Email, ProviderId, ProviderSubject};
use sanvi_identity::domain::{
    Clock, EventPublisher, IdGenerator, IdentityError, Invitation, InvitationId,
    InvitationRepository, LinkIntentRepository, LinkedAccount, LinkedAccountRepository, Membership,
    MembershipRepository, MembershipStatus, Notifications, RoleCatalog, SessionCache, SessionInfo,
    User, UserId, UserRepository, UserStatus,
};

use sanvi_kernel::{RoleId, TenantId};
use sanvi_persistence::{PlatformScope, ScopedTx, TxManager};

// ---------------------------------------------------------------------------
// In-memory fakes
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Memory {
    users: Vec<User>,
    memberships: Vec<Membership>,
    /// Mirrors `identity.memberships.version`: a per-(tenant, user) counter
    /// bumped on every save and carried into the emitted event, so tests
    /// exercise the same ordering the access sync consumer relies on.
    membership_versions: std::collections::HashMap<(TenantId, UserId), i64>,
    links: Vec<LinkedAccount>,
    invitations: Vec<Invitation>,
    intents: Vec<LinkIntent>,
    events: Vec<sanvi_identity::domain::IdentityEvent>,
    invites_sent: Vec<(String, String)>,
    now: chrono::DateTime<Utc>,
    next_id: u64,
}

#[derive(Clone)]
struct World {
    memory: Arc<Mutex<Memory>>,
    trust_verified: bool,
}

impl World {
    fn new() -> Self {
        Self {
            memory: Arc::new(Mutex::new(Memory {
                now: Utc::now(),
                ..Default::default()
            })),
            trust_verified: true,
        }
    }

    fn next_id(&self) -> UserId {
        let mut memory = self.memory.lock().unwrap();
        memory.next_id += 1;
        UserId::try_from(memory.next_id as i64).expect("valid test id")
    }
}

impl Clock for World {
    fn now(&self) -> chrono::DateTime<Utc> {
        self.memory.lock().unwrap().now
    }
}

impl IdGenerator for World {
    fn next_user_id(&self) -> Result<UserId, IdentityError> {
        Ok(self.next_id())
    }

    fn next_invitation_id(&self) -> Result<InvitationId, IdentityError> {
        Ok(InvitationId::try_from(self.next_id().as_i64()).expect("valid test id"))
    }
}

#[async_trait::async_trait]
impl UserRepository for World {
    async fn save(&self, _scope: &mut PlatformScope<'_>, user: &User) -> Result<(), IdentityError> {
        let mut memory = self.memory.lock().unwrap();
        if let Some(existing) = memory.users.iter_mut().find(|u| u.id() == user.id()) {
            *existing = user.clone();
        } else {
            memory.users.push(user.clone());
        }
        Ok(())
    }

    async fn find_by_id(
        &self,
        _scope: &mut PlatformScope<'_>,
        id: UserId,
    ) -> Result<Option<User>, IdentityError> {
        Ok(self
            .memory
            .lock()
            .unwrap()
            .users
            .iter()
            .find(|u| u.id() == id)
            .cloned())
    }

    async fn find_by_kratos(
        &self,
        _scope: &mut PlatformScope<'_>,
        kratos_identity_id: uuid::Uuid,
    ) -> Result<Option<User>, IdentityError> {
        Ok(self
            .memory
            .lock()
            .unwrap()
            .users
            .iter()
            .find(|u| u.kratos_identity_id() == kratos_identity_id)
            .cloned())
    }

    async fn find_by_email(
        &self,
        _scope: &mut PlatformScope<'_>,
        email: &Email,
    ) -> Result<Option<User>, IdentityError> {
        Ok(self
            .memory
            .lock()
            .unwrap()
            .users
            .iter()
            .find(|u| u.primary_email() == email && u.status() == UserStatus::Active)
            .cloned())
    }
}

#[async_trait::async_trait]
impl MembershipRepository for World {
    async fn remove_all_platform(
        &self,
        _scope: &mut PlatformScope<'_>,
        user_id: UserId,
    ) -> Result<usize, IdentityError> {
        let mut memory = self.memory.lock().unwrap();
        let before = memory.memberships.len();
        memory.memberships.retain(|m| m.user_id() != user_id);
        Ok(before - memory.memberships.len())
    }

    async fn save(
        &self,
        _scope: &mut ScopedTx<'_>,
        membership: &Membership,
    ) -> Result<i64, IdentityError> {
        let mut memory = self.memory.lock().unwrap();
        if let Some(existing) = memory.memberships.iter_mut().find(|m| {
            m.tenant_id() == membership.tenant_id() && m.user_id() == membership.user_id()
        }) {
            *existing = membership.clone();
        } else {
            memory.memberships.push(membership.clone());
        }
        let version = memory
            .membership_versions
            .entry((membership.tenant_id(), membership.user_id()))
            .or_insert(0);
        *version += 1;
        Ok(*version)
    }

    async fn find(
        &self,
        _scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        user_id: UserId,
    ) -> Result<Option<Membership>, IdentityError> {
        Ok(self
            .memory
            .lock()
            .unwrap()
            .memberships
            .iter()
            .find(|m| m.tenant_id() == tenant_id && m.user_id() == user_id)
            .cloned())
    }

    async fn list(
        &self,
        _scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<Membership>, IdentityError> {
        Ok(self
            .memory
            .lock()
            .unwrap()
            .memberships
            .iter()
            .filter(|m| m.tenant_id() == tenant_id)
            .cloned()
            .collect())
    }

    async fn list_with_emails(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<(Membership, Email)>, IdentityError> {
        let memberships = MembershipRepository::list(self, scope, tenant_id).await?;
        let memory = self.memory.lock().unwrap();
        Ok(memberships
            .into_iter()
            .map(|m| {
                let email = memory
                    .users
                    .iter()
                    .find(|u| u.id() == m.user_id())
                    .map(|u| u.primary_email().clone())
                    .expect("member must be a known user");
                (m, email)
            })
            .collect())
    }

    async fn remove(
        &self,
        _scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        user_id: UserId,
    ) -> Result<Option<i64>, IdentityError> {
        let mut memory = self.memory.lock().unwrap();
        let before = memory.memberships.len();
        memory
            .memberships
            .retain(|m| !(m.tenant_id() == tenant_id && m.user_id() == user_id));
        if memory.memberships.len() == before {
            return Ok(None);
        }
        let version = memory
            .membership_versions
            .entry((tenant_id, user_id))
            .or_insert(0);
        *version += 1;
        Ok(Some(*version))
    }

    async fn list_all_platform(
        &self,
        _scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<Membership>, IdentityError> {
        Ok(self.memory.lock().unwrap().memberships.clone())
    }

    async fn count_members_platform(
        &self,
        _scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<u32, IdentityError> {
        Ok(self
            .memory
            .lock()
            .unwrap()
            .memberships
            .iter()
            .filter(|m| m.tenant_id() == tenant_id && m.status() == MembershipStatus::Active)
            .count() as u32)
    }

    async fn count_role_holders(
        &self,
        _scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        role_id: RoleId,
    ) -> Result<u32, IdentityError> {
        Ok(self
            .memory
            .lock()
            .unwrap()
            .memberships
            .iter()
            .filter(|m| m.tenant_id() == tenant_id && m.role_ids().contains(&role_id))
            .count() as u32)
    }

    async fn list_for_user_platform(
        &self,
        _scope: &mut PlatformScope<'_>,
        user_id: UserId,
    ) -> Result<Vec<Membership>, IdentityError> {
        Ok(self
            .memory
            .lock()
            .unwrap()
            .memberships
            .iter()
            .filter(|m| m.user_id() == user_id)
            .cloned()
            .collect())
    }
}

#[async_trait::async_trait]
impl LinkedAccountRepository for World {
    async fn remove_all(
        &self,
        _scope: &mut PlatformScope<'_>,
        user_id: UserId,
    ) -> Result<usize, IdentityError> {
        let mut memory = self.memory.lock().unwrap();
        let before = memory.links.len();
        memory.links.retain(|a| a.user_id() != user_id);
        Ok(before - memory.links.len())
    }

    async fn find(
        &self,
        _scope: &mut PlatformScope<'_>,
        provider: &ProviderId,
        subject: &ProviderSubject,
    ) -> Result<Option<LinkedAccount>, IdentityError> {
        Ok(self
            .memory
            .lock()
            .unwrap()
            .links
            .iter()
            .find(|link| link.provider() == provider && link.subject() == subject)
            .cloned())
    }

    async fn link(
        &self,
        _scope: &mut PlatformScope<'_>,
        account: &LinkedAccount,
    ) -> Result<(), IdentityError> {
        self.memory.lock().unwrap().links.push(account.clone());
        Ok(())
    }

    async fn list_for_user(
        &self,
        _scope: &mut PlatformScope<'_>,
        user_id: UserId,
    ) -> Result<Vec<LinkedAccount>, IdentityError> {
        Ok(self
            .memory
            .lock()
            .unwrap()
            .links
            .iter()
            .filter(|link| link.user_id() == user_id)
            .cloned()
            .collect())
    }
}

#[async_trait::async_trait]
impl InvitationRepository for World {
    async fn remove_by_email_platform(
        &self,
        _scope: &mut PlatformScope<'_>,
        email: &str,
    ) -> Result<usize, IdentityError> {
        let mut memory = self.memory.lock().unwrap();
        let before = memory.invitations.len();
        memory
            .invitations
            .retain(|invitation| invitation.email().as_str() != email);
        Ok(before - memory.invitations.len())
    }

    async fn save(
        &self,
        _scope: &mut ScopedTx<'_>,
        invitation: &Invitation,
    ) -> Result<(), IdentityError> {
        let mut memory = self.memory.lock().unwrap();
        if let Some(existing) = memory
            .invitations
            .iter_mut()
            .find(|i| i.id() == invitation.id())
        {
            *existing = invitation.clone();
        } else {
            memory.invitations.push(invitation.clone());
        }
        Ok(())
    }

    async fn find_by_token_hash(
        &self,
        _scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        token_hash: &str,
    ) -> Result<Option<Invitation>, IdentityError> {
        Ok(self
            .memory
            .lock()
            .unwrap()
            .invitations
            .iter()
            .find(|i| i.tenant_id() == tenant_id && i.token_hash() == token_hash)
            .cloned())
    }

    async fn find_by_id(
        &self,
        _scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        id: InvitationId,
    ) -> Result<Option<Invitation>, IdentityError> {
        Ok(self
            .memory
            .lock()
            .unwrap()
            .invitations
            .iter()
            .find(|i| i.tenant_id() == tenant_id && i.id() == id)
            .cloned())
    }

    async fn list(
        &self,
        _scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<Invitation>, IdentityError> {
        Ok(self
            .memory
            .lock()
            .unwrap()
            .invitations
            .iter()
            .filter(|i| i.tenant_id() == tenant_id)
            .cloned()
            .collect())
    }

    async fn remove(
        &self,
        _scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        id: InvitationId,
    ) -> Result<(), IdentityError> {
        let mut memory = self.memory.lock().unwrap();
        memory
            .invitations
            .retain(|i| !(i.tenant_id() == tenant_id && i.id() == id));
        Ok(())
    }
}

#[async_trait::async_trait]
impl LinkIntentRepository for World {
    async fn remove_by_email_platform(
        &self,
        _scope: &mut PlatformScope<'_>,
        email: &str,
    ) -> Result<usize, IdentityError> {
        let mut memory = self.memory.lock().unwrap();
        let before = memory.intents.len();
        memory.intents.retain(|intent| {
            intent
                .payload()
                .email()
                .as_ref()
                .is_none_or(|e| e.as_str() != email)
        });
        Ok(before - memory.intents.len())
    }

    async fn save(
        &self,
        _scope: &mut PlatformScope<'_>,
        intent: &LinkIntent,
    ) -> Result<(), IdentityError> {
        self.memory.lock().unwrap().intents.push(intent.clone());
        Ok(())
    }

    async fn find(
        &self,
        _scope: &mut PlatformScope<'_>,
        nonce_hash: &str,
    ) -> Result<Option<LinkIntent>, IdentityError> {
        Ok(self
            .memory
            .lock()
            .unwrap()
            .intents
            .iter()
            .find(|intent| intent.nonce_hash() == nonce_hash)
            .cloned())
    }

    async fn take(
        &self,
        _scope: &mut PlatformScope<'_>,
        nonce_hash: &str,
    ) -> Result<Option<LinkIntent>, IdentityError> {
        let mut memory = self.memory.lock().unwrap();
        let index = memory
            .intents
            .iter()
            .position(|intent| intent.nonce_hash() == nonce_hash);
        Ok(index.map(|index| memory.intents.remove(index)))
    }
}

#[async_trait::async_trait]
impl EventPublisher for World {
    async fn publish(
        &self,
        _scope: &mut PlatformScope<'_>,
        events: &[sanvi_identity::domain::IdentityEvent],
    ) -> Result<(), IdentityError> {
        self.memory
            .lock()
            .unwrap()
            .events
            .extend(events.iter().cloned());
        Ok(())
    }

    async fn publish_scoped(
        &self,
        _scope: &mut ScopedTx<'_>,
        events: &[sanvi_identity::domain::IdentityEvent],
    ) -> Result<(), IdentityError> {
        self.memory
            .lock()
            .unwrap()
            .events
            .extend(events.iter().cloned());
        Ok(())
    }

    async fn publish_envelopes_scoped(
        &self,
        _scope: &mut ScopedTx<'_>,
        _envelopes: &[sanvi_kernel::EventEnvelope],
    ) -> Result<(), IdentityError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl Notifications for World {
    async fn send_invitation(
        &self,
        _tenant_id: TenantId,
        _invitation_id: sanvi_identity::domain::InvitationId,
        email: &Email,
        _tenant_name: &str,
        invite_url: &str,
    ) -> Result<(), IdentityError> {
        self.memory
            .lock()
            .unwrap()
            .invites_sent
            .push((email.to_string(), invite_url.to_string()));
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct FakeRoleCatalog {
    tenant_owner: RoleId,
    tenant_admin: RoleId,
    tenant_member: RoleId,
}

#[async_trait::async_trait]
impl RoleCatalog for FakeRoleCatalog {
    async fn assignable_roles(
        &self,
        _tenant_id: TenantId,
    ) -> Result<Vec<(RoleId, String)>, IdentityError> {
        Ok(vec![
            (self.tenant_owner, "tenant_owner".to_string()),
            (self.tenant_admin, "tenant_admin".to_string()),
            (self.tenant_member, "tenant_member".to_string()),
        ])
    }

    async fn role_key(
        &self,
        _tenant_id: TenantId,
        role_id: RoleId,
    ) -> Result<Option<String>, IdentityError> {
        Ok(match role_id {
            id if id == self.tenant_owner => Some("tenant_owner".to_string()),
            id if id == self.tenant_admin => Some("tenant_admin".to_string()),
            id if id == self.tenant_member => Some("tenant_member".to_string()),
            _ => None,
        })
    }

    async fn permissions_for_roles(
        &self,
        _tenant_id: TenantId,
        role_ids: &[RoleId],
    ) -> Result<Vec<String>, IdentityError> {
        Ok(role_ids
            .iter()
            .filter_map(|id| match *id {
                id if id == self.tenant_owner => Some("identity.member.read".to_string()),
                id if id == self.tenant_admin => Some("identity.member.read".to_string()),
                id if id == self.tenant_member => Some("tenancy.settings.read".to_string()),
                _ => None,
            })
            .collect())
    }
}

#[async_trait::async_trait]
impl sanvi_identity::domain::KratosClient for World {
    async fn delete_identity(&self, _kratos_identity_id: uuid::Uuid) -> Result<(), IdentityError> {
        Ok(())
    }

    async fn whoami(
        &self,
        _session_token: &str,
        _channel: sanvi_http::SessionChannel,
    ) -> Result<Option<SessionInfo>, IdentityError> {
        Ok(None)
    }

    async fn revoke_session(&self, _session_id: &str) -> Result<(), IdentityError> {
        Ok(())
    }

    async fn set_metadata_user_id(
        &self,
        _kratos_identity_id: uuid::Uuid,
        _user_id: UserId,
    ) -> Result<(), IdentityError> {
        Ok(())
    }

    async fn get_identity(
        &self,
        _kratos_identity_id: uuid::Uuid,
    ) -> Result<Option<SessionInfo>, IdentityError> {
        Ok(None)
    }
}

#[async_trait::async_trait]
impl SessionCache for World {
    async fn get(&self, _token_hash: &str) -> Result<Option<SessionInfo>, IdentityError> {
        Ok(None)
    }

    async fn put(
        &self,
        _token_hash: &str,
        _session: &SessionInfo,
        _ttl_secs: u64,
    ) -> Result<(), IdentityError> {
        Ok(())
    }

    async fn invalidate_session(&self, _session_id: &str) -> Result<(), IdentityError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fixtures & helpers
// ---------------------------------------------------------------------------

struct Fixture {
    world: World,
    roles: FakeRoleCatalog,
    tenant_id: TenantId,
    tx: TxManager,
    // The pool below borrows this database. Dropping the handle drops the
    // database with it, so the fixture has to outlive every query it serves.
    _db: sanvi_testsupport::TestDb,
}

impl Fixture {
    fn new(db: sanvi_testsupport::TestDb) -> Self {
        Self {
            world: World::new(),
            roles: FakeRoleCatalog {
                tenant_owner: RoleId::try_from(101_i64).unwrap(),
                tenant_admin: RoleId::try_from(103_i64).unwrap(),
                tenant_member: RoleId::try_from(102_i64).unwrap(),
            },
            tenant_id: TenantId::try_from(1_i64).unwrap(),
            tx: TxManager::new(db.app_pool(), sanvi_testsupport::test_ids()),
            _db: db,
        }
    }

    fn sync(&self) -> SyncIdentityFromKratos {
        SyncIdentityFromKratos::new(
            self.tx.clone(),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            if self.world.trust_verified {
                vec!["google".to_string()]
            } else {
                vec![]
            },
        )
    }

    fn invite(&self) -> InviteMember {
        InviteMember::new(
            self.tx.clone(),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            Arc::new(self.roles),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            7,
            "https://app.sanvi.dev",
        )
    }

    fn accept(&self) -> AcceptInvitation {
        AcceptInvitation::new(
            self.tx.clone(),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            Arc::new(self.roles),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
        )
    }

    fn start_challenge(&self) -> StartLinkChallenge {
        StartLinkChallenge::new(
            self.tx.clone(),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            600,
        )
    }

    fn complete_challenge(&self) -> CompleteLinkChallenge {
        CompleteLinkChallenge::new(
            self.tx.clone(),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
        )
    }

    fn grant_member(&self) -> GrantMembership {
        GrantMembership::new(
            self.tx.clone(),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            Arc::new(self.roles),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
        )
    }

    fn remove_member(&self) -> RemoveMember {
        RemoveMember::new(
            self.tx.clone(),
            Arc::new(self.world.clone()),
            Arc::new(self.roles),
            Arc::new(self.world.clone()),
        )
    }

    fn update_roles(&self) -> UpdateMemberRoles {
        UpdateMemberRoles::new(
            self.tx.clone(),
            Arc::new(self.world.clone()),
            Arc::new(self.roles),
            Arc::new(self.world.clone()),
        )
    }

    fn logout(&self) -> HandleLogoutWebhook {
        HandleLogoutWebhook::new(
            self.tx.clone(),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
            Arc::new(self.world.clone()),
        )
    }

    async fn seed_user_async(&self, email: &str, verified: bool) -> UserId {
        let input = SyncIdentityInput {
            flow: Flow::Registration,
            kratos_identity_id: uuid::Uuid::now_v7(),
            email: Email::parse(email).unwrap(),
            email_verified: verified,
            locale: None,
            oidc_identifiers: vec![],
            session_id: None,
        };
        self.sync().execute(input).await.unwrap();
        self.world
            .memory
            .lock()
            .unwrap()
            .users
            .iter()
            .find(|u| u.primary_email().as_str() == email)
            .expect("user seeded")
            .id()
    }
}

// ---------------------------------------------------------------------------
// Webhook sync + account-linking matrix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registration_creates_the_projection_and_is_replay_safe() {
    let fixture = Fixture::new(sanvi_testsupport::test_db().await);
    let input = SyncIdentityInput {
        flow: Flow::Registration,
        kratos_identity_id: uuid::Uuid::now_v7(),
        email: Email::parse("alice@example.com").unwrap(),
        email_verified: false,
        locale: None,
        oidc_identifiers: vec![],
        session_id: None,
    };
    let sync = fixture.sync();
    sync.execute(input.clone()).await.unwrap();
    // Replay (Kratos retries webhooks) must be a no-op, not a duplicate.
    sync.execute(input).await.unwrap();
    {
        let memory = fixture.world.memory.lock().unwrap();
        assert_eq!(memory.users.len(), 1);
        assert_eq!(
            memory
                .events
                .iter()
                .filter(|e| matches!(
                    e,
                    sanvi_identity::domain::IdentityEvent::UserRegistered { .. }
                ))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn registration_with_oidc_links_via_registration() {
    let fixture = Fixture::new(sanvi_testsupport::test_db().await);
    let input = SyncIdentityInput {
        flow: Flow::Registration,
        kratos_identity_id: uuid::Uuid::now_v7(),
        email: Email::parse("alice@example.com").unwrap(),
        email_verified: true,
        locale: None,
        oidc_identifiers: vec!["google:sub-1".to_string()],
        session_id: None,
    };
    fixture.sync().execute(input).await.unwrap();
    {
        let memory = fixture.world.memory.lock().unwrap();
        assert_eq!(memory.links.len(), 1);
        assert_eq!(memory.links[0].linked_via(), LinkedVia::Registration);
    }
}

#[tokio::test]
async fn auto_link_only_when_both_sides_are_verified_and_provider_trusted() {
    let mut fixture = Fixture::new(sanvi_testsupport::test_db().await);
    let alice = fixture.seed_user_async("alice@example.com", true).await;

    // Trusted provider + verified email + verified local user → auto-link.
    let input = SyncIdentityInput {
        flow: Flow::Login,
        kratos_identity_id: uuid::Uuid::now_v7(),
        email: Email::parse("alice@example.com").unwrap(),
        email_verified: true,
        locale: None,
        oidc_identifiers: vec!["google:sub-2".to_string()],
        session_id: Some("session-1".to_string()),
    };
    // The new identity belongs to the *same* email; Kratos would have
    // resolved the duplicate. Simulate: an existing identity id.
    let existing_kratos_id = fixture.world.memory.lock().unwrap().users[0].kratos_identity_id();
    let mut input = input;
    input.kratos_identity_id = existing_kratos_id;
    fixture.sync().execute(input).await.unwrap();
    {
        {
            let memory = fixture.world.memory.lock().unwrap();
            assert!(memory.links.iter().any(|link| {
                link.user_id() == alice
                    && link.provider().as_str() == "google"
                    && link.linked_via() == LinkedVia::AutoVerified
            }));
            // Security notice trigger: AccountLinked event with AutoVerified and user email.
            assert!(memory.events.iter().any(|event| matches!(
                event,
                sanvi_identity::domain::IdentityEvent::AccountLinked {
                    user_id,
                    linked_via: LinkedVia::AutoVerified,
                    email: Some(email),
                    ..
                } if *user_id == alice && email.as_str() == "alice@example.com"
            )));
        }
    }

    // Untrusted provider → must not link.
    fixture.world.trust_verified = false;
    let input = SyncIdentityInput {
        flow: Flow::Login,
        kratos_identity_id: existing_kratos_id,
        email: Email::parse("alice@example.com").unwrap(),
        email_verified: true,
        locale: None,
        oidc_identifiers: vec!["google:sub-3".to_string()],
        session_id: None,
    };
    fixture.sync().execute(input).await.unwrap();
    {
        {
            let memory = fixture.world.memory.lock().unwrap();
            assert!(
                !memory
                    .links
                    .iter()
                    .any(|link| link.subject().as_str() == "sub-3")
            );
            assert!(memory.events.iter().any(|event| matches!(
                event,
                sanvi_identity::domain::IdentityEvent::AccountLinkRejected { .. }
            )));
        }
    }

    // Trusted provider but unverified email → must not link.
    fixture.world.trust_verified = true;
    let input = SyncIdentityInput {
        flow: Flow::Login,
        kratos_identity_id: existing_kratos_id,
        email: Email::parse("alice@example.com").unwrap(),
        email_verified: false,
        locale: None,
        oidc_identifiers: vec!["google:sub-4".to_string()],
        session_id: None,
    };
    fixture.sync().execute(input).await.unwrap();
    {
        {
            let memory = fixture.world.memory.lock().unwrap();
            assert!(
                !memory
                    .links
                    .iter()
                    .any(|link| link.subject().as_str() == "sub-4")
            );
        }
    }
}

#[tokio::test]
async fn colliding_registration_never_binds() {
    let fixture = Fixture::new(sanvi_testsupport::test_db().await);
    fixture.seed_user_async("alice@example.com", true).await;

    // A *different* Kratos identity carries the same email (Kratos already
    // refused this flow): we must refuse to bind anything.
    let input = SyncIdentityInput {
        flow: Flow::Registration,
        kratos_identity_id: uuid::Uuid::now_v7(),
        email: Email::parse("alice@example.com").unwrap(),
        email_verified: true,
        locale: None,
        oidc_identifiers: vec!["google:sub-9".to_string()],
        session_id: None,
    };
    fixture.sync().execute(input).await.unwrap();
    {
        let memory = fixture.world.memory.lock().unwrap();
        assert_eq!(memory.users.len(), 1);
        assert!(memory.links.is_empty());
        assert!(memory.events.iter().any(|event| matches!(
            event,
            sanvi_identity::domain::IdentityEvent::AccountLinkRejected { .. }
        )));
    }
}

// ---------------------------------------------------------------------------
// Linking challenge
// ---------------------------------------------------------------------------

#[tokio::test]
async fn challenge_roundtrip_links_and_is_single_use() {
    let fixture = Fixture::new(sanvi_testsupport::test_db().await);
    let alice = fixture.seed_user_async("alice@example.com", false).await;
    let email = Email::parse("alice@example.com").unwrap();

    let start = StartLinkChallengeCommand {
        provider: "google".to_string(),
        subject: "sub-10".to_string(),
        email: "alice@example.com".to_string(),
        kratos_flow_id: None,
    };
    let response = fixture.start_challenge().execute(start).await.unwrap();
    assert_eq!(response.nonce.len(), 64);

    // Wrong email must fail and consume nothing.
    let wrong = Email::parse("bob@example.com").unwrap();
    assert!(matches!(
        fixture
            .complete_challenge()
            .execute(&response.nonce, alice, &wrong)
            .await,
        Err(IdentityError::LinkIntentEmailMismatch)
    ));

    // Correct email links.
    match fixture
        .complete_challenge()
        .execute(&response.nonce, alice, &email)
        .await
    {
        Ok(LinkChallengeResult::Linked { provider, .. }) => {
            assert_eq!(provider.as_str(), "google");
        }
        other => panic!("expected Linked, got {other:?}"),
    }
    {
        let memory = fixture.world.memory.lock().unwrap();
        assert_eq!(memory.links[0].linked_via(), LinkedVia::Challenge);
    }

    // Replay: the intent is gone.
    assert!(matches!(
        fixture
            .complete_challenge()
            .execute(&response.nonce, alice, &email)
            .await,
        Err(IdentityError::LinkIntentUnavailable)
    ));
}

#[tokio::test]
async fn expired_challenge_is_unavailable() {
    let fixture = Fixture::new(sanvi_testsupport::test_db().await);
    let alice = fixture.seed_user_async("alice@example.com", true).await;
    let email = Email::parse("alice@example.com").unwrap();
    let start = StartLinkChallengeCommand {
        provider: "google".to_string(),
        subject: "sub-11".to_string(),
        email: "alice@example.com".to_string(),
        kratos_flow_id: None,
    };
    let response = fixture.start_challenge().execute(start).await.unwrap();
    // Age the clock past the TTL.
    {
        let mut memory = fixture.world.memory.lock().unwrap();
        memory.now += Duration::seconds(601);
    }
    assert!(matches!(
        fixture
            .complete_challenge()
            .execute(&response.nonce, alice, &email)
            .await,
        Err(IdentityError::LinkIntentUnavailable)
    ));
}

// ---------------------------------------------------------------------------
// Invitations + memberships
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invitation_lifecycle_grant_accept_and_guard() {
    let fixture = Fixture::new(sanvi_testsupport::test_db().await);
    let alice = fixture.seed_user_async("alice@example.com", true).await;
    let bob = fixture.seed_user_async("bob@example.com", true).await;
    let bob_email = Email::parse("bob@example.com").unwrap();

    // Grant alice tenant_owner directly.
    fixture
        .grant_member()
        .execute(
            fixture.tenant_id,
            GrantMembershipCommand {
                email: "alice@example.com".to_string(),
                role_ids: vec![fixture.roles.tenant_owner],
            },
            UserId::try_from(999_999_i64).unwrap(),
        )
        .await
        .unwrap();

    // Invite bob as tenant_member.
    let (view, token) = fixture
        .invite()
        .execute(
            fixture.tenant_id,
            InviteMemberCommand {
                email: "bob@example.com".to_string(),
                role_ids: vec![fixture.roles.tenant_member],
            },
            alice,
        )
        .await
        .unwrap();
    assert_eq!(token.len(), 64);
    assert_eq!(view.email, "bob@example.com");
    assert_eq!(fixture.world.memory.lock().unwrap().invites_sent.len(), 1);

    // Accept with the wrong email → refused.
    let wrong = Email::parse("carol@example.com").unwrap();
    assert!(matches!(
        fixture
            .accept()
            .execute(
                fixture.tenant_id,
                AcceptInvitationCommand {
                    token: token.clone(),
                },
                bob,
                &wrong,
            )
            .await,
        Err(IdentityError::InvitationEmailMismatch)
    ));

    // Accept with the right email → membership granted.
    fixture
        .accept()
        .execute(
            fixture.tenant_id,
            AcceptInvitationCommand {
                token: token.clone(),
            },
            bob,
            &bob_email,
        )
        .await
        .unwrap();
    {
        {
            let memory = fixture.world.memory.lock().unwrap();
            let membership = memory
                .memberships
                .iter()
                .find(|m| m.user_id() == bob)
                .expect("bob is a member");
            assert_eq!(membership.role_ids(), &[fixture.roles.tenant_member]);
        }
    }

    // Replay: single-use token.
    assert!(matches!(
        fixture
            .accept()
            .execute(
                fixture.tenant_id,
                AcceptInvitationCommand { token },
                bob,
                &bob_email,
            )
            .await,
        Err(IdentityError::InvitationAlreadyAccepted)
    ));

    // Removing the last tenant_owner is refused; removing bob is not.
    assert!(matches!(
        fixture
            .remove_member()
            .execute(fixture.tenant_id, alice, bob)
            .await,
        Err(IdentityError::LastRoleHolder { .. })
    ));
    fixture
        .remove_member()
        .execute(fixture.tenant_id, bob, alice)
        .await
        .unwrap();

    // Self-removal is refused.
    fixture
        .grant_member()
        .execute(
            fixture.tenant_id,
            GrantMembershipCommand {
                email: "bob@example.com".to_string(),
                role_ids: vec![fixture.roles.tenant_member],
            },
            alice,
        )
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .remove_member()
            .execute(fixture.tenant_id, bob, bob)
            .await,
        Err(IdentityError::CannotRemoveSelf)
    ));
}

#[tokio::test]
async fn role_updates_emit_delta_events() {
    let fixture = Fixture::new(sanvi_testsupport::test_db().await);
    let alice = fixture.seed_user_async("alice@example.com", true).await;
    fixture
        .grant_member()
        .execute(
            fixture.tenant_id,
            GrantMembershipCommand {
                email: "alice@example.com".to_string(),
                role_ids: vec![fixture.roles.tenant_member],
            },
            UserId::try_from(999_999_i64).unwrap(),
        )
        .await
        .unwrap();
    fixture
        .update_roles()
        .execute(
            fixture.tenant_id,
            alice,
            UpdateRolesCommand {
                role_ids: vec![fixture.roles.tenant_owner],
            },
            alice,
        )
        .await
        .unwrap();
    {
        let memory = fixture.world.memory.lock().unwrap();
        assert!(memory.events.iter().any(|event| matches!(
            event,
            sanvi_identity::domain::IdentityEvent::MembershipRolesChanged { .. }
        )));
    }
}

#[tokio::test]
async fn logout_webhook_emits_revocation() {
    let fixture = Fixture::new(sanvi_testsupport::test_db().await);
    let alice = fixture.seed_user_async("alice@example.com", true).await;
    let kratos_id = fixture.world.memory.lock().unwrap().users[0].kratos_identity_id();
    fixture
        .logout()
        .execute(Some("session-42".to_string()), Some(kratos_id))
        .await
        .unwrap();
    {
        let memory = fixture.world.memory.lock().unwrap();
        assert!(memory.events.iter().any(|event| matches!(
            event,
            sanvi_identity::domain::IdentityEvent::SessionRevoked {
                user_id: Some(id),
                session_id,
                ..
            } if *id == alice && session_id == "session-42"
        )));
    }
}

#[tokio::test]
async fn delegation_policy_enforces_ownership_limits() {
    let fixture = Fixture::new(sanvi_testsupport::test_db().await);
    let alice = fixture.seed_user_async("alice@example.com", true).await;
    let bob = fixture.seed_user_async("bob@example.com", true).await;
    let _charlie = fixture.seed_user_async("charlie@example.com", true).await;

    // Alice is granted the initial tenant_owner (bootstrap allowed when 0 owners exist).
    fixture
        .grant_member()
        .execute(
            fixture.tenant_id,
            GrantMembershipCommand {
                email: "alice@example.com".to_string(),
                role_ids: vec![fixture.roles.tenant_owner],
            },
            alice,
        )
        .await
        .unwrap();

    // Alice (owner) grants Bob tenant_admin.
    fixture
        .grant_member()
        .execute(
            fixture.tenant_id,
            GrantMembershipCommand {
                email: "bob@example.com".to_string(),
                role_ids: vec![fixture.roles.tenant_admin],
            },
            alice,
        )
        .await
        .unwrap();

    // Bob (tenant_admin) attempts to grant Charlie tenant_owner -> Denied!
    let grant_err = fixture
        .grant_member()
        .execute(
            fixture.tenant_id,
            GrantMembershipCommand {
                email: "charlie@example.com".to_string(),
                role_ids: vec![fixture.roles.tenant_owner],
            },
            bob,
        )
        .await;
    assert!(matches!(
        grant_err,
        Err(IdentityError::DelegationDenied { .. })
    ));

    // Bob (tenant_admin) attempts to invite Charlie as tenant_owner -> Denied!
    let invite_err = fixture
        .invite()
        .execute(
            fixture.tenant_id,
            InviteMemberCommand {
                email: "charlie@example.com".to_string(),
                role_ids: vec![fixture.roles.tenant_owner],
            },
            bob,
        )
        .await;
    assert!(matches!(
        invite_err,
        Err(IdentityError::DelegationDenied { .. })
    ));

    // Bob (tenant_admin) attempts to elevate self to tenant_owner via update_roles -> Denied!
    let update_err = fixture
        .update_roles()
        .execute(
            fixture.tenant_id,
            bob,
            UpdateRolesCommand {
                role_ids: vec![fixture.roles.tenant_owner],
            },
            bob,
        )
        .await;
    assert!(matches!(
        update_err,
        Err(IdentityError::DelegationDenied { .. })
    ));

    // Alice (sole owner) attempts to replace her role with tenant_member -> LastRoleHolder!
    let last_owner_err = fixture
        .update_roles()
        .execute(
            fixture.tenant_id,
            alice,
            UpdateRolesCommand {
                role_ids: vec![fixture.roles.tenant_member],
            },
            alice,
        )
        .await;
    assert!(matches!(
        last_owner_err,
        Err(IdentityError::LastRoleHolder { .. })
    ));

    // Alice (owner) promotes Bob to tenant_owner -> Succeeded!
    fixture
        .update_roles()
        .execute(
            fixture.tenant_id,
            bob,
            UpdateRolesCommand {
                role_ids: vec![fixture.roles.tenant_owner],
            },
            alice,
        )
        .await
        .unwrap();

    // Now that there are 2 owners, Alice can replace her own role with tenant_member -> Succeeded!
    fixture
        .update_roles()
        .execute(
            fixture.tenant_id,
            alice,
            UpdateRolesCommand {
                role_ids: vec![fixture.roles.tenant_member],
            },
            alice,
        )
        .await
        .unwrap();
}
