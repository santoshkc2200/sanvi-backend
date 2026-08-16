//! HTTP driving adapter: handlers, routes and Kratos webhook receivers.

pub mod catalog;
mod handlers;
pub mod routes;
pub mod webhooks;

pub use handlers::{
    __path_accept_invitation, __path_complete_link_challenge, __path_get_me, __path_grant_member,
    __path_invite_member, __path_list_invitations, __path_list_members, __path_list_sessions,
    __path_remove_member, __path_resend_invitation, __path_revoke_invitation,
    __path_revoke_session, __path_start_link_challenge, __path_update_member_roles,
    CompleteLinkChallengeBody, IdentityState, LinkCompleteResponse, accept_invitation, api_error,
    complete_link_challenge, get_me, grant_member, invite_member, list_invitations, list_members,
    list_sessions, remove_member, resend_invitation, revoke_invitation, revoke_session,
    start_link_challenge, update_member_roles,
};
pub use routes::{identity_routes, internal_routes, tenant_scope_routes};
pub use webhooks::WebhookState;
