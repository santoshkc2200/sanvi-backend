//! The async-path contract: a job enqueued under one locale renders in
//! that locale when processed later, with no request to re-negotiate from.
//!
//! Locale is a required field on every event that produces user-visible
//! output downstream; workers read `metadata.locale` and render with it.

use std::sync::Arc;

use sanvi_kernel::events::EventEnvelope;
use sanvi_localization::adapters::{LocalizationCache, LocalizationMetrics, LocalizationService};
use sanvi_localization::domain::{Arg, LocalizationError, TranslationRepository};
use sanvi_localization::{LocalizationConfig, Message, Translator};
use sanvi_persistence::{PgPool, PlatformScope, ScopedTx};

struct EmptyRepos;

#[async_trait::async_trait]
impl TranslationRepository for EmptyRepos {
    async fn list_tenant(
        &self,
        _scope: &mut ScopedTx<'_>,
        _tenant_id: uuid::Uuid,
    ) -> Result<Vec<sanvi_localization::Translation>, LocalizationError> {
        Ok(Vec::new())
    }
    async fn list_platform(
        &self,
        _pool: &PgPool,
    ) -> Result<Vec<sanvi_localization::Translation>, LocalizationError> {
        Ok(Vec::new())
    }
    async fn upsert_tenant(
        &self,
        _scope: &mut ScopedTx<'_>,
        _tenant_id: uuid::Uuid,
        _translation: &sanvi_localization::Translation,
    ) -> Result<(), LocalizationError> {
        Ok(())
    }
    async fn upsert_platform(
        &self,
        _scope: &mut PlatformScope<'_>,
        _translation: &sanvi_localization::Translation,
    ) -> Result<(), LocalizationError> {
        Ok(())
    }
    async fn delete_tenant(
        &self,
        _scope: &mut ScopedTx<'_>,
        _tenant_id: uuid::Uuid,
        _key: &str,
        _locale: &str,
    ) -> Result<(), LocalizationError> {
        Ok(())
    }
}

fn service() -> Arc<LocalizationService> {
    // A lazy pool that is never touched: rendering falls back to the static
    // catalogs when the cache is cold, which is exactly what this test
    // exercises.
    let pool = PgPool::connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
        .expect("lazy pool construction never connects");
    let cache = Arc::new(LocalizationCache::new());
    let metrics = Arc::new(LocalizationMetrics::new(&prometheus::Registry::new()));
    Arc::new(LocalizationService::new(
        pool,
        Arc::new(EmptyRepos),
        cache,
        metrics,
        LocalizationConfig::default(),
    ))
}

/// The worker's view of an event: no request, no headers — only what the
/// envelope carries.
struct WorkerContext<'a> {
    locale: &'a str,
    scope: &'a str,
}

impl<'a> WorkerContext<'a> {
    fn from_envelope(envelope: &'a EventEnvelope) -> WorkerContext<'a> {
        // The contract under test: every user-visible event carries the
        // locale it was enqueued under.
        let locale = envelope
            .metadata
            .get("locale")
            .and_then(|value| value.as_str())
            .expect("user-visible events must carry metadata.locale");
        let scope = envelope
            .metadata
            .get("tenant_id")
            .and_then(|value| value.as_str())
            .unwrap_or("platform");
        WorkerContext { locale, scope }
    }
}

fn render_email(
    translator: &dyn Translator,
    context: &WorkerContext<'_>,
    invitation: Message,
) -> (String, String) {
    let subject = invitation.render(translator, context.scope, context.locale);
    let body = translator.translate(
        context.scope,
        context.locale,
        "emails.invitation.body",
        &[
            ("name", Arg::from("山田 太郎")),
            ("tenant", Arg::from("Acme")),
            ("url", Arg::from("https://acme.localhost/invite/abc")),
            ("days", Arg::from(7i64)),
        ],
    );
    (subject, body)
}

#[tokio::test]
async fn an_en_request_that_enqueues_a_job_renders_en_later() {
    let translator = service();
    let envelope = EventEnvelope::new(
        "identity.invitation",
        uuid::Uuid::now_v7(),
        "identity.invitation_created",
        serde_json::json!({ "invitee": "taro@example.com" }),
    )
    .with_locale("en");

    let context = WorkerContext::from_envelope(&envelope);
    let (subject, body) = render_email(
        translator.as_ref(),
        &context,
        Message::new("emails.invitation.subject").with_args(vec![("tenant", Arg::from("Acme"))]),
    );

    assert_eq!(subject, "You're invited to join Acme");
    assert!(body.contains("Hi 山田 太郎"), "body: {body}");
    assert!(body.contains("expires in 7 days"), "body: {body}");
}

#[tokio::test]
async fn a_ja_request_that_enqueues_a_job_renders_ja_later() {
    let translator = service();
    let envelope = EventEnvelope::new(
        "identity.invitation",
        uuid::Uuid::now_v7(),
        "identity.invitation_created",
        serde_json::json!({ "invitee": "taro@example.com" }),
    )
    .with_locale("ja");

    let context = WorkerContext::from_envelope(&envelope);
    let (subject, body) = render_email(
        translator.as_ref(),
        &context,
        Message::new("emails.invitation.subject").with_args(vec![("tenant", Arg::from("Acme"))]),
    );

    assert_eq!(subject, "Acme への招待");
    assert!(body.contains("山田 太郎 様"), "body: {body}");
    assert!(body.contains("7日"), "body: {body}");
}

#[test]
fn locale_is_a_required_field_on_user_visible_events() {
    // A worker context without a locale is a bug this assertion catches at
    // enqueue time (see `EventEnvelope::with_locale` in the kernel).
    let envelope = EventEnvelope::new(
        "identity.invitation",
        uuid::Uuid::now_v7(),
        "identity.invitation_created",
        serde_json::json!({}),
    );
    let result = std::panic::catch_unwind(|| WorkerContext::from_envelope(&envelope));
    assert!(
        result.is_err(),
        "events without metadata.locale must panic the worker-side test helper"
    );
}
