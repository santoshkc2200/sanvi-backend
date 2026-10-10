-- TASK-028 (FR-1130): the retention executors need DELETE on the
-- `archive`-action classes' tables — the archival sweep moves aged rows
-- out, and the app role (the worker connects as it) could not delete
-- them. Deleting rows past their registered statutory window is now a
-- designed operation, not a break-glass one.

GRANT DELETE ON billing.invoices TO sanvi_app;
GRANT DELETE ON payments.payments TO sanvi_app;
-- `transaction_record`'s history: refunds and disputes reference payments,
-- so the sweep archives (and deletes) them before their parent.
GRANT DELETE ON payments.refunds TO sanvi_app;
GRANT DELETE ON payments.disputes TO sanvi_app;
