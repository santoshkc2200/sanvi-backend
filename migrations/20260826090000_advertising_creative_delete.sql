-- Creative deletion was added after the advertising core migration had already
-- shipped. Keep the original migration immutable so existing databases retain
-- their recorded SQLx checksum.
GRANT DELETE ON advertising.creatives TO sanvi_app;
