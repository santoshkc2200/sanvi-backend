-- Consumer failures remain durable even if Redis or its DLQ is lost.
CREATE TABLE platform.consumer_dead_letters (
    stream text NOT NULL,
    consumer text NOT NULL,
    entry_id text NOT NULL,
    event_id bigint,
    event_payload text NOT NULL,
    failure text NOT NULL,
    dead_lettered_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (stream, consumer, entry_id),
    UNIQUE (consumer, event_id)
);
GRANT SELECT, INSERT, UPDATE ON platform.consumer_dead_letters TO sanvi_app;
CREATE INDEX consumer_dead_letters_time ON platform.consumer_dead_letters (dead_lettered_at);
