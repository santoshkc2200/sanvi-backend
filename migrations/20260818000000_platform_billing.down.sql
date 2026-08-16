-- Phase 04 down: drop the billing schema (webhook events, invoices,
-- subscriptions, prices, entitlement maps, plans).

DROP TABLE IF EXISTS billing.webhook_events;
DROP TABLE IF EXISTS billing.invoices;
DROP TABLE IF EXISTS billing.subscriptions;
DROP TABLE IF EXISTS billing.plan_prices;
DROP TABLE IF EXISTS billing.plan_entitlements;
DROP TABLE IF EXISTS billing.plans;
DROP SCHEMA IF EXISTS billing;
