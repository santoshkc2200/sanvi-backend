mod advertising_integration;
mod budget_integration;
mod tracking_integration;

/// The date the budget windows query: production computes period
/// boundaries in the tenant's calendar timezone (TASK-017), and the test
/// harness pins every tenant to `Asia/Tokyo` (`HarnessTimezones`). Metrics
/// rows must therefore be seeded at the JST date — using the UTC date
/// seeded rows outside the queried window for the 9 hours a day when JST
/// runs ahead of UTC, zeroing spend and silently flipping every
/// below-spend/sweep assertion (found by TASK-022's verification run,
/// 2026-10-07 23:48 UTC).
pub(crate) fn tenant_today() -> chrono::NaiveDate {
    chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(9 * 3600).expect("JST offset is constant"))
        .date_naive()
}
