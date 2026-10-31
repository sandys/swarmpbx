-- Agent outcomes, dispositions, callbacks, and the DNC list (GAPS.md AGNT-2,
-- LEAD-10, LEAD-5, COMP-7, decision D-6).

-- The disposition codes of the campaign (`ad_disposition::Disposition`). The
-- system codes are in the code, not in a table.
ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS dispositions JSONB NOT NULL DEFAULT '[]';

-- The claim takes leads of a higher priority first. A callback has 50.
ALTER TABLE leads ADD COLUMN IF NOT EXISTS priority SMALLINT NOT NULL DEFAULT 0;
DROP INDEX IF EXISTS leads_dialable_idx;
CREATE INDEX IF NOT EXISTS leads_dialable_idx
    ON leads (campaign_id, priority DESC, called_count, id) WHERE status IN ('new', 'queued');

-- The agent session of an attempt. The node keeps only the SHA-256 of the
-- session token; the agent shows the token. `agent_outcome` is the checked
-- report (`ad_disposition::AgentOutcome`); the first one stays.
ALTER TABLE call_attempts ADD COLUMN IF NOT EXISTS session_token_sha256 BYTEA;
ALTER TABLE call_attempts ADD COLUMN IF NOT EXISTS agent_outcome JSONB;
ALTER TABLE call_attempts ADD COLUMN IF NOT EXISTS disposition TEXT;
ALTER TABLE call_attempts ADD COLUMN IF NOT EXISTS outcome_at TIMESTAMPTZ;

-- The DNC list. A number is in E.164 (`ad_contact`). A system entry stops
-- every campaign; a campaign entry stops one campaign.
CREATE TABLE IF NOT EXISTS dnc_numbers (
    id           BIGSERIAL PRIMARY KEY,
    phone_number TEXT        NOT NULL,
    scope        TEXT        NOT NULL CHECK (scope IN ('system', 'campaign')),
    campaign_id  BIGINT      REFERENCES campaigns(id) ON DELETE CASCADE,
    -- 'agent' (a request in the call) or 'api'.
    source       TEXT        NOT NULL,
    attempt_id   BIGINT      REFERENCES call_attempts(id) ON DELETE SET NULL,
    evidence     JSONB       NOT NULL DEFAULT '{}',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((scope = 'system') = (campaign_id IS NULL))
);

CREATE UNIQUE INDEX IF NOT EXISTS dnc_numbers_key
    ON dnc_numbers (phone_number, scope, COALESCE(campaign_id, 0));
