-- Join-paced dialing (GAPS.md DIAL-9, decision D-10).
--
-- One row for each agent pool: the settings that the API changes at run time
-- (`ad_pacing::Settings`), and the controller state (`ad_pacing::State`). A
-- NULL state starts again at the pool's first limit, in slow start.
CREATE TABLE IF NOT EXISTS agent_pools (
    pool       TEXT PRIMARY KEY,
    settings   JSONB NOT NULL DEFAULT '{}',
    state      JSONB,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The pacing of the campaign, for example {"mode": "strict"}.
ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS pacing JSONB NOT NULL DEFAULT '{}';

-- The pool of the attempt, and its window and limit at the claim.
ALTER TABLE call_attempts ADD COLUMN IF NOT EXISTS pool TEXT;
ALTER TABLE call_attempts ADD COLUMN IF NOT EXISTS pool_window INTEGER;
ALTER TABLE call_attempts ADD COLUMN IF NOT EXISTS pool_limit INTEGER;
-- Copies of the snapshot times, for the window count and the samples.
ALTER TABLE call_attempts ADD COLUMN IF NOT EXISTS agent_started_at TIMESTAMPTZ;
ALTER TABLE call_attempts ADD COLUMN IF NOT EXISTS agent_ready_at TIMESTAMPTZ;
-- When the controller learned from the attempt. Each attempt teaches once.
ALTER TABLE call_attempts ADD COLUMN IF NOT EXISTS paced_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS call_attempts_pool_window_idx
    ON call_attempts (pool) WHERE state <> 'ended' AND agent_ready_at IS NULL;
CREATE INDEX IF NOT EXISTS call_attempts_pool_unpaced_idx
    ON call_attempts (pool) WHERE pool IS NOT NULL AND paced_at IS NULL;
CREATE INDEX IF NOT EXISTS call_attempts_pool_created_idx
    ON call_attempts (pool, created_at) WHERE pool IS NOT NULL;
