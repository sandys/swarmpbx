-- Campaigns. Mirrors the ad_campaign::Campaign aggregate (PLAN.md §0.10.2).
-- State is stored as text (a discriminator in data), so adding a state needs
-- no migration — only an enum variant in Rust.
CREATE TABLE IF NOT EXISTS campaigns (
    id             BIGSERIAL PRIMARY KEY,
    org_id         BIGINT      NOT NULL,
    name           TEXT        NOT NULL,
    state          TEXT        NOT NULL DEFAULT 'draft',
    total_rows     BIGINT      NOT NULL DEFAULT 0,
    processed_rows BIGINT      NOT NULL DEFAULT 0,
    failed_rows    BIGINT      NOT NULL DEFAULT 0,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS campaigns_org_id_idx ON campaigns (org_id);
CREATE INDEX IF NOT EXISTS campaigns_state_idx ON campaigns (state);
