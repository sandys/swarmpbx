-- Hopper: leads selected and ready to dial, claimed by dispatch workers via
-- SELECT ... FOR UPDATE SKIP LOCKED (PLAN.md §0.8, §0.10.4). `claimed_by` /
-- `claimed_at` track the owning worker; a NULL claimed_by means available.
CREATE TABLE IF NOT EXISTS hopper (
    id          BIGSERIAL PRIMARY KEY,
    campaign_id BIGINT      NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    lead_id     BIGINT      NOT NULL REFERENCES leads(id) ON DELETE CASCADE,
    priority    INTEGER     NOT NULL DEFAULT 0,
    claimed_by  TEXT,
    claimed_at  TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (lead_id)
);

-- Index to fetch the next available items quickly, highest priority first.
CREATE INDEX IF NOT EXISTS hopper_available_idx
    ON hopper (campaign_id, priority DESC, id ASC)
    WHERE claimed_by IS NULL;
