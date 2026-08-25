-- Finalizing a reconnect retains the original connection and removes the
-- temporary pending row after its encrypted token has moved.
GRANT DELETE ON advertising.connections TO sanvi_app;
