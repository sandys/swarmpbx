-- Call outcomes: one row per terminal call attempt (PLAN.md §0.10.8a). The log
-- is the durable truth behind the Redis event layer. The circuit breaker can be
-- rebuilt from recent rows, and reports read from here.
--
-- Idempotency: UNIQUE (lead_id, attempt_at) means replaying a call_completed
-- event (Redis is at-least-once) inserts the same row once; the second insert
-- is a no-op via ON CONFLICT. This is what makes the write-back safe.
CREATE TABLE IF NOT EXISTS call_outcomes (
    id          BIGSERIAL PRIMARY KEY,
    campaign_id BIGINT      NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    lead_id     BIGINT      NOT NULL REFERENCES leads(id) ON DELETE CASCADE,
    -- The outcome discriminator in data (answered / no_answer / busy / failed /
    -- voicemail / error). No migration to add a disposition.
    outcome     TEXT        NOT NULL,
    answered    BOOLEAN     NOT NULL,
    attempt_at  TIMESTAMPTZ NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (lead_id, attempt_at)
);

CREATE INDEX IF NOT EXISTS call_outcomes_campaign_time_idx
    ON call_outcomes (campaign_id, attempt_at DESC);
