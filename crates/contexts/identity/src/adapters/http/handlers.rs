//! Handlers: thin actix shims over the application layer.
//!
//! Handlers know nothing about Postgres, Redis or Kratos internals; they
//! resolve use cases from application state and translate domain errors
//! into [`ApiError`] at this boundary (the identity context's slice of the
//! single mapping rule).

use std::sync::Arc;

use actix_web::{HttpRequest, HttpResponse, Responder, web};
use sanvi_http::{
    ApiError, CurrentSubject, CurrentTenant, OptionalSubject, RequiredPermission, SessionToken,
};
use serde::Deserialize;

use crate::application::{
    AcceptInvitation, CompleteLinkChallenge, GetMe, GrantMembership, InviteMember, ListInvitations,
    ListMembers, ListSessions, RemoveMember, ResendInvitation, RevokeInvitation, RevokeSession,
    StartLinkChallenge, UpdateMemberRoles,
};
use crate::domain::IdentityError;
use crate::domain::InvitationId;
use crate::domain::user::UserId;

/// Application state visible to this context's handlers.
#[derive(Clone)]
pub struct IdentityState {
    pub get_me: Arc<GetMe>,
    pub list_sessions: Arc<ListSessions>,
    pub revoke_session: Arc<RevokeSession>,
    pub start_link_challenge: Arc<StartLinkChallenge>,
    pub complete_link_challenge: Arc<CompleteLinkChallenge>,
    pub list_members: Arc<ListMembers>,
    pub grant_member: Arc<GrantMembership>,
    pub update_member_roles: Arc<UpdateMemberRoles>,
    pub remove_member: Arc<RemoveMember>,
    pub list_invitations: Arc<ListInvitations>,
    pub invite_member: Arc<InviteMember>,
    pub accept_invitation: Arc<AcceptInvitation>,
    pub resend_invitation: Arc<ResendInvitation>,
    pub revoke_invitation: Arc<RevokeInvitation>,
    pub link_intent_ttl_secs: u64,
}

/// The identity context's domain-error → HTTP mapping (single place, this
/// boundary).
pub fn api_error(err: IdentityError) -> ApiError {
    match err {
        IdentityError::InvalidEmail(detail)
        | IdentityError::InvalidProvider(detail)
        | IdentityError::InvalidLocale(detail)
        | IdentityError::InvalidUserStatus(detail)
        | IdentityError::InvalidMembershipStatus(detail)
        | IdentityError::MalformedWebhook(detail) => ApiError::BadRequest { detail },
        IdentityError::NoRoles => ApiError::BadRequest {
            detail: "at least one role is required".to_string(),
        },
        IdentityError::UserNotFound { id } => ApiError::NotFound {
            resource: format!("user {id}"),
        },
        IdentityError::UserEmailNotFound { email } => ApiError::NotFound {
            resource: format!("user with email {email}"),
        },
        IdentityError::UserNotActive { .. } => ApiError::Forbidden {
            detail: "user account is deactivated".to_string(),
        },
        IdentityError::NotAMember { .. } => ApiError::NotFound {
            resource: "membership".to_string(),
        },
        IdentityError::CannotRemoveSelf => ApiError::Forbidden {
            detail: "a member cannot remove their own membership".to_string(),
        },
        IdentityError::LastRoleHolder { role, .. } => ApiError::Conflict {
            detail: format!("this change would remove the last holder of role {role}"),
        },
        IdentityError::AccountLinkedToOtherUser { provider, .. } => ApiError::Conflict {
            detail: format!("this {provider} account is already linked to another user"),
        },
        IdentityError::AccountAlreadyLinked { provider } => ApiError::Conflict {
            detail: format!("this {provider} account is already linked"),
        },
        IdentityError::InvitationNotFound { id } => ApiError::NotFound {
            resource: format!("invitation {id}"),
        },
        IdentityError::InvitationAlreadyAccepted => ApiError::Conflict {
            detail: "invitation already accepted".to_string(),
        },
        IdentityError::InvitationExpired => ApiError::Gone {
            detail: "invitation expired".to_string(),
        },
        IdentityError::InvitationEmailMismatch => ApiError::Forbidden {
            detail: "invitation is for a different email address".to_string(),
        },
        IdentityError::UnknownRole(id) => ApiError::BadRequest {
            detail: format!("unknown role id {id}"),
        },
        IdentityError::RoleNotAssignable(id) => ApiError::BadRequest {
            detail: format!("role {id} is not assignable in this tenant"),
        },
        IdentityError::LinkIntentUnavailable => ApiError::Gone {
            detail: "linking intent not found, expired or already used".to_string(),
        },
        IdentityError::LinkIntentEmailMismatch => ApiError::Forbidden {
            detail: "linking intent is for a different email address".to_string(),
        },
        IdentityError::SessionNotFound => ApiError::NotFound {
            resource: "session".to_string(),
        },
        IdentityError::KratosUnreachable(detail) => {
            tracing::error!(error = %detail, "kratos unreachable");
            ApiError::ServiceUnavailable {
                detail: "identity provider is temporarily unavailable".to_string(),
            }
        }
        IdentityError::KratosRejected(detail) => {
            tracing::error!(error = %detail, "kratos rejected request");
            ApiError::BadGateway {
                detail: "identity provider rejected the request".to_string(),
            }
        }
        IdentityError::IllegalUserTransition { .. } => ApiError::Conflict {
            detail: "illegal user state transition".to_string(),
        },
        IdentityError::Storage(detail) => {
            tracing::error!(error = %detail, "identity storage failure");
            ApiError::internal("storage failure")
        }
        IdentityError::IdGeneration(detail) => {
            tracing::error!(error = %detail, "identity ID generation failure");
            ApiError::ServiceUnavailable {
                detail: "ID generation is temporarily unavailable".to_string(),
            }
        }
    }
}

fn respond(
    result: Result<impl serde::Serialize, IdentityError>,
    status: actix_web::http::StatusCode,
) -> HttpResponse {
    match result {
        Ok(body) => HttpResponse::build(status).json(body),
        Err(err) => {
            let error = api_error(err);
            HttpResponse::build(error.status()).json(error.problem())
        }
    }
}

/// `GET /api/v1/me` — the signed-in user's profile.
#[utoipa::path(
    get,
    path = "/api/v1/me",
    tag = "identity",
    operation_id = "get_me",
    responses(
        (status = 200, description = "The current user", body = crate::application::MeView),
        (status = 401, description = "Not authenticated", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn get_me(
    state: web::Data<IdentityState>,
    subject: CurrentSubject,
    _guard: RequiredPermission,
) -> impl Responder {
    let user_id = subject.0.user_id;
    respond(
        state.get_me.execute(user_id).await,
        actix_web::http::StatusCode::OK,
    )
}

/// `GET /api/v1/me/sessions` — the current session (self-management scope
/// in phase 02).
#[utoipa::path(
    get,
    path = "/api/v1/me/sessions",
    tag = "identity",
    operation_id = "list_sessions",
    responses(
        (status = 200, description = "The current session", body = Vec<crate::application::SessionView>),
        (status = 401, description = "Not authenticated", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_sessions(
    state: web::Data<IdentityState>,
    token: SessionToken,
    _guard: RequiredPermission,
) -> impl Responder {
    respond(
        state.list_sessions.execute(&token.0).await,
        actix_web::http::StatusCode::OK,
    )
}

/// `DELETE /api/v1/me/sessions/{session_id}` — revoke the current session.
#[utoipa::path(
    delete,
    path = "/api/v1/me/sessions/{session_id}",
    tag = "identity",
    operation_id = "revoke_session",
    params(("session_id" = String, Path, description = "Session id")),
    responses(
        (status = 204, description = "Session revoked"),
        (status = 401, description = "Not authenticated", body = sanvi_http::ProblemDetail),
        (status = 404, description = "Session not found", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn revoke_session(
    state: web::Data<IdentityState>,
    path: web::Path<String>,
    subject: CurrentSubject,
    token: SessionToken,
    _guard: RequiredPermission,
) -> impl Responder {
    let user_id = subject.0.user_id;
    match state
        .revoke_session
        .execute(user_id, &path.into_inner(), &token.0)
        .await
    {
        Ok(()) => HttpResponse::NoContent().finish(),
        Err(err) => {
            let error = api_error(err);
            HttpResponse::build(error.status()).json(error.problem())
        }
    }
}

/// `POST /api/v1/auth/link/challenge` — start the account-linking challenge.
#[utoipa::path(
    post,
    path = "/api/v1/auth/link/challenge",
    tag = "identity",
    operation_id = "start_link_challenge",
    request_body = crate::application::StartLinkChallengeCommand,
    responses(
        (status = 201, description = "Challenge started", body = crate::application::StartLinkChallengeResponse),
        (status = 400, description = "Invalid input", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn start_link_challenge(
    state: web::Data<IdentityState>,
    body: web::Json<crate::application::StartLinkChallengeCommand>,
    _subject: OptionalSubject,
) -> impl Responder {
    respond(
        state.start_link_challenge.execute(body.into_inner()).await,
        actix_web::http::StatusCode::CREATED,
    )
}

/// `POST /api/v1/auth/link/complete` — complete the linking challenge
/// (authenticated: the browser holds the nonce, the session proves
/// ownership of the existing account).
#[utoipa::path(
    post,
    path = "/api/v1/auth/link/complete",
    tag = "identity",
    operation_id = "complete_link_challenge",
    request_body = CompleteLinkChallengeBody,
    responses(
        (status = 200, description = "Account linked", body = LinkCompleteResponse),
        (status = 401, description = "Not authenticated", body = sanvi_http::ProblemDetail),
        (status = 410, description = "Intent unavailable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn complete_link_challenge(
    state: web::Data<IdentityState>,
    subject: CurrentSubject,
    req: HttpRequest,
    body: web::Json<CompleteLinkChallengeBody>,
) -> impl Responder {
    // The nonce arrives either in the body or the `sanvi_link_intent` cookie.
    let nonce = body
        .nonce
        .clone()
        .or_else(|| {
            req.cookie("sanvi_link_intent")
                .map(|cookie| cookie.value().to_string())
        })
        .unwrap_or_default();

    let user = match state.get_me.execute(subject.0.user_id).await {
        Ok(user) => user,
        Err(err) => {
            let error = api_error(err);
            return HttpResponse::build(error.status()).json(error.problem());
        }
    };
    let email = match crate::domain::Email::parse(&user.email) {
        Ok(email) => email,
        Err(err) => {
            let error = api_error(err);
            return HttpResponse::build(error.status()).json(error.problem());
        }
    };

    match state
        .complete_link_challenge
        .execute(&nonce, subject.0.user_id, &email)
        .await
    {
        Ok(crate::application::LinkChallengeResult::Linked {
            provider,
            subject: provider_subject,
        }) => {
            let mut response = HttpResponse::Ok().json(LinkCompleteResponse {
                provider: provider.as_str().to_string(),
                subject: provider_subject.as_str().to_string(),
                linked: true,
            });
            // Consume the intent cookie either way.
            let _ = response.add_removal_cookie(
                &actix_web::cookie::Cookie::build("sanvi_link_intent", "")
                    .path("/")
                    .http_only(true)
                    .same_site(actix_web::cookie::SameSite::Lax)
                    .finish(),
            );
            response
        }
        Ok(crate::application::LinkChallengeResult::AlreadyLinked) => {
            HttpResponse::Ok().json(LinkCompleteResponse {
                provider: "".to_string(),
                subject: "".to_string(),
                linked: false,
            })
        }
        Err(err) => {
            let error = api_error(err);
            HttpResponse::build(error.status()).json(error.problem())
        }
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CompleteLinkChallengeBody {
    #[schema(example = "64-hex-chars")]
    pub nonce: Option<String>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct LinkCompleteResponse {
    pub provider: String,
    pub subject: String,
    pub linked: bool,
}

/// `GET /api/v1/tenant/members` — the tenant's members.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/members",
    tag = "identity",
    operation_id = "list_members",
    responses(
        (status = 200, description = "Members", body = Vec<crate::application::MemberView>),
        (status = 401, description = "Not authenticated", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_members(
    state: web::Data<IdentityState>,
    current: CurrentTenant,
    _guard: RequiredPermission,
) -> impl Responder {
    respond(
        state.list_members.execute(current.0.tenant_id).await,
        actix_web::http::StatusCode::OK,
    )
}

/// `POST /api/v1/tenant/members` — grant a membership.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/members",
    tag = "identity",
    operation_id = "grant_member",
    request_body = crate::application::GrantMembershipCommand,
    responses(
        (status = 201, description = "Membership granted", body = crate::application::MemberView),
        (status = 401, description = "Not authenticated", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 404, description = "User not found", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn grant_member(
    state: web::Data<IdentityState>,
    current: CurrentTenant,
    subject: CurrentSubject,
    body: web::Json<crate::application::GrantMembershipCommand>,
    _guard: RequiredPermission,
) -> impl Responder {
    let actor = subject.0.user_id;
    respond(
        state
            .grant_member
            .execute(current.0.tenant_id, body.into_inner(), actor)
            .await,
        actix_web::http::StatusCode::CREATED,
    )
}

/// `PATCH /api/v1/tenant/members/{user_id}/roles` — replace a member's roles.
#[utoipa::path(
    patch,
    path = "/api/v1/tenant/members/{user_id}/roles",
    tag = "identity",
    operation_id = "update_member_roles",
    params(("user_id" = String, Path, description = "User id")),
    request_body = crate::application::UpdateRolesCommand,
    responses(
        (status = 200, description = "Roles updated", body = ()),
        (status = 401, description = "Not authenticated", body = sanvi_http::ProblemDetail),
        (status = 404, description = "Membership not found", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn update_member_roles(
    state: web::Data<IdentityState>,
    current: CurrentTenant,
    path: web::Path<String>,
    body: web::Json<crate::application::UpdateRolesCommand>,
    _guard: RequiredPermission,
) -> impl Responder {
    let user_id = match path.parse::<UserId>() {
        Ok(id) => id,
        Err(_) => {
            return HttpResponse::BadRequest()
                .json(ApiError::bad_request("invalid user id").problem());
        }
    };
    respond(
        state
            .update_member_roles
            .execute(current.0.tenant_id, user_id, body.into_inner())
            .await,
        actix_web::http::StatusCode::OK,
    )
}

/// `DELETE /api/v1/tenant/members/{user_id}` — revoke a membership.
#[utoipa::path(
    delete,
    path = "/api/v1/tenant/members/{user_id}",
    tag = "identity",
    operation_id = "remove_member",
    params(("user_id" = String, Path, description = "User id")),
    responses(
        (status = 204, description = "Membership revoked"),
        (status = 403, description = "Permission denied or last-owner guard", body = sanvi_http::ProblemDetail),
        (status = 404, description = "Membership not found", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn remove_member(
    state: web::Data<IdentityState>,
    current: CurrentTenant,
    path: web::Path<String>,
    subject: CurrentSubject,
    _guard: RequiredPermission,
) -> impl Responder {
    let user_id = match path.parse::<UserId>() {
        Ok(id) => id,
        Err(_) => {
            return HttpResponse::BadRequest()
                .json(ApiError::bad_request("invalid user id").problem());
        }
    };
    let actor = subject.0.user_id;
    match state
        .remove_member
        .execute(current.0.tenant_id, user_id, actor)
        .await
    {
        Ok(()) => HttpResponse::NoContent().finish(),
        Err(err) => {
            let error = api_error(err);
            HttpResponse::build(error.status()).json(error.problem())
        }
    }
}

/// `GET /api/v1/tenant/invitations` — pending and accepted invitations.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/invitations",
    tag = "identity",
    operation_id = "list_invitations",
    responses(
        (status = 200, description = "Invitations", body = Vec<crate::application::InvitationView>),
        (status = 401, description = "Not authenticated", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_invitations(
    state: web::Data<IdentityState>,
    current: CurrentTenant,
    _guard: RequiredPermission,
) -> impl Responder {
    respond(
        state.list_invitations.execute(current.0.tenant_id).await,
        actix_web::http::StatusCode::OK,
    )
}

/// `POST /api/v1/tenant/invitations` — invite a member by email.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/invitations",
    tag = "identity",
    operation_id = "invite_member",
    request_body = crate::application::InviteMemberCommand,
    responses(
        (status = 201, description = "Invitation created", body = crate::application::InvitationView),
        (status = 400, description = "Invalid input", body = sanvi_http::ProblemDetail),
        (status = 401, description = "Not authenticated", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn invite_member(
    state: web::Data<IdentityState>,
    current: CurrentTenant,
    subject: CurrentSubject,
    body: web::Json<crate::application::InviteMemberCommand>,
    _guard: RequiredPermission,
) -> impl Responder {
    let actor = subject.0.user_id;
    match state
        .invite_member
        .execute(current.0.tenant_id, body.into_inner(), actor)
        .await
    {
        Ok((view, _token)) => HttpResponse::Created().json(view),
        Err(err) => {
            let error = api_error(err);
            HttpResponse::build(error.status()).json(error.problem())
        }
    }
}

/// `POST /api/v1/tenant/invitations/accept` — redeem an invitation token.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/invitations/accept",
    tag = "identity",
    operation_id = "accept_invitation",
    request_body = crate::application::AcceptInvitationCommand,
    responses(
        (status = 200, description = "Invitation accepted", body = ()),
        (status = 401, description = "Not authenticated", body = sanvi_http::ProblemDetail),
        (status = 404, description = "Invitation not found", body = sanvi_http::ProblemDetail),
        (status = 410, description = "Invitation expired", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn accept_invitation(
    state: web::Data<IdentityState>,
    current: CurrentTenant,
    subject: CurrentSubject,
    body: web::Json<crate::application::AcceptInvitationCommand>,
    _guard: RequiredPermission,
) -> impl Responder {
    let email = match state.get_me.execute(subject.0.user_id).await {
        Ok(user) => match crate::domain::Email::parse(&user.email) {
            Ok(email) => email,
            Err(err) => {
                let error = api_error(err);
                return HttpResponse::build(error.status()).json(error.problem());
            }
        },
        Err(err) => {
            let error = api_error(err);
            return HttpResponse::build(error.status()).json(error.problem());
        }
    };
    match state
        .accept_invitation
        .execute(
            current.0.tenant_id,
            body.into_inner(),
            subject.0.user_id,
            &email,
        )
        .await
    {
        Ok(()) => HttpResponse::Ok().json(serde_json::json!({ "accepted": true })),
        Err(err) => {
            let error = api_error(err);
            HttpResponse::build(error.status()).json(error.problem())
        }
    }
}

/// `POST /api/v1/tenant/invitations/{invitation_id}/resend` — reissue.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/invitations/{invitation_id}/resend",
    tag = "identity",
    operation_id = "resend_invitation",
    params(("invitation_id" = String, Path, description = "Invitation id")),
    responses(
        (status = 200, description = "Invitation reissued", body = crate::application::InvitationView),
        (status = 404, description = "Invitation not found", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn resend_invitation(
    state: web::Data<IdentityState>,
    current: CurrentTenant,
    path: web::Path<String>,
    _guard: RequiredPermission,
) -> impl Responder {
    let invitation_id = match path.parse::<InvitationId>() {
        Ok(id) => id,
        Err(_) => {
            return HttpResponse::BadRequest()
                .json(ApiError::bad_request("invalid invitation id").problem());
        }
    };
    respond(
        state
            .resend_invitation
            .execute(current.0.tenant_id, invitation_id)
            .await,
        actix_web::http::StatusCode::OK,
    )
}

/// `DELETE /api/v1/tenant/invitations/{invitation_id}` — revoke.
#[utoipa::path(
    delete,
    path = "/api/v1/tenant/invitations/{invitation_id}",
    tag = "identity",
    operation_id = "revoke_invitation",
    params(("invitation_id" = String, Path, description = "Invitation id")),
    responses(
        (status = 204, description = "Invitation revoked"),
        (status = 404, description = "Invitation not found", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn revoke_invitation(
    state: web::Data<IdentityState>,
    current: CurrentTenant,
    path: web::Path<String>,
    _guard: RequiredPermission,
) -> impl Responder {
    let invitation_id = match path.parse::<InvitationId>() {
        Ok(id) => id,
        Err(_) => {
            return HttpResponse::BadRequest()
                .json(ApiError::bad_request("invalid invitation id").problem());
        }
    };
    match state
        .revoke_invitation
        .execute(current.0.tenant_id, invitation_id)
        .await
    {
        Ok(()) => HttpResponse::NoContent().finish(),
        Err(err) => {
            let error = api_error(err);
            HttpResponse::build(error.status()).json(error.problem())
        }
    }
}
