-- TASK-017 review fix: durable FX snapshots for pinned cap bases.
--
-- A pinned basis date is validated by persisting the provider's *complete*
-- published day — every currency's units per EUR — before the cap is
-- accepted. Pair-shaped caches cannot do this: a currency that first
-- appears after the provider's retention window (ECB keeps 90 days; the
-- daily feed keeps one) would have no stored pair and enforcement would
-- stop. From one stored day, every pair that day can ever be asked for
-- derives as (quote per EUR) ÷ (base per EUR).
--
-- Rows are global market data — no tenant column, hence no tenant RLS —
-- and immutable: nothing updates or deletes a recorded day.

CREATE TABLE advertising.fx_rate_days (
  date       date        PRIMARY KEY,
  fetched_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE advertising.fx_rate_day_rates (
  date          date             NOT NULL REFERENCES advertising.fx_rate_days(date) ON DELETE CASCADE,
  currency      text             NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  units_per_eur double precision NOT NULL CHECK (
                   units_per_eur > 0
                   AND units_per_eur::text NOT IN ('NaN', 'Infinity', '-Infinity')
                 ),
  PRIMARY KEY (date, currency)
);

GRANT SELECT, INSERT ON advertising.fx_rate_days, advertising.fx_rate_day_rates TO sanvi_app;
