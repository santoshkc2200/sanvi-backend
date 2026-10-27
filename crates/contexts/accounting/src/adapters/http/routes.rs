use actix_web::{http::Method, web};
use sanvi_http::{PermissionMap, require_permission};

use super::handlers::{
    activate_accounting_book, cancel_accounting_switch, complete_accounting_authorization,
    create_accounting_book, get_accounting_book, list_accounting_books,
    register_accounting_connection, revoke_accounting_authorization, schedule_accounting_switch,
    start_accounting_authorization,
};

pub const ACCOUNTING_ENTITLEMENTS: &[&str] = &["accounting.freee"];

pub fn accounting_tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    let read = require_permission!("tenant.accounting.read", tenant);
    let manage = require_permission!("tenant.accounting.manage", tenant);
    let feature_gate = || sanvi_http::RequiredAnyFeature::declared(ACCOUNTING_ENTITLEMENTS);

    cfg.service(
        web::resource("/accounting/books")
            .app_data(PermissionMap::new(vec![
                (Method::GET, read),
                (Method::POST, manage),
            ]))
            .app_data(feature_gate())
            .route(web::get().to(list_accounting_books))
            .route(web::post().to(create_accounting_book)),
    );

    cfg.service(
        web::resource("/accounting/books/{book_id}")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .app_data(feature_gate())
            .route(web::get().to(get_accounting_book)),
    );

    cfg.service(
        web::resource("/accounting/books/{book_id}/connections")
            .app_data(PermissionMap::new(vec![(Method::POST, manage)]))
            .app_data(feature_gate())
            .route(web::post().to(register_accounting_connection)),
    );

    cfg.service(
        web::resource("/accounting/books/{book_id}/connections/{connection_id}/authorization")
            .app_data(PermissionMap::new(vec![
                (Method::POST, manage),
                (Method::DELETE, manage),
            ]))
            .app_data(feature_gate())
            .route(web::post().to(start_accounting_authorization))
            .route(web::delete().to(revoke_accounting_authorization)),
    );

    cfg.service(
        web::resource("/accounting/authorization/callback")
            .app_data(PermissionMap::new(vec![(Method::POST, manage)]))
            .app_data(feature_gate())
            .route(web::post().to(complete_accounting_authorization)),
    );

    cfg.service(
        web::resource("/accounting/books/{book_id}/activation")
            .app_data(PermissionMap::new(vec![(Method::POST, manage)]))
            .app_data(feature_gate())
            .route(web::post().to(activate_accounting_book)),
    );

    cfg.service(
        web::resource("/accounting/books/{book_id}/switch")
            .app_data(PermissionMap::new(vec![
                (Method::PUT, manage),
                (Method::DELETE, manage),
            ]))
            .app_data(feature_gate())
            .route(web::put().to(schedule_accounting_switch))
            .route(web::delete().to(cancel_accounting_switch)),
    );
}
