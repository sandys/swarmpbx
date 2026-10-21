-- Agents and cluster nodes (PLAN.md §0.10.7a, §0.10.8b, decision D-2).
--
-- `campaigns.agent` is the agent of the campaign, as data: the `runtime` field
-- names the adapter (`livekit`, `pipecat`), so a new runtime needs no
-- migration. NULL means the campaign dials with no agent.
ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS agent JSONB;

-- The node that runs the attempt, and the generation of that assignment. The
-- scheduler raises `assign_gen` on each assignment. A node writes an event
-- only while the row names it with its generation, so a node that lost the
-- attempt to another node cannot dial it or change it.
ALTER TABLE call_attempts ADD COLUMN IF NOT EXISTS node TEXT;
ALTER TABLE call_attempts ADD COLUMN IF NOT EXISTS assign_gen INTEGER NOT NULL DEFAULT 0;
-- The identity of the agent in the room, once it joined.
ALTER TABLE call_attempts ADD COLUMN IF NOT EXISTS agent_identity TEXT;
-- The agent state name (`ad_attempt::AgentLeg::name`).
ALTER TABLE call_attempts ADD COLUMN IF NOT EXISTS agent_state TEXT NOT NULL DEFAULT 'none';

CREATE INDEX IF NOT EXISTS call_attempts_node_live_idx
    ON call_attempts (node) WHERE state <> 'ended';
