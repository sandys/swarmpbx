-- Call attempts (PLAN.md §0.10.7a, GAPS.md BETTER-3, BETTER-4).
--
-- One row per call attempt. `snapshot` is the `ad_attempt::Attempt`
-- statechart, and `state` is its leg name. Each applied event is a row in
-- `attempt_events` with the next `seq`; the event row and the snapshot change
-- commit in one transaction.
--
-- A live attempt (state <> 'ended') holds one unit of campaign capacity. The
-- claim creates the attempt, so there is no separate hopper claim to release.
CREATE TABLE IF NOT EXISTS call_attempts (
    id                   BIGSERIAL PRIMARY KEY,
    campaign_id          BIGINT      NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    lead_id              BIGINT      NOT NULL REFERENCES leads(id) ON DELETE CASCADE,
    phone_number         TEXT        NOT NULL,
    room                 TEXT        NOT NULL,
    participant_identity TEXT        NOT NULL,
    worker               TEXT        NOT NULL,
    state                TEXT        NOT NULL,
    snapshot             JSONB       NOT NULL,
    seq                  INTEGER     NOT NULL DEFAULT 0,
    -- Set when the attempt ends (`ad_attempt::Outcome`).
    outcome              TEXT,
    sip_code             INTEGER,
    detail               TEXT,
    -- The circuit breaker counts this attempt as a failure
    -- (`ad_attempt::Outcome::is_failure`).
    failure              BOOLEAN     NOT NULL DEFAULT false,
    dialed_at            TIMESTAMPTZ,
    answered_at          TIMESTAMPTZ,
    ended_at             TIMESTAMPTZ,
    -- The last time a reconcile found the call in the room.
    checked_at           TIMESTAMPTZ,
    -- The lead was updated from this attempt.
    settled_at           TIMESTAMPTZ,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A lead has at most one live attempt, so it can never dial twice at once.
CREATE UNIQUE INDEX IF NOT EXISTS call_attempts_one_live_per_lead
    ON call_attempts (lead_id) WHERE state <> 'ended';
CREATE INDEX IF NOT EXISTS call_attempts_live_idx
    ON call_attempts (campaign_id) WHERE state <> 'ended';
CREATE INDEX IF NOT EXISTS call_attempts_room_live_idx
    ON call_attempts (room) WHERE state <> 'ended';
CREATE INDEX IF NOT EXISTS call_attempts_unsettled_idx
    ON call_attempts (campaign_id) WHERE state = 'ended' AND settled_at IS NULL;
-- The circuit-breaker window.
CREATE INDEX IF NOT EXISTS call_attempts_ended_idx
    ON call_attempts (campaign_id, ended_at DESC) WHERE ended_at IS NOT NULL;

CREATE TABLE IF NOT EXISTS attempt_events (
    attempt_id BIGINT      NOT NULL REFERENCES call_attempts(id) ON DELETE CASCADE,
    seq        INTEGER     NOT NULL,
    -- The event type (the serde tag of ad_attempt::AttemptEvent).
    kind       TEXT        NOT NULL,
    payload    JSONB       NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (attempt_id, seq)
);

-- The live-attempt count is bounded by this cap (GAPS.md DIAL-2).
ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS concurrency_cap INTEGER NOT NULL DEFAULT 10;
-- The breaker counts only attempts that ended after this time. A resume sets
-- it, so old failures do not pause the campaign again (GAPS.md DIAL-8).
ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS breaker_reset_at TIMESTAMPTZ;

-- The claim reads due leads directly (the eligibility index, GAPS.md BETTER-3).
CREATE INDEX IF NOT EXISTS leads_dialable_idx
    ON leads (campaign_id, called_count, id) WHERE status IN ('new', 'queued');

-- The attempt replaces the hopper claim and the outcome log.
DROP TABLE IF EXISTS hopper;
DROP TABLE IF EXISTS call_outcomes;
