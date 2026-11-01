-- The retry settings of a campaign (`ad_campaign::Retry`). The column holds
-- only the keys that differ from the defaults; the API merges a change into it.
ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS retry JSONB NOT NULL DEFAULT '{}';
