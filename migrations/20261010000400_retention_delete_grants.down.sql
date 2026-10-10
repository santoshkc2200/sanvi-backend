-- Down: retract the retention delete grants.
REVOKE DELETE ON billing.invoices FROM sanvi_app;
REVOKE DELETE ON payments.payments FROM sanvi_app;
REVOKE DELETE ON payments.refunds FROM sanvi_app;
REVOKE DELETE ON payments.disputes FROM sanvi_app;
