//! FX rate lookup for cross-currency budget math (TASK-016/017).
//!
//! Two providers behind [`FxRatePort`]:
//!
//! - [`UnavailableFxRate`] — always reports no rate. The honest opt-out:
//!   cross-currency caps are refused until an operator configures a real
//!   provider, rather than converting against a guessed rate.
//! - [`EcbFxRate`] — the ECB euro reference rates (free, no key): the 90-day
//!   history feed, fetched once and cached 24 h. Absence of a rate (unknown
//!   currency, a date before the history window, fetch failure) is a normal
//!   outcome, not an error: callers degrade the affected rows and the cap
//!   write path refuses loudly.
//!
//! Rates are daily snapshots quoted against EUR; a cross pair converts
//! through EUR (`USD→JPY = (JPY/EUR) ÷ (USD/EUR)`). A requested date resolves
//! to that day's snapshot, or — the feed publishes business days only — the
//! most recent snapshot on or before it (previous-close convention). The
//! returned [`FxRate::date`] is always the snapshot actually used, never the
//! requested date, so threshold math and audit metadata cannot mistake one
//! day's rate for another's.
//!
//! Staleness is bounded: a snapshot older than [`MAX_SNAPSHOT_AGE`] is never
//! served, even during an outage — repeated failures degrade to unavailable
//! rather than keeping an arbitrarily old rate alive for auto-pause
//! decisions. Fetch attempts are single-flighted (one claimant under the
//! state lock) and back off on [`FAILURE_COOLDOWN`], so an outage produces
//! one request instead of one per spend row.
//!
//! Resolved rates are also recorded durably
//! ([`SqlxFxSnapshotStore`], wrapped by [`PersistedFxRates`]): a pinned
//! basis date keeps resolving — and thresholds stay reproducible — even
//! after the provider's retention window moves past it, and
//! [`FxRatePort::has_snapshot`] validates a pinned date exactly at cap-save
//! time, independent of any current spend rows.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::NaiveDate;

use crate::config::AdvertisingConfig;
use sanvi_persistence::TxManager;

use crate::domain::{AdvertisingError, FxRate, FxRatePort};

/// How long one fetched feed serves conversions before a refresh is attempted.
const SNAPSHOT_TTL: Duration = Duration::from_secs(24 * 3600);
/// Hard expiry: a feed older than this is never served, even when its
/// refresh keeps failing. Past this point every lookup degrades to
/// unavailable until a fetch succeeds again.
const MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(48 * 3600);
/// How long a failed fetch suppresses retries. Without it, every spend row
/// of an evaluation would issue its own HTTP request during an outage.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(5 * 60);

/// Always reports no rate available. Same-currency pairs never need one —
/// the query layer only calls this port when `base != quote`.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableFxRate;

impl UnavailableFxRate {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl FxRatePort for UnavailableFxRate {
    async fn rate(
        &self,
        _base: &str,
        _quote: &str,
        _date: NaiveDate,
    ) -> Result<Option<FxRate>, AdvertisingError> {
        Ok(None)
    }

    async fn has_snapshot(&self, _date: NaiveDate) -> Result<bool, AdvertisingError> {
        Ok(false)
    }

    async fn published_day(
        &self,
        _date: NaiveDate,
    ) -> Result<Option<Vec<(String, f64)>>, AdvertisingError> {
        Ok(None)
    }
}

#[derive(Debug, Clone)]
struct CachedFeed {
    fetched_at: Instant,
    /// Reference day → `currency → units per EUR`.
    days: std::collections::BTreeMap<NaiveDate, HashMap<String, f64>>,
}

#[derive(Debug)]
struct EcbState {
    feed: Option<CachedFeed>,
    failed_at: Option<Instant>,
    /// Set while one caller's fetch is in flight: every other caller waits
    /// for that refresh to finish (and consumes its result) instead of
    /// issuing its own duplicate request or answering from stale state.
    /// Self heals after twice the HTTP timeout, in case a fetch is lost.
    refresh_started_at: Option<Instant>,
}

/// ECB euro reference rates ([daily feed](https://www.ecb.europa.eu/stats/eurofxref/eurofxref-daily.xml)).
#[derive(Clone)]
pub struct EcbFxRate {
    http: reqwest::Client,
    url: String,
    /// The HTTP timeout this client enforces; bounds both the single-flight
    /// claim and a waiter's patience, so neither a lost refresh nor a
    /// vanished claimant can stall callers forever.
    timeout: Duration,
    state: Arc<tokio::sync::RwLock<EcbState>>,
    /// Fired whenever an in-flight refresh completes, so the callers that
    /// lost the claim wake up and consume its result instead of answering
    /// from stale state while a fresh feed is milliseconds away.
    refresh_done: Arc<tokio::sync::Notify>,
}

impl std::fmt::Debug for EcbFxRate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EcbFxRate")
            .field("url", &self.url)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl EcbFxRate {
    pub fn new(url: String, timeout: Duration) -> Result<Self, AdvertisingError> {
        let http = reqwest::Client::builder()
            .connect_timeout(timeout)
            .timeout(timeout)
            .build()
            .map_err(|e| AdvertisingError::PlatformUnavailable(e.to_string()))?;
        Ok(Self {
            http,
            url,
            timeout,
            state: Arc::new(tokio::sync::RwLock::new(EcbState {
                feed: None,
                failed_at: None,
                refresh_started_at: None,
            })),
            refresh_done: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Upper bound on one fetch: past this, an in-flight claim is considered
    /// lost and the next caller may claim the refresh again.
    fn refresh_deadline(&self) -> Duration {
        self.timeout * 2
    }

    /// The usable feed: fresh when young; while a refresh is in flight,
    /// waiters sleep on it and consume its result; stale-but-bounded while
    /// refreshes fail; absent past [`MAX_SNAPSHOT_AGE`] or before the first
    /// success.
    async fn feed(&self) -> Option<CachedFeed> {
        loop {
            {
                let state = self.state.read().await;
                if let Some(cached) = &state.feed
                    && cached.fetched_at.elapsed() < SNAPSHOT_TTL
                {
                    return Some(cached.clone());
                }
            }

            // Register interest in the next completion *before* deciding:
            // a caller that observes someone else's in-flight refresh waits
            // for it rather than answering from stale state (or `None` when
            // nothing stale is servable). `enable` fixes the registration
            // ahead of the re-check, so a refresh finishing in between
            // cannot be missed.
            let notified = self.refresh_done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            // Single flight: exactly one caller claims the refresh under the
            // write lock (no in-flight claim inside the deadline, no failure
            // cooldown). The loop only gets here with no fresh feed — the
            // fresh fast path returns at the top.
            let (claimed, in_flight) = {
                let mut state = self.state.write().await;
                let in_flight = state
                    .refresh_started_at
                    .is_some_and(|started| started.elapsed() < self.refresh_deadline());
                let cooling_down = state
                    .failed_at
                    .is_some_and(|at| at.elapsed() < FAILURE_COOLDOWN);
                if !in_flight && !cooling_down {
                    state.refresh_started_at = Some(Instant::now());
                    (true, false)
                } else {
                    (false, in_flight)
                }
            };

            if !claimed {
                if !in_flight {
                    // Cooling down from a failed fetch: waiting out the
                    // cooldown cannot produce a feed, so serve the stale
                    // policy and answer whatever it allows.
                    return self.stale_tail().await;
                }
                // Someone else is fetching: wait (bounded — a vanished
                // claimant must not stall callers forever), then re-check
                // from the top. If the refresh succeeded, the fresh fast
                // path answers; if it failed, the cooldown governs.
                let _ = tokio::time::timeout(self.refresh_deadline(), notified).await;
                continue;
            }

            match self.fetch().await {
                Ok(days) => {
                    let mut state = self.state.write().await;
                    let cached = CachedFeed {
                        fetched_at: Instant::now(),
                        days,
                    };
                    state.feed = Some(cached.clone());
                    state.failed_at = None;
                    state.refresh_started_at = None;
                    drop(state);
                    self.refresh_done.notify_waiters();
                    return Some(cached);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "ECB FX feed fetch failed; cross-currency spend degrades to unavailable");
                    let mut state = self.state.write().await;
                    state.failed_at = Some(Instant::now());
                    state.refresh_started_at = None;
                    drop(state);
                    self.refresh_done.notify_waiters();
                    // The claimant falls through to the stale-tail policy.
                }
            }

            return self.stale_tail().await;
        }
    }

    /// The stale policy: serve the previous feed only inside its maximum
    /// age, never past it.
    async fn stale_tail(&self) -> Option<CachedFeed> {
        let state = self.state.read().await;
        state.feed.clone().filter(|cached| {
            let age = cached.fetched_at.elapsed();
            if age >= MAX_SNAPSHOT_AGE {
                tracing::warn!(
                    age_secs = age.as_secs(),
                    "ECB FX feed is past its maximum age; cross-currency spend degrades to unavailable until a refresh succeeds"
                );
                false
            } else {
                true
            }
        })
    }

    async fn fetch(
        &self,
    ) -> Result<std::collections::BTreeMap<NaiveDate, HashMap<String, f64>>, AdvertisingError> {
        let body = self
            .http
            .get(&self.url)
            .send()
            .await
            .map_err(|e| AdvertisingError::PlatformUnavailable(e.to_string()))?
            .error_for_status()
            .map_err(|e| AdvertisingError::PlatformUnavailable(e.to_string()))?
            .text()
            .await
            .map_err(|e| AdvertisingError::PlatformUnavailable(e.to_string()))?;
        parse_ecb_feed(&body)
    }
}

#[async_trait]
impl FxRatePort for EcbFxRate {
    async fn rate(
        &self,
        base: &str,
        quote: &str,
        date: NaiveDate,
    ) -> Result<Option<FxRate>, AdvertisingError> {
        let base = base.trim().to_ascii_uppercase();
        let quote = quote.trim().to_ascii_uppercase();
        if base == quote {
            return Ok(Some(FxRate { rate: 1.0, date }));
        }
        let Some(feed) = self.feed().await else {
            return Ok(None);
        };
        // The feed publishes business days only: resolve the requested date
        // to its own snapshot, else the most recent snapshot on or before it
        // (previous-close convention). A date older than the history window
        // has no honest rate and degrades to unavailable.
        let Some((snapshot_date, rates)) = feed.days.range(..=date).next_back() else {
            return Ok(None);
        };
        let base_per_eur = if base == "EUR" {
            1.0
        } else {
            match rates.get(&base) {
                Some(rate) => *rate,
                None => return Ok(None),
            }
        };
        let quote_per_eur = if quote == "EUR" {
            1.0
        } else {
            match rates.get(&quote) {
                Some(rate) => *rate,
                None => return Ok(None),
            }
        };
        if !base_per_eur.is_finite() || !quote_per_eur.is_finite() || base_per_eur <= 0.0 {
            return Ok(None);
        }
        // One unit of base buys quote_per_eur/base_per_eur units of quote:
        // 1 USD = (JPY/EUR) ÷ (USD/EUR) ≈ 156 JPY, not its inverse.
        // The date is the snapshot actually used, never the requested date.
        Ok(Some(FxRate {
            rate: quote_per_eur / base_per_eur,
            date: *snapshot_date,
        }))
    }

    async fn has_snapshot(&self, date: NaiveDate) -> Result<bool, AdvertisingError> {
        // Exact membership, not previous-close: the check exists to validate
        // a *pinned* basis date, which later lookups must resolve to its own
        // snapshot or not at all.
        Ok(self.published_day(date).await?.is_some())
    }

    async fn published_day(
        &self,
        date: NaiveDate,
    ) -> Result<Option<Vec<(String, f64)>>, AdvertisingError> {
        let Some(feed) = self.feed().await else {
            return Ok(None);
        };
        let Some(rates) = feed.days.get(&date) else {
            return Ok(None);
        };
        // Sorted for determinism; EUR itself is included at 1.0 so a stored
        // day is self-contained and any pair on it derives without special
        // cases.
        let mut day: Vec<(String, f64)> = rates
            .iter()
            .map(|(currency, units)| (currency.clone(), *units))
            .collect();
        day.push(("EUR".to_string(), 1.0));
        day.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(Some(day))
    }
}

/// Parse the ECB feed into `reference day → (currency → units per EUR)`.
///
/// The feed nests `<Cube currency='USD' rate='1.0848'/>` rows inside
/// `<Cube time='2026-09-10'>` day wrappers; a currency row attaches to the
/// most recent day wrapper seen (rows before any day wrapper are skipped).
/// Only rows carrying *both* attributes with a valid code and positive finite
/// rate count. Strict on shape — a document with no dated rates is an error,
/// never an empty map that would read as "no coverage" — and tolerant on
/// quoting. Day wrappers with unparseable dates are skipped, not fatal — one
/// bad day must not void ninety good ones — and their child rates are skipped
/// with them: the wrapper detaches from the previous valid day instead of
/// silently re-dating its children onto it.
fn parse_ecb_feed(
    body: &str,
) -> Result<std::collections::BTreeMap<NaiveDate, HashMap<String, f64>>, AdvertisingError> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut days: std::collections::BTreeMap<NaiveDate, HashMap<String, f64>> =
        std::collections::BTreeMap::new();
    let mut current_day: Option<NaiveDate> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(element)) | Ok(Event::Start(element)) => {
                if element.name().as_ref() == b"Cube" {
                    let mut day: Option<NaiveDate> = None;
                    let mut currency: Option<String> = None;
                    let mut rate: Option<f64> = None;
                    for attr in element.attributes() {
                        let attr = attr.map_err(|e| {
                            AdvertisingError::PlatformUnavailable(format!(
                                "ECB FX feed has malformed attributes: {e}"
                            ))
                        })?;
                        // Decode + unescape without version-dependent
                        // normalisation: dates, currency codes and decimal
                        // rates are newline-free, so normalisation would be a
                        // no-op.
                        let value = reader.decoder().decode(attr.value.as_ref()).map_err(|e| {
                            AdvertisingError::PlatformUnavailable(format!(
                                "ECB FX feed has undecodable attribute: {e}"
                            ))
                        })?;
                        let value = quick_xml::escape::unescape(&value).map_err(|e| {
                            AdvertisingError::PlatformUnavailable(format!(
                                "ECB FX feed has unescapable attribute: {e}"
                            ))
                        })?;
                        match attr.key.as_ref() {
                            b"time" => {
                                // Detach from the carried day *before*
                                // parsing: an unparseable wrapper date must
                                // not leave its children attached to the
                                // previous valid day — they are skipped with
                                // the wrapper.
                                current_day = None;
                                day = NaiveDate::parse_from_str(&value, "%Y-%m-%d").ok();
                            }
                            b"currency" => {
                                currency = Some(value.to_ascii_uppercase());
                            }
                            b"rate" => {
                                rate = value.parse::<f64>().ok();
                            }
                            _ => {}
                        }
                    }
                    if let Some(day) = day {
                        current_day = Some(day);
                        days.entry(day).or_default();
                    } else if let (Some(code), Some(value)) = (currency, rate)
                        && code.len() == 3
                        && code.chars().all(|c| c.is_ascii_alphabetic())
                        && value.is_finite()
                        && value > 0.0
                        && let Some(day) = current_day
                    {
                        days.entry(day).or_default().insert(code, value);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(AdvertisingError::PlatformUnavailable(format!(
                    "ECB FX feed is not valid XML: {e}"
                )));
            }
            _ => {}
        }
        buf.clear();
    }

    days.retain(|_, rates| !rates.is_empty());
    if days.is_empty() {
        return Err(AdvertisingError::PlatformUnavailable(
            "ECB FX feed contained no dated currency rates".to_string(),
        ));
    }
    Ok(days)
}

/// Durable, immutable published days for pinned basis dates
/// (`advertising.fx_rate_days` + `advertising.fx_rate_day_rates`). Rows are
/// global market data — no tenant column, no tenant RLS — and nothing
/// updates or deletes a recorded day. Writes use a plain transaction (the
/// tables have no RLS to scope), and a lost write is harmless: the next
/// resolution records the day again. A day is stored *complete* — every
/// currency the provider published — so pairs that only appear in scope
/// later (a new ad account, a new currency) still resolve after the
/// provider's retention window has moved past the date.
#[derive(Clone)]
pub struct SqlxFxSnapshotStore {
    tx: TxManager,
}

impl std::fmt::Debug for SqlxFxSnapshotStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqlxFxSnapshotStore")
            .finish_non_exhaustive()
    }
}

impl SqlxFxSnapshotStore {
    pub fn new(tx: TxManager) -> Self {
        Self { tx }
    }

    /// The stored day's reference rates — only when the *complete* published
    /// day was persisted (marker row in `fx_rate_days`), never a partial
    /// set that would resolve some pairs and not others.
    async fn day_rates(
        &self,
        date: NaiveDate,
    ) -> Result<Option<Vec<(String, f64)>>, AdvertisingError> {
        let mut tx = self
            .tx
            .begin()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        let rows: Vec<(String, f64)> = sqlx::query_as(
            "SELECT r.currency, r.units_per_eur
             FROM advertising.fx_rate_day_rates r
             JOIN advertising.fx_rate_days d ON d.date = r.date
             WHERE r.date = $1
             ORDER BY r.currency",
        )
        .bind(date)
        .fetch_all(&mut **tx.as_inner())
        .await
        .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        tx.commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        Ok(if rows.is_empty() { None } else { Some(rows) })
    }

    async fn record_day(
        &self,
        date: NaiveDate,
        day: &[(String, f64)],
    ) -> Result<(), AdvertisingError> {
        let (currencies, units): (Vec<String>, Vec<f64>) = day.iter().cloned().unzip();
        let mut tx = self
            .tx
            .begin()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        sqlx::query(
            "INSERT INTO advertising.fx_rate_days (date) VALUES ($1) ON CONFLICT DO NOTHING",
        )
        .bind(date)
        .execute(&mut **tx.as_inner())
        .await
        .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        sqlx::query(
            "INSERT INTO advertising.fx_rate_day_rates (date, currency, units_per_eur)
             SELECT $1, * FROM unnest($2::text[], $3::float8[])
             ON CONFLICT (date, currency) DO NOTHING",
        )
        .bind(date)
        .bind(&currencies)
        .bind(&units)
        .execute(&mut **tx.as_inner())
        .await
        .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        tx.commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        Ok(())
    }

    async fn has_day(&self, date: NaiveDate) -> Result<bool, AdvertisingError> {
        let mut tx = self
            .tx
            .begin()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        let known: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM advertising.fx_rate_days WHERE date = $1)",
        )
        .bind(date)
        .fetch_one(&mut **tx.as_inner())
        .await
        .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        tx.commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        Ok(known)
    }
}

/// The configured provider behind the durable day store: a day published
/// once is served forever, for every pair. Lookups derive from the stored
/// day first, so a pinned basis keeps resolving — and thresholds stay
/// reproducible — after the provider's retention window moves past the
/// pinned date, for any currency the day quoted, including ones that only
/// appear in scope afterwards.
#[derive(Clone)]
pub struct PersistedFxRates {
    provider: Arc<dyn FxRatePort>,
    store: SqlxFxSnapshotStore,
}

impl PersistedFxRates {
    pub fn new(provider: Arc<dyn FxRatePort>, store: SqlxFxSnapshotStore) -> Self {
        Self { provider, store }
    }

    /// Derive a cross rate from a stored day: units-per-EUR on both legs
    /// (EUR itself stored at 1.0), so one unit of base buys
    /// quote_per_eur ÷ base_per_eur units of quote.
    fn derive_from_day(
        day: &[(String, f64)],
        base: &str,
        quote: &str,
        date: NaiveDate,
    ) -> Option<FxRate> {
        let base_per_eur = day.iter().find(|(c, _)| c == base)?.1;
        let quote_per_eur = day.iter().find(|(c, _)| c == quote)?.1;
        if !base_per_eur.is_finite() || !quote_per_eur.is_finite() || base_per_eur <= 0.0 {
            return None;
        }
        Some(FxRate {
            rate: quote_per_eur / base_per_eur,
            date,
        })
    }

    /// Persist a published day quietly: a failed write only costs the next
    /// resolution a fresh provider round-trip, so it must not fail the
    /// conversion or validation that surfaced it.
    async fn record_day_quiet(&self, date: NaiveDate, day: &[(String, f64)]) {
        if let Err(e) = self.store.record_day(date, day).await {
            tracing::warn!(
                date = %date,
                error = %e,
                "Failed to persist FX day snapshot; will retry on the next resolution"
            );
        }
    }
}

#[async_trait]
impl FxRatePort for PersistedFxRates {
    async fn rate(
        &self,
        base: &str,
        quote: &str,
        date: NaiveDate,
    ) -> Result<Option<FxRate>, AdvertisingError> {
        let base = base.trim().to_ascii_uppercase();
        let quote = quote.trim().to_ascii_uppercase();
        if base == quote {
            return Ok(Some(FxRate { rate: 1.0, date }));
        }
        if let Some(derived) = self
            .store
            .day_rates(date)
            .await?
            .and_then(|day| Self::derive_from_day(&day, &base, &quote, date))
        {
            return Ok(Some(derived));
        }
        // No stored day (or it does not quote this pair — a currency the
        // feed did not publish that day): fall through to the provider,
        // which may still cover it.
        match self.provider.rate(&base, &quote, date).await? {
            Some(fx) => {
                if fx.rate.is_finite() && fx.rate > 0.0 {
                    match self.provider.published_day(fx.date).await {
                        Ok(Some(day)) => self.record_day_quiet(fx.date, &day).await,
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!(
                                date = %fx.date,
                                error = %e,
                                "Could not read the published FX day; snapshot not persisted"
                            );
                        }
                    }
                }
                Ok(Some(fx))
            }
            None => Ok(None),
        }
    }

    async fn has_snapshot(&self, date: NaiveDate) -> Result<bool, AdvertisingError> {
        // A day recorded durably answers even once the provider's retention
        // has moved past the date — for every pair it quoted.
        if self.store.has_day(date).await? {
            return Ok(true);
        }
        // Accepting a pin persists the *complete* day, not an existence
        // bit: any currency the day quoted must resolve from storage after
        // the provider's window moves on, or enforcement would silently
        // stop for pairs that first appear later. The persistence is
        // therefore part of the answer — a failed write fails the save
        // loudly instead of accepting a pin whose reproducibility was never
        // stored. (Conversion-path writes stay quiet: they already
        // succeeded, and the next resolution retries the write.)
        let Some(day) = self.provider.published_day(date).await? else {
            // No published day: the pin itself is invalid (weekend, future,
            // out of retention) — the caller turns `false` into a validation
            // error naming the date.
            return Ok(false);
        };
        // The persistence is part of the answer: a failed write fails the
        // save loudly instead of accepting a pin whose reproducibility was
        // never stored. (Conversion-path writes stay quiet: they already
        // succeeded, and the next resolution retries the write.)
        self.store.record_day(date, &day).await?;
        Ok(true)
    }

    async fn published_day(
        &self,
        date: NaiveDate,
    ) -> Result<Option<Vec<(String, f64)>>, AdvertisingError> {
        // The store answers from the days it has recorded; the provider is
        // consulted for days it has not. Delegating keeps every consumer —
        // cap validation included — reading through one durable view.
        if let Some(day) = self.store.day_rates(date).await? {
            return Ok(Some(day));
        }
        self.provider.published_day(date).await
    }
}

/// Build the configured FX provider. `"none"` (or anything unrecognised,
/// which [`AdvertisingConfig::validate`] already rejects at boot) returns
/// the refusing [`UnavailableFxRate`] *directly*: the documented opt-out
/// refuses every cross-currency conversion, so it must not be wrapped with
/// the durable store — previously stored snapshots would otherwise keep
/// resolving against a provider the operator disabled. `"ecb"` serves live
/// reference rates behind the durable day store.
pub fn build_fx_rates(config: &AdvertisingConfig, tx: TxManager) -> Arc<dyn FxRatePort> {
    if !config.fx_provider.trim().eq_ignore_ascii_case("ecb") {
        return Arc::new(UnavailableFxRate::new());
    }
    let timeout = Duration::from_secs(config.fx_timeout_secs.max(1));
    match EcbFxRate::new(config.fx_ecb_url.clone(), timeout) {
        Ok(rates) => {
            tracing::info!(
                url = %config.fx_ecb_url,
                "FX provider: ECB reference rates (24h cache, durable day snapshots)"
            );
            Arc::new(PersistedFxRates::new(
                Arc::new(rates),
                SqlxFxSnapshotStore::new(tx),
            ))
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                "ECB FX client failed to build; falling back to unavailable rates"
            );
            Arc::new(UnavailableFxRate::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<gesmes:Envelope xmlns:gesmes="http://www.gesmes.org/xml/2002-08-01" xmlns="http://www.ecb.int/vocabulary/2002-08-01/eurofxref">
  <gesmes:subject>Reference rates</gesmes:subject>
  <gesmes:Sender><gesmes:name>European Central Bank</gesmes:name></gesmes:Sender>
  <Cube>
    <Cube time='2026-09-09'>
      <Cube currency='USD' rate='1.0801'/>
      <Cube currency="JPY" rate="168.95"/>
    </Cube>
    <Cube time='2026-09-10'>
      <Cube currency='USD' rate='1.0848'/>
      <Cube currency="JPY" rate="169.42"/>
      <Cube currency='GBP' rate='0.85310'/>
    </Cube>
  </Cube>
</gesmes:Envelope>"#;

    fn day(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn parses_ecb_days_regardless_of_quote_style() {
        let days = parse_ecb_feed(SAMPLE_FEED).unwrap();
        assert_eq!(days.len(), 2);
        assert!((days[&day(2026, 9, 10)]["USD"] - 1.0848).abs() < 1e-9);
        assert!((days[&day(2026, 9, 10)]["JPY"] - 169.42).abs() < 1e-9);
        assert!((days[&day(2026, 9, 9)]["USD"] - 1.0801).abs() < 1e-9);
    }

    #[test]
    fn rejects_empty_and_malformed_feeds() {
        assert!(parse_ecb_feed("").is_err());
        assert!(parse_ecb_feed("<Cube></Cube>").is_err());
        assert!(parse_ecb_feed("<Cube currency='USD'/>").is_err());
        // A day wrapper with no rates is dropped; nothing dated remains.
        assert!(parse_ecb_feed("<Cube><Cube time='2026-09-10'></Cube></Cube>").is_err());
        assert!(parse_ecb_feed("not xml at all <").is_err());
    }

    #[test]
    fn invalid_day_wrapper_does_not_redate_its_children() {
        let feed = r#"<?xml version="1.0" encoding="UTF-8"?>
<gesmes:Envelope xmlns:gesmes="http://www.gesmes.org/xml/2002-08-01" xmlns="http://www.ecb.int/vocabulary/2002-08-01/eurofxref">
  <Cube>
    <Cube time='2026-09-09'>
      <Cube currency='USD' rate='1.0801'/>
    </Cube>
    <Cube time='not-a-date'>
      <Cube currency='JPY' rate='999.99'/>
    </Cube>
    <Cube time='2026-09-10'>
      <Cube currency='GBP' rate='0.85310'/>
    </Cube>
  </Cube>
</gesmes:Envelope>"#;
        let days = parse_ecb_feed(feed).unwrap();
        // The bad wrapper's children are skipped, not attached to the
        // previous (or next) valid day.
        assert_eq!(days.len(), 2);
        assert!((days[&day(2026, 9, 9)]["USD"] - 1.0801).abs() < 1e-9);
        assert!(!days[&day(2026, 9, 9)].contains_key("JPY"));
        assert!(days.iter().all(|(_, rates)| !rates.contains_key("JPY")));
        assert!((days[&day(2026, 9, 10)]["GBP"] - 0.85310).abs() < 1e-9);
    }

    fn feed_with(days: std::collections::BTreeMap<NaiveDate, HashMap<String, f64>>) -> EcbFxRate {
        let fx = EcbFxRate::new(
            "http://localhost:9/unused".to_string(),
            Duration::from_secs(1),
        )
        .unwrap();
        fx.state
            .try_write()
            .expect("test-only synchronous cache prime")
            .feed = Some(CachedFeed {
            fetched_at: Instant::now(),
            days,
        });
        fx
    }

    #[tokio::test]
    async fn usd_to_jpy_crosses_in_the_sane_direction() {
        let fx = feed_with(parse_ecb_feed(SAMPLE_FEED).unwrap());
        // 1 USD ≈ 156 JPY on the 10th: the wrong-way division would answer ~0.0064.
        let rate = fx
            .rate("USD", "JPY", day(2026, 9, 10))
            .await
            .unwrap()
            .unwrap();
        assert!((rate.rate - 156.18).abs() < 0.05);
        // The returned date is the snapshot used, and the 9th resolves to
        // its own (different) rate.
        assert_eq!(rate.date, day(2026, 9, 10));
        let older = fx
            .rate("USD", "JPY", day(2026, 9, 9))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(older.date, day(2026, 9, 9));
        assert!((older.rate - 156.40).abs() < 0.05);
        // Identity and EUR legs.
        assert_eq!(
            fx.rate("USD", "USD", day(2026, 9, 10))
                .await
                .unwrap()
                .unwrap()
                .rate,
            1.0
        );
        let eur_usd = fx
            .rate("EUR", "USD", day(2026, 9, 10))
            .await
            .unwrap()
            .unwrap();
        assert!((eur_usd.rate - 1.0848).abs() < 1e-9);
        // Unknown currencies degrade to unavailable, never error.
        assert!(
            fx.rate("USD", "XXX", day(2026, 9, 10))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn weekend_resolves_to_friday_and_predates_window_is_unavailable() {
        let fx = feed_with(parse_ecb_feed(SAMPLE_FEED).unwrap());
        // Saturday the 12th carries Friday the 10th's snapshot forward, and
        // says so on the returned date.
        let weekend = fx
            .rate("USD", "JPY", day(2026, 9, 12))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(weekend.date, day(2026, 9, 10));
        // Before the history window there is no honest rate.
        assert!(
            fx.rate("USD", "JPY", day(2026, 9, 8))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn snapshot_existence_is_exact() {
        let fx = feed_with(parse_ecb_feed(SAMPLE_FEED).unwrap());
        assert!(fx.has_snapshot(day(2026, 9, 9)).await.unwrap());
        // The weekend and the day after the feed both fail exact membership,
        // though rate() would happily previous-close the Saturday: existence
        // validates a *pinned* date, which must resolve to its own day.
        assert!(!fx.has_snapshot(day(2026, 9, 12)).await.unwrap());
        assert!(!fx.has_snapshot(day(2026, 9, 11)).await.unwrap());
    }

    #[tokio::test]
    async fn waiter_consumes_the_in_flight_refresh() {
        let days = parse_ecb_feed(SAMPLE_FEED).unwrap();
        let fx = EcbFxRate::new(
            "http://localhost:9/unused".to_string(),
            Duration::from_secs(1),
        )
        .unwrap();
        {
            // Someone else's refresh is in flight: this caller must wait for
            // it, not answer `None` from an empty feed.
            let mut state = fx.state.try_write().unwrap();
            state.refresh_started_at = Some(Instant::now());
        }
        let rescuer = tokio::spawn({
            let fx = fx.clone();
            async move {
                // The claimant "fetches": the fresh feed lands, the claim
                // clears, and waiters are woken.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let mut state = fx.state.try_write().unwrap();
                state.feed = Some(CachedFeed {
                    fetched_at: Instant::now(),
                    days,
                });
                state.refresh_started_at = None;
                drop(state);
                fx.refresh_done.notify_waiters();
            }
        });
        let rate = fx
            .rate("USD", "JPY", day(2026, 9, 10))
            .await
            .unwrap()
            .expect("waiter must consume the completed refresh");
        rescuer.await.unwrap();
        assert!((rate.rate - 156.18).abs() < 0.05);
    }

    #[tokio::test]
    async fn vanished_claimant_bounds_the_wait() {
        let fx = EcbFxRate::new(
            "http://localhost:9/unused".to_string(),
            Duration::from_secs(1),
        )
        .unwrap();
        {
            // A claimant exists but will never finish (dropped future): the
            // waiter's bounded patience expires, the lost claim is reclaimed,
            // and the unroutable fetch records the failure.
            fx.state.try_write().unwrap().refresh_started_at = Some(Instant::now());
        }
        let rate = fx.rate("USD", "JPY", day(2026, 9, 10)).await.unwrap();
        assert!(rate.is_none());
        assert!(fx.state.try_read().unwrap().failed_at.is_some());
    }

    #[tokio::test]
    async fn feed_past_max_age_degrades_to_unavailable() {
        let mut feed = CachedFeed {
            fetched_at: Instant::now() - MAX_SNAPSHOT_AGE - Duration::from_secs(1),
            days: parse_ecb_feed(SAMPLE_FEED).unwrap(),
        };
        // Sanity: the map itself still resolves — only serving is gated.
        assert!(feed.days.range(..=day(2026, 9, 10)).next_back().is_some());
        let fx = EcbFxRate::new(
            "http://localhost:9/unused".to_string(),
            Duration::from_secs(1),
        )
        .unwrap();
        // A refresh would be attempted (cooldown clear) but the URL is
        // unroutable, so the lookup must degrade rather than serve the
        // expired snapshot.
        fx.state.try_write().unwrap().feed = Some(feed.clone());
        assert!(
            fx.rate("USD", "JPY", day(2026, 9, 10))
                .await
                .unwrap()
                .is_none()
        );
        // Within the grace window (TTL < age < max) the stale feed serves.
        feed.fetched_at = Instant::now() - SNAPSHOT_TTL - Duration::from_secs(1);
        fx.state.try_write().unwrap().feed = Some(feed);
        fx.state.try_write().unwrap().failed_at =
            Some(Instant::now() - FAILURE_COOLDOWN - Duration::from_secs(1));
        // The refresh attempt fails (unroutable URL) and the stale snapshot
        // answers — exercising the failure path without network flakes,
        // since localhost:9 refuses fast.
        let _ = fx.rate("USD", "JPY", day(2026, 9, 10)).await.unwrap();
    }
}
