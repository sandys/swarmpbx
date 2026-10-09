-- Leads. Mirrors the ad_lead::Lead aggregate (PLAN.md §0.10.3). Compliance-
-- bearing columns (timezone, state, postal_code, called_count,
-- last_local_call_time) feed ad_compliance and the hopper. `context` holds the
-- CSV extra columns as JSONB. `status` is a text discriminator (no migration to
-- add a status).
CREATE TABLE IF NOT EXISTS leads (
    id                   BIGSERIAL PRIMARY KEY,
    campaign_id          BIGINT      NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    phone_number         TEXT        NOT NULL,
    timezone             TEXT,
    state                TEXT,
    postal_code          TEXT,
    status               TEXT        NOT NULL DEFAULT 'new',
    called_count         INTEGER     NOT NULL DEFAULT 0,
    last_local_call_time TIMESTAMPTZ,
    context              JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS leads_campaign_id_idx ON leads (campaign_id);
CREATE INDEX IF NOT EXISTS leads_status_idx ON leads (status);
CREATE INDEX IF NOT EXISTS leads_phone_number_idx ON leads (phone_number);
