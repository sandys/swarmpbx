-- Postgres replaces Redis for the orchestrator event layer (PLAN.md §0.10.8a,
-- decision D-3, 2026-10-20).
--
-- Each event is a row. The publisher inserts the row and, in the same
-- transaction, sends NOTIFY on channel 'campaign_events' with only the row id.
-- A listener reads rows by id after its last seen id, so a notification that is
-- lost (no listener connected, a reconnect) loses no event.
CREATE TABLE IF NOT EXISTS campaign_events (
    id          BIGSERIAL PRIMARY KEY,
    campaign_id BIGINT      NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    -- The event type (the serde tag of ad_events::CampaignEvent).
    kind        TEXT        NOT NULL,
    payload     JSONB       NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS campaign_events_campaign_idx
    ON campaign_events (campaign_id, id);

-- Retries move from Redis due-timers to the lead row. A queued lead with
-- next_eligible_at in the future waits; at or after that time the orchestrator
-- re-admits it to the hopper.
ALTER TABLE leads ADD COLUMN IF NOT EXISTS next_eligible_at TIMESTAMPTZ;
ALTER TABLE leads ADD COLUMN IF NOT EXISTS retry_reason TEXT;

CREATE INDEX IF NOT EXISTS leads_due_idx
    ON leads (campaign_id, next_eligible_at)
    WHERE next_eligible_at IS NOT NULL;
