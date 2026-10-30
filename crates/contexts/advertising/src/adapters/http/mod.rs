pub mod handlers;
pub mod routes;
pub mod tracking_handlers;
use crate::adapters::remote::{
    Delegation, GatewayState, RequestBinding, TENANT_AUDIENCE, header, proxy,
};
use actix_web::{HttpRequest, HttpResponse, web};
use sanvi_http::{ApiError, CurrentSubject, CurrentTenant, RequiredPermission};
pub async fn proxy_tenant(
    state: web::Data<GatewayState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    permission: RequiredPermission,
    _feature: sanvi_http::RequiredAnyFeature,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    proxy(state, tenant, subject, permission, request, body).await
}
pub async fn proxy_platform(
    state: web::Data<GatewayState>,
    subject: CurrentSubject,
    permission: RequiredPermission,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let content_type = header(&request, "content-type");
    let mut claims = Delegation::new(
        TENANT_AUDIENCE,
        "platform",
        &RequestBinding {
            method: request.method().as_str(),
            target: &request.uri().to_string(),
            body: &body,
            content_type,
            if_match: "",
        },
    );
    claims.subject = Some(subject.0);
    claims.permission = Some(permission.key.into());
    state
        .remote
        .send(&claims, body.to_vec(), content_type, "")
        .await?
        .into_http()
}
