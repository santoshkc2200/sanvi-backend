-- TASK-029 (FR-1134): the erasure propagation ledger.
--
-- The DSR saga erases the subject inside every registered provider, but an
-- erasure does not stop at the provider registry: it must also reach the
-- archives (TASK-028's archived ranges), the backup window, the caches,
-- the search indexes and the ad-platform suppression lists. Each
-- destination is either purged or documented as lawfully retained — with
-- an end date where the retention is time-boxed — and this table is where
-- that statement becomes evidence instead of a claim.
--
-- One row per (request, destination). `status` states the outcome:
--   purged      — the destination no longer holds the subject's data
--   retained    — lawfully retained; `retained` enumerates class, basis
--                 and end date for every retained item
--   failed      — the destination could not be reached (retry rewrites
--                 the row)
-- The compliance drills and the evidence packs read this table; a
-- destination with no row after completion is a gap the drills fail on.

CREATE TABLE privacy.erasure_propagations (
  request_id    bigint      NOT NULL
    REFERENCES privacy.data_subject_requests (id) ON DELETE CASCADE,
  subject_key   text        NOT NULL,
  destination   text        NOT NULL,
  purged        bigint      NOT NULL DEFAULT 0,
  retained      jsonb       NOT NULL DEFAULT '[]',
  status        text        NOT NULL
    CHECK (status IN ('purged', 'retained', 'failed')),
  detail        jsonb       NOT NULL DEFAULT '{}',
  propagated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (request_id, destination)
);

-- Global operational ledger — no tenant rows, hence no tenant RLS.
GRANT SELECT, INSERT, UPDATE ON privacy.erasure_propagations TO sanvi_app;
