use actix_web::{App, HttpRequest, HttpResponse, HttpServer, web};
use sanvi_media::{MediaConfig, adapters::MediaPrivacyProvider};
use sanvi_privacy::domain::{EraseMode, PersonalDataProvider, PrivacyError, SubjectRef};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[actix_web::test]
async fn populated_media_export_and_erasure_wait_for_namespace_object_purge() {
    let db = sanvi_testsupport::test_db().await;
    let subject = SubjectRef::end_user(
        sanvi_kernel::TenantId::try_from(600_i64).unwrap(),
        "customer",
    );
    sqlx::query("INSERT INTO media.subject_namespaces(subject_key,tenant_id,owner_ref) VALUES($1,600,'customer')")
        .bind(subject.canonical_key()).execute(&db.admin_pool().await).await.unwrap();
    let phase = Arc::new(AtomicUsize::new(0));
    let state = phase.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = HttpServer::new(move || {
        App::new().app_data(web::Data::new(state.clone()))
            .route("/v1/assets", web::get().to(|request: HttpRequest, phase: web::Data<Arc<AtomicUsize>>| async move {
                assert_eq!(request.headers().get("X-Namespace-ID").unwrap(), "600");
                assert_eq!(request.headers().get("Authorization").unwrap(), "Bearer test-key");
                assert!(request.query_string().contains("owner_id=customer"));
                let status = match phase.load(Ordering::SeqCst) { 0 => "ready", 1 => "deleting", _ => return HttpResponse::Ok().json(serde_json::json!([])) };
                HttpResponse::Ok().json(serde_json::json!([{"asset":{"id":"1","status":status},"original_url":"https://objects.example.com/signed-original"}]))
            }))
            .route("/v1/assets/1", web::delete().to(|request: HttpRequest, phase: web::Data<Arc<AtomicUsize>>| async move {
                assert_eq!(request.headers().get("X-Namespace-ID").unwrap(), "600");
                phase.store(1, Ordering::SeqCst);
                HttpResponse::NoContent().finish()
            }))
    }).workers(1).listen(listener).unwrap().run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    let provider = MediaPrivacyProvider::with_storage(
        sanvi_persistence::TxManager::new(db.app_pool(), sanvi_testsupport::test_ids()),
        MediaConfig {
            base_url: format!("http://{address}"),
            service_api_key: "test-key".to_string(),
        },
    );
    let exported = provider.collect(&subject).await.unwrap();
    assert_eq!(exported.entries.len(), 1);
    assert!(
        exported.entries[0].value["original_url"]
            .as_str()
            .unwrap()
            .contains("signed-original")
    );
    let other = SubjectRef::end_user(
        sanvi_kernel::TenantId::try_from(601_i64).unwrap(),
        "customer",
    );
    assert!(provider.collect(&other).await.unwrap().entries.is_empty());
    assert!(matches!(
        provider.erase(&subject, EraseMode::Full).await,
        Err(PrivacyError::ProcessingPending(_))
    ));
    assert_eq!(phase.load(Ordering::SeqCst), 1);
    assert!(matches!(
        provider.erase(&subject, EraseMode::Full).await,
        Err(PrivacyError::ProcessingPending(_))
    ));
    phase.store(2, Ordering::SeqCst);
    assert!(
        provider
            .erase(&subject, EraseMode::Full)
            .await
            .unwrap()
            .retained
            .is_empty()
    );
    handle.stop(true).await;
}
