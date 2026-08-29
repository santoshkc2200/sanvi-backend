use sanvi_advertising::adapters::http::handlers::{
    AccountView, AdvertisingProblemType, CampaignChangesView, CampaignView, CampaignsView,
    ConnectionHealthView, ConnectionView, ConnectionsView, CreateAdGroupRequest, CreateAdRequest,
    CreateCampaignRequest, CreateConnectionRequest, CreateCreativeRequest, CreativePreviewView,
    CreativePreviewsView, CreativeView, CreativesView, MetricPoint, MoneyView, OAuthCallbackQuery,
    PatchAdGroupRequest, PatchAdRequest, PatchCampaignRequest, PendingConnectionView,
    RenderedMoneyView, ResolveCampaignDriftRequest, ResolvedCampaignDriftView, StartOAuthRequest,
    StartOAuthResponse, ValidationResultView,
};
use sanvi_advertising::adapters::http::tracking_handlers::{
    CreateAudienceRequest, TestTrackingEventResponse, TrackConversionRequest,
};
use sanvi_advertising::domain::{
    Audience, AudienceStatus, ClickIds, ConsentSnapshot, ConversionDiagnostics, ConversionEvent,
    HashedIdentifier, Money as AdvertisingMoney, TrackingSettings, UploadReceipt, UploadState,
    ValueSource,
};
use sanvi_http::{ApiError, ProblemDetail};
use utoipa::OpenApi;

/// The merged OpenAPI document: every context contributes its paths and
/// schemas; the platform contributes the shared error model.
#[derive(OpenApi)]
#[openapi(
    paths(
        sanvi_system::adapters::http::healthz,
        sanvi_system::adapters::http::readyz,
        sanvi_system::adapters::http::version,
        sanvi_tenancy::adapters::http::provision_tenant,
        sanvi_tenancy::adapters::http::list_tenants,
        sanvi_tenancy::adapters::http::get_tenant,
        sanvi_tenancy::adapters::http::activate_tenant,
        sanvi_tenancy::adapters::http::suspend_tenant,
        sanvi_tenancy::adapters::http::resume_tenant,
        sanvi_tenancy::adapters::http::archive_tenant,
        sanvi_tenancy::adapters::http::get_context,
        sanvi_tenancy::adapters::http::get_settings,
        sanvi_tenancy::adapters::http::put_settings,
        sanvi_tenancy::adapters::http::slug_availability,
        sanvi_tenancy::adapters::http::public_tenant_context,
        sanvi_identity::adapters::http::get_me,
        sanvi_identity::adapters::http::list_sessions,
        sanvi_identity::adapters::http::revoke_session,
        sanvi_identity::adapters::http::start_link_challenge,
        sanvi_identity::adapters::http::complete_link_challenge,
        sanvi_identity::adapters::http::list_members,
        sanvi_identity::adapters::http::grant_member,
        sanvi_identity::adapters::http::update_member_roles,
        sanvi_identity::adapters::http::remove_member,
        sanvi_identity::adapters::http::list_invitations,
        sanvi_identity::adapters::http::invite_member,
        sanvi_identity::adapters::http::accept_invitation,
        sanvi_identity::adapters::http::resend_invitation,
        sanvi_identity::adapters::http::revoke_invitation,
        sanvi_access::adapters::http::list_permissions,
        sanvi_access::adapters::http::list_roles_platform,
        sanvi_access::adapters::http::list_roles_tenant,
        sanvi_access::adapters::http::create_role,
        sanvi_access::adapters::http::update_role,
        sanvi_access::adapters::http::delete_role,
        sanvi_access::adapters::http::list_features,
        sanvi_access::adapters::http::define_feature,
        sanvi_access::adapters::http::update_feature,
        sanvi_access::adapters::http::deprecate_feature,
        sanvi_access::adapters::http::list_tenant_entitlements,
        sanvi_audit::adapters::http::list_audit,
        sanvi_platform_admin::adapters::http::search_tenant_admin_views,
        sanvi_platform_admin::adapters::http::get_tenant_admin_view,
        sanvi_platform_admin::adapters::http::create_impersonation,
        sanvi_platform_admin::adapters::http::list_impersonations,
        sanvi_platform_admin::adapters::http::revoke_impersonation,
        sanvi_platform_admin::adapters::http::grant_entitlement_override,
        sanvi_platform_admin::adapters::http::list_entitlement_overrides,
        sanvi_platform_admin::adapters::http::revoke_entitlement_override,
        sanvi_platform_admin::adapters::http::get_application_fee,
        sanvi_platform_admin::adapters::http::set_application_fee,
        sanvi_platform_admin::adapters::http::disable_application_fee,
        sanvi_platform_admin::adapters::http::list_pending_approvals,
        sanvi_platform_admin::adapters::http::approve_request,
        sanvi_platform_admin::adapters::http::reject_request,
        sanvi_billing::adapters::http::handlers::list_public_plans,
        sanvi_billing::adapters::http::handlers::create_checkout_session,
        sanvi_billing::adapters::http::handlers::create_portal_session,
        sanvi_billing::adapters::http::handlers::get_subscription,
        sanvi_billing::adapters::http::handlers::list_invoices,
        sanvi_billing::adapters::http::handlers::list_plans,
        sanvi_billing::adapters::http::handlers::create_plan,
        sanvi_billing::adapters::http::handlers::update_plan,
        sanvi_billing::adapters::http::handlers::add_plan_price,
        sanvi_billing::adapters::http::handlers::set_plan_entitlements,
        sanvi_billing::adapters::http::handlers::apply_subscription_override,
        sanvi_billing::adapters::http::handlers::revoke_subscription_override,
        sanvi_privacy::adapters::http::handlers::submit_dsr,
        sanvi_privacy::adapters::http::handlers::verify_dsr,
        sanvi_privacy::adapters::http::handlers::get_dsr_status,
        sanvi_privacy::adapters::http::handlers::download_export,
        sanvi_privacy::adapters::http::handlers::record_opt_out,
        sanvi_privacy::adapters::http::handlers::limit_sensitive_use,
        sanvi_privacy::adapters::http::handlers::get_consents,
        sanvi_privacy::adapters::http::handlers::update_consents,
        sanvi_privacy::adapters::http::handlers::get_directives,
        sanvi_privacy::adapters::http::handlers::appeal_request,
        sanvi_privacy::adapters::http::handlers::rectify_request,
        sanvi_privacy::adapters::http::handlers::get_privacy_notice,
        sanvi_privacy::adapters::http::handlers::get_notice_at_collection,
        sanvi_privacy::adapters::http::handlers::get_public_metrics,
        sanvi_privacy::adapters::http::handlers::list_public_subprocessors,
        sanvi_privacy::adapters::http::handlers::submit_tenant_dsr,
        sanvi_privacy::adapters::http::handlers::list_tenant_dsrs,
        sanvi_privacy::adapters::http::handlers::list_dsrs,
        sanvi_privacy::adapters::http::handlers::reject_dsr,
        sanvi_privacy::adapters::http::handlers::extend_dsr,
        sanvi_privacy::adapters::http::handlers::list_appeals,
        sanvi_privacy::adapters::http::handlers::decide_appeal,
        sanvi_privacy::adapters::http::handlers::list_jurisdictions,
        sanvi_privacy::adapters::http::handlers::upsert_jurisdiction,
        sanvi_privacy::adapters::http::handlers::list_holds,
        sanvi_privacy::adapters::http::handlers::apply_hold,
        sanvi_privacy::adapters::http::handlers::release_hold,
        sanvi_privacy::adapters::http::handlers::list_incidents,
        sanvi_privacy::adapters::http::handlers::record_incident,
        sanvi_privacy::adapters::http::handlers::list_obligations,
        sanvi_privacy::adapters::http::handlers::mark_obligation_notified,
        sanvi_privacy::adapters::http::handlers::list_subprocessors,
        sanvi_privacy::adapters::http::handlers::upsert_subprocessor,
        sanvi_privacy::adapters::http::handlers::remove_subprocessor,
        sanvi_privacy::adapters::http::handlers::list_activities,
        sanvi_privacy::adapters::http::handlers::upsert_activity,
        sanvi_privacy::adapters::http::handlers::list_assessments,
        sanvi_privacy::adapters::http::handlers::upsert_assessment,
        sanvi_privacy::adapters::http::handlers::list_retention_rules,
        sanvi_privacy::adapters::http::handlers::upsert_retention_rule,
        sanvi_localization::adapters::http::handlers::get_public_locales,
        sanvi_localization::adapters::http::handlers::get_public_messages,
        sanvi_localization::adapters::http::handlers::get_localization_settings,
        sanvi_localization::adapters::http::handlers::put_localization_settings,
        sanvi_localization::adapters::http::handlers::get_localization_overrides,
        sanvi_localization::adapters::http::handlers::put_localization_overrides,
        sanvi_localization::adapters::http::handlers::get_platform_translations,
        sanvi_localization::adapters::http::handlers::put_platform_translations,
        sanvi_theming::adapters::http::handlers::get_public_theme,
        sanvi_theming::adapters::http::handlers::mint_theme_preview_token,
        sanvi_theming::adapters::http::handlers::preview_tenant_theme,
        sanvi_theming::adapters::http::handlers::list_available_themes,
        sanvi_theming::adapters::http::handlers::get_tenant_theme_draft,
        sanvi_theming::adapters::http::handlers::put_tenant_theme_draft,
        sanvi_theming::adapters::http::handlers::publish_tenant_theme,
        sanvi_theming::adapters::http::handlers::rollback_tenant_theme,
        sanvi_theming::adapters::http::handlers::upload_brand_asset,
        sanvi_theming::adapters::http::handlers::remove_brand_asset,
        sanvi_theming::adapters::http::handlers::list_platform_themes,
        sanvi_theming::adapters::http::handlers::install_theme_version,
        sanvi_theming::adapters::http::handlers::list_platform_theme_versions,
        sanvi_theming::adapters::http::handlers::yank_theme_version,
        sanvi_domains::adapters::http::handlers::claim_custom_domain,
        sanvi_domains::adapters::http::handlers::list_custom_domains,
        sanvi_domains::adapters::http::handlers::get_domain_instructions,
        sanvi_domains::adapters::http::handlers::request_domain_verification,
        sanvi_domains::adapters::http::handlers::promote_primary_domain,
        sanvi_domains::adapters::http::handlers::remove_custom_domain,
        sanvi_domains::adapters::http::handlers::search_domains,
        sanvi_domains::adapters::http::handlers::place_domain_order,
        sanvi_domains::adapters::http::handlers::list_domain_orders,
        sanvi_domains::adapters::http::handlers::set_order_auto_renew,
        sanvi_domains::adapters::http::handlers::tls_authorize,
        sanvi_domains::adapters::http::handlers::list_platform_domains,
        sanvi_domains::adapters::http::handlers::release_platform_domain,
        sanvi_payments::adapters::http::handlers::list_payment_providers,
        sanvi_payments::adapters::http::handlers::create_payment_connection,
        sanvi_payments::adapters::http::handlers::create_payment_connection_session,
        sanvi_payments::adapters::http::handlers::get_payment_connection,
        sanvi_payments::adapters::http::handlers::delete_payment_connection,
        sanvi_payments::adapters::http::handlers::create_tenant_checkout,
        sanvi_payments::adapters::http::handlers::get_tenant_checkout,
        sanvi_payments::adapters::http::handlers::list_payments,
        sanvi_payments::adapters::http::handlers::get_payment,
        sanvi_payments::adapters::http::handlers::refund_payment,
        sanvi_payments::adapters::http::handlers::list_disputes,
        sanvi_payments::adapters::http::handlers::export_payments,
        sanvi_payments::adapters::http::handlers::list_payouts,
        sanvi_payments::adapters::http::handlers::get_tax_settings,
        sanvi_payments::adapters::http::handlers::put_tax_settings,
        sanvi_advertising::adapters::http::handlers::list_ad_platforms,
        sanvi_advertising::adapters::http::handlers::resolve_campaign_drift,
        sanvi_advertising::adapters::http::handlers::create_campaign,
        sanvi_advertising::adapters::http::handlers::list_campaigns,
        sanvi_advertising::adapters::http::handlers::get_campaign,
        sanvi_advertising::adapters::http::handlers::patch_campaign,
        sanvi_advertising::adapters::http::handlers::validate_campaign,
        sanvi_advertising::adapters::http::handlers::publish_campaign,
        sanvi_advertising::adapters::http::handlers::pause_campaign,
        sanvi_advertising::adapters::http::handlers::resume_campaign,
        sanvi_advertising::adapters::http::handlers::list_campaign_changes,
        sanvi_advertising::adapters::http::handlers::add_ad_group,
        sanvi_advertising::adapters::http::handlers::update_ad_group,
        sanvi_advertising::adapters::http::handlers::add_ad,
        sanvi_advertising::adapters::http::handlers::update_ad,
        sanvi_advertising::adapters::http::handlers::start_oauth,
        sanvi_advertising::adapters::http::handlers::oauth_callback,
        sanvi_advertising::adapters::http::handlers::create_connection,
        sanvi_advertising::adapters::http::handlers::list_connections,
        sanvi_advertising::adapters::http::handlers::delete_connection,
        sanvi_advertising::adapters::http::handlers::create_creative,
        sanvi_advertising::adapters::http::handlers::get_creative,
        sanvi_advertising::adapters::http::handlers::list_creatives,
        sanvi_advertising::adapters::http::handlers::delete_creative,
        sanvi_advertising::adapters::http::handlers::get_creative_previews,
        sanvi_advertising::adapters::http::tracking_handlers::public_track_conversion,
        sanvi_advertising::adapters::http::tracking_handlers::get_tracking_settings,
        sanvi_advertising::adapters::http::tracking_handlers::put_tracking_settings,
        sanvi_advertising::adapters::http::tracking_handlers::test_tracking_event,
        sanvi_advertising::adapters::http::tracking_handlers::list_conversions,
        sanvi_advertising::adapters::http::tracking_handlers::get_conversion_diagnostics,
        sanvi_advertising::adapters::http::tracking_handlers::retry_conversion,
        sanvi_advertising::adapters::http::tracking_handlers::list_audiences,
        sanvi_advertising::adapters::http::tracking_handlers::create_audience,
        sanvi_advertising::adapters::http::tracking_handlers::refresh_audience,
    ),
    components(schemas(
        ProblemDetail, ApiError, AdvertisingProblemType, MetricPoint, MoneyView,
        RenderedMoneyView, ResolveCampaignDriftRequest, ResolvedCampaignDriftView,
        CampaignView, CampaignsView, CreateCampaignRequest, PatchCampaignRequest,
        ValidationResultView, CampaignChangesView, CreateAdGroupRequest, PatchAdGroupRequest,
        CreateAdRequest, PatchAdRequest, StartOAuthRequest, StartOAuthResponse,
        OAuthCallbackQuery, AccountView, PendingConnectionView, CreateConnectionRequest,
        ConnectionHealthView, ConnectionView, ConnectionsView,
        CreateCreativeRequest, CreativeView, CreativesView, CreativePreviewView, CreativePreviewsView,
        TrackConversionRequest, TestTrackingEventResponse, TrackingSettings, ConversionEvent,
        ConversionDiagnostics, ConsentSnapshot, ClickIds, ValueSource,
        UploadReceipt, UploadState, AdvertisingMoney, HashedIdentifier,
        CreateAudienceRequest, Audience, AudienceStatus
    )),
    tags(
        (name = "system", description = "System, health and build information"),
        (name = "tenancy", description = "Tenants, lifecycle, settings and context resolution"),
        (name = "identity", description = "Users, memberships, invitations and account linking"),
        (name = "access", description = "Permissions, roles, features and entitlements"),
        (name = "audit", description = "Append-only, hash-chained audit trail"),
        (name = "platform_admin", description = "Operator control plane: tenants, impersonation, approvals"),
        (name = "billing", description = "Platform billing: plans, subscriptions, invoices, checkout and portal"),
        (name = "privacy", description = "Privacy & compliance: jurisdiction profiles, consent/opt-out directives, data subject requests (export/erasure/appeals), retention, processing records, sub-processors and breach obligations"),
        (name = "localization", description = "Locales, message catalogs, tenant locale settings and copy overrides"),
        (name = "theming", description = "Theme registry, tenant theme drafts/publish/rollback, brand assets and resolved-theme delivery"),
        (name = "domains", description = "Custom domains: claims, verification, registrar search/purchase, TLS authorization and health"),
        (name = "payments", description = "Tenant payment providers (Stripe Connect, direct charges — Sanvi is never the merchant of record). Money is minor units + ISO-4217 (JPY is zero-decimal). Problem types: payments/provider-unavailable, payments/provider-auth, payments/provider-rate-limited, payments/invalid-country, payments/country-unsupported, payments/connection-exists, payments/connection-not-active, payments/capability-missing, payments/currency-unsupported, payments/idempotency-conflict. Every mutating endpoint requires an Idempotency-Key header; connection creation keys the provider-side idempotency chain, so retries adopt instead of duplicate. Account Session secrets are never persisted or logged."),
        (name = "advertising", description = "Tenant advertising. Money is integer minor units paired with ISO-4217 account currency; rendered FX values carry their rate date. Problem types: advertising/platform-unavailable, advertising/connection-not-active, advertising/capability-unsupported, advertising/token-expired, advertising/directive-suppressed, advertising/budget-cap-reached. Every mutating endpoint requires Idempotency-Key.")
    ),
    info(
        title = "Sanvi API",
        version = env!("CARGO_PKG_VERSION"),
        description = "Multi-tenant SaaS platform API (v1)",
    )
)]
pub struct ApiDoc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_contains_the_system_contract() {
        let json = ApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("/healthz"));
        assert!(json.contains("/readyz"));
        assert!(json.contains("/api/v1/system/version"));
        assert!(json.contains("ProblemDetail"));
    }

    #[test]
    fn doc_contains_the_tenancy_contract() {
        let json = ApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("/api/v1/platform/tenants"));
        assert!(json.contains("/api/v1/tenant/context"));
        assert!(json.contains("/api/v1/tenant/settings"));
        assert!(json.contains("/api/v1/public/slug-availability"));
        assert!(json.contains("/api/v1/public/tenant-context"));
        assert!(json.contains("TenantView"));
        assert!(json.contains("TenantContext"));
    }

    #[test]
    fn doc_contains_the_identity_and_access_contracts() {
        let json = ApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("/api/v1/me"));
        assert!(json.contains("/api/v1/tenant/members"));
        assert!(json.contains("/api/v1/tenant/invitations"));
        assert!(json.contains("/api/v1/auth/link/challenge"));
        assert!(json.contains("/api/v1/access/permissions"));
        assert!(json.contains("/api/v1/tenant/roles"));
        assert!(json.contains("/api/v1/platform/features"));
        assert!(json.contains("/api/v1/tenant/entitlements"));
        assert!(json.contains("MeView"));
        assert!(json.contains("MemberView"));
        assert!(json.contains("PermissionView"));
        assert!(json.contains("RoleView"));
        assert!(json.contains("FeatureView"));
    }

    #[test]
    fn doc_contains_the_platform_admin_and_audit_contracts() {
        let json = ApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("/api/v1/platform/admin/tenants"));
        assert!(json.contains("/api/v1/platform/impersonations"));
        assert!(json.contains("/api/v1/platform/audit"));
        assert!(json.contains("/api/v1/platform/approvals"));
        assert!(json.contains("TenantAdminView"));
        assert!(json.contains("ImpersonationGrantView"));
        assert!(json.contains("AuditPageView"));
        assert!(json.contains("ApprovalView"));
    }

    #[test]
    fn doc_contains_the_privacy_contract() {
        let json = ApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("/api/v1/privacy/requests"));
        assert!(json.contains("/api/v1/privacy/opt-out"));
        assert!(json.contains("/api/v1/privacy/directives"));
        assert!(json.contains("/api/v1/privacy/consents"));
        assert!(json.contains("/api/v1/public/privacy/notice"));
        assert!(json.contains("/api/v1/public/privacy/notice-at-collection"));
        assert!(json.contains("/api/v1/public/privacy/metrics"));
        assert!(json.contains("/api/v1/public/subprocessors"));
        assert!(json.contains("/api/v1/platform/privacy/jurisdictions"));
        assert!(json.contains("/api/v1/platform/privacy/holds"));
        assert!(json.contains("/api/v1/platform/privacy/incidents"));
        assert!(json.contains("/api/v1/tenant/privacy/requests"));
        assert!(json.contains("PurposeDirective"));
        assert!(json.contains("SubmitDsrCommand"));
        assert!(json.contains("JurisdictionProfile"));
    }

    #[test]
    fn doc_contains_the_billing_contract() {
        let json = ApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("/api/v1/public/plans"));
        assert!(json.contains("/api/v1/tenant/billing/checkout-session"));
        assert!(json.contains("/api/v1/tenant/billing/portal-session"));
        assert!(json.contains("/api/v1/tenant/billing/subscription"));
        assert!(json.contains("/api/v1/tenant/billing/invoices"));
        assert!(json.contains("/api/v1/platform/plans"));
        assert!(json.contains("/api/v1/platform/tenants/{id}/subscription/override"));
        assert!(json.contains("PublicPlanView"));
        assert!(json.contains("PlanAdminView"));
        assert!(json.contains("SubscriptionView"));
        assert!(json.contains("InvoiceView"));
    }

    #[test]
    fn doc_contains_the_localization_contract() {
        let json = ApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("/api/v1/public/locales"));
        assert!(json.contains("/api/v1/public/messages/{domain}"));
        assert!(json.contains("/api/v1/tenant/localization/settings"));
        assert!(json.contains("/api/v1/tenant/localization/overrides"));
        assert!(json.contains("/api/v1/platform/localization/translations"));
        assert!(json.contains("LocalizationSettingsView"));
        assert!(json.contains("CatalogView"));
        assert!(json.contains("TranslationView"));
    }

    #[test]
    fn doc_contains_the_theming_contract() {
        let json = ApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("/api/v1/public/theme"));
        assert!(json.contains("/api/v1/tenant/theme/draft"));
        assert!(json.contains("/api/v1/tenant/theme/preview"));
        assert!(json.contains("/api/v1/tenant/theme/publish"));
        assert!(json.contains("/api/v1/tenant/theme/rollback"));
        assert!(json.contains("/api/v1/tenant/themes/available"));
        assert!(json.contains("/api/v1/tenant/theme/assets"));
        assert!(json.contains("/api/v1/platform/themes"));
        assert!(json.contains("/api/v1/platform/themes/{key}/versions"));
        assert!(json.contains("/api/v1/platform/themes/{key}/versions/{version}/yank"));
        assert!(json.contains("ResolvedTheme"));
        assert!(json.contains("TenantThemeView"));
        assert!(json.contains("UpdateTenantThemeDraftCommand"));
        assert!(json.contains("ThemeAdminView"));
    }

    #[test]
    fn doc_contains_the_domains_contract() {
        let json = ApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("/api/v1/tenant/domains"));
        assert!(json.contains("/api/v1/tenant/domains/search"));
        assert!(json.contains("/api/v1/tenant/domains/orders"));
        assert!(json.contains("/api/v1/tenant/domains/{id}/instructions"));
        assert!(json.contains("/api/v1/tenant/domains/{id}/verify"));
        assert!(json.contains("/api/v1/tenant/domains/{id}/promote"));
        assert!(json.contains("/api/v1/tenant/domains/orders/{id}/auto-renew"));
        assert!(json.contains("/internal/tls/authorize"));
        assert!(json.contains("/api/v1/platform/domains"));
        assert!(json.contains("/api/v1/platform/domains/{id}/release"));
        assert!(json.contains("CustomDomainView"));
        assert!(json.contains("InstructionsView"));
        assert!(json.contains("OrderView"));
        assert!(json.contains("PlaceDomainOrderCommand"));
        assert!(json.contains("ClaimCustomDomainCommand"));
    }

    #[test]
    fn doc_contains_the_payments_contract() {
        let json = ApiDoc::openapi().to_json().unwrap();
        assert!(json.contains("/api/v1/tenant/payments/providers"));
        assert!(json.contains("ProvidersView"));
        assert!(json.contains("ProviderView"));
        // The catalog's 09.1 shape: kind-keyed entries with per-tenant
        // availability and country support — the frontend's card list.
        assert!(json.contains("supported_countries"));
        assert!(json.contains("requires_onboarding"));
        assert!(json.contains("available"));
        // The conventions this slice fixes for every later payments
        // endpoint: the error namespace, minor-unit money, idempotency.
        assert!(json.contains("payments/provider-unavailable"));
        assert!(json.contains("payments/connection-not-active"));
        assert!(json.contains("payments/capability-missing"));
        assert!(json.contains("payments/currency-unsupported"));
        assert!(json.contains("payments/idempotency-conflict"));
        assert!(json.contains("Idempotency-Key"));
    }

    #[test]
    fn doc_contains_the_advertising_contract() {
        let document: serde_json::Value =
            serde_json::from_str(&ApiDoc::openapi().to_json().unwrap()).unwrap();
        assert!(
            document["paths"]
                .get("/api/v1/tenant/ads/platforms")
                .is_some()
        );
        assert!(
            document["paths"]
                .get("/api/v1/tenant/ads/campaigns/{campaign_id}/drift")
                .is_some()
        );
        assert!(
            document["paths"]
                .get("/api/v1/tenant/ads/creatives")
                .is_some()
        );
        assert!(
            document["paths"]
                .get("/api/v1/tenant/ads/creatives/{creative_id}")
                .is_some()
        );
        assert!(
            document["paths"]
                .get("/api/v1/tenant/ads/creatives/{creative_id}/previews")
                .is_some()
        );
        assert!(
            document["paths"]
                .get("/api/v1/tenant/ads/conversions/{id}/diagnostics")
                .is_some()
        );
        assert!(
            document["paths"]
                .get("/api/v1/tenant/ads/conversions/{id}/retry")
                .is_some()
        );
        assert!(
            document["paths"]
                .get("/api/v1/tenant/ads/audiences")
                .is_some()
        );
        let preview_parameters = document["paths"]
            ["/api/v1/tenant/ads/creatives/{creative_id}/previews"]["get"]["parameters"]
            .as_array()
            .expect("creative previews declares parameters");
        let placement = preview_parameters
            .iter()
            .find(|parameter| parameter["name"] == "placement")
            .expect("creative previews declares placement");
        assert_eq!(placement["in"], "query");
        assert_ne!(placement["required"], true);
        let schemas = &document["components"]["schemas"];
        assert!(schemas.get("PlatformsView").is_some());
        assert!(schemas.get("CreativeView").is_some());
        assert!(schemas.get("CreativesView").is_some());
        assert!(schemas.get("CreateCreativeRequest").is_some());
        assert!(schemas.get("CreativePreviewsView").is_some());
        assert!(schemas.get("ConversionDiagnostics").is_some());
        for schema in [
            "AdvertisingProblemType",
            "MetricPoint",
            "MoneyView",
            "RenderedMoneyView",
            "ResolveCampaignDriftRequest",
            "ResolvedCampaignDriftView",
        ] {
            assert!(
                schemas.get(schema).is_some(),
                "missing advertising schema {schema}"
            );
        }
        for field in [
            "amount_minor",
            "currency",
            "fx_rate_date",
            "campaign_id",
            "rendered_spend",
        ] {
            assert!(
                schemas["MetricPoint"]["properties"].get(field).is_some()
                    || schemas["MoneyView"]["properties"].get(field).is_some()
                    || schemas["RenderedMoneyView"]["properties"]
                        .get(field)
                        .is_some(),
                "missing advertising field {field}"
            );
        }
        let problem_types = schemas["AdvertisingProblemType"]["enum"]
            .as_array()
            .expect("advertising problem types are an enum");
        for slug in [
            "advertising/platform-unavailable",
            "advertising/connection-not-active",
            "advertising/capability-unsupported",
            "advertising/token-expired",
            "advertising/directive-suppressed",
            "advertising/budget-cap-reached",
        ] {
            assert!(
                problem_types.iter().any(|value| value == slug),
                "missing {slug}"
            );
        }
    }
}
