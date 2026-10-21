#!/usr/bin/env bash
# Automated e2e: one agentdialer node carries two calls itself, one with each
# agent runtime, on the stack of docker-compose.cluster.yml (Kamailio, a NAT,
# a SIPp carrier, LiveKit, and the test agents; no host networking, no
# livekit/sip, no LiveKit webhook).
#
# Campaign 1 has a LiveKit agent (explicit dispatch by agent name). Campaign 2
# has a pipecat bot (a start request to the bot server). For each call the
# node joins the cluster in kine, leads, claims the lead as a call attempt,
# starts the agent in the attempt's room, waits until the agent is in the
# room, and only then sends the INVITE through Kamailio. On the answer the
# node joins the room and carries the audio. The agent's tone goes out as RTP
# through the NAT; the carrier sends it back (it latches), and the agent must
# hear it. The carrier hangs up after 8 s: its BYE comes back through
# Kamailio and ends the attempt on the node. The leader then settles the
# lead, closes the room, and completes the campaign.
#
# The checks treat Postgres and kine as managed services: they go through the
# database URL and the kine endpoint, never into a container.
#
# Exits non-zero on any failure. Always tears down.
set -uo pipefail
cd "$(dirname "$0")"
export E2E_NET=ad-cluster_cluster
export E2E_DATABASE_URL=postgres://ad:ad@postgres:5432/ad
export E2E_ETCD_ENDPOINT=kine:2379
. ./lib.sh

CL="docker compose -f docker-compose.cluster.yml"
HOLD_MS=8000
export E2E_CALLS=2
LK_STATS=http://livekit-agent:8088/stats
PC_STATS=http://pipecat-bot:8089/stats
FAIL=0

cleanup() {
  echo "== teardown =="
  agent_stats_json_of "$LK_STATS" > "$E2E_OUT/run_e2e.livekit-agent.stats.json" 2>/dev/null || true
  agent_stats_json_of "$PC_STATS" > "$E2E_OUT/run_e2e.pipecat-bot.stats.json" 2>/dev/null || true
  for s in node-1 kamailio carrier livekit livekit-agent pipecat-bot kine nat; do
    $CL logs "$s" > "$E2E_OUT/run_e2e.$s.log" 2>&1 || true
  done
  $CL down -v >/dev/null 2>&1 || true
  echo "== teardown complete (logs in $E2E_OUT/run_e2e.*.log) =="
}
trap cleanup EXIT

route_added() { [ "$($CL ps -a --format '{{.Service}} {{.State}} {{.ExitCode}}' | grep "^$1 " | cut -d' ' -f2-)" = "exited 0" ]; }

check_call() {
  local lead=$1 runtime=$2 stats=$3 pattern=$4 room agent events
  echo "== $runtime: lead $lead =="
  wait_for 60 "$runtime: attempt created" pg_ge "SELECT count(*) FROM call_attempts WHERE lead_id=$lead" 1 || { FAIL=1; return; }
  room=$(pg "SELECT room FROM call_attempts WHERE lead_id=$lead")
  wait_for 90 "$runtime: agent ready in $room" pg_ge \
    "SELECT count(*) FROM call_attempts WHERE lead_id=$lead AND agent_identity IS NOT NULL" 1 || FAIL=1
  agent=$(pg "SELECT agent_identity FROM call_attempts WHERE lead_id=$lead")
  [[ "$agent" == $pattern ]] || { echo "FAIL: $runtime agent identity '$agent' does not match $pattern"; FAIL=1; }
  echo "ok: $runtime agent identity $agent"
  wait_for 40 "$runtime: the call connected" pg_eq "SELECT answered_at IS NOT NULL FROM call_attempts WHERE lead_id=$lead" t || FAIL=1
  wait_for 25 "$runtime: the agent heard its tone back through the NAT" bash -c \
    ". ./lib.sh; n=\$(room_stat $stats $room loud_in); [ \"\${n:-0}\" -gt 0 ]" || FAIL=1
  wait_for 30 "$runtime: the carrier's BYE came back through Kamailio and ended the attempt" pg_eq \
    "SELECT count(*) FROM call_attempts a JOIN attempt_events e ON e.attempt_id = a.id
     WHERE a.lead_id=$lead AND a.outcome='completed' AND e.kind='hangup' AND e.payload->>'detail' = 'the callee hung up'" 1 || FAIL=1
  wait_for 15 "$runtime: lead dispositioned and counted once" pg_eq \
    "SELECT status || ':' || called_count FROM leads WHERE id=$lead" dispositioned:1 || FAIL=1
  wait_for 20 "$runtime: the leader closed the room and the agent left" bash -c \
    ". ./lib.sh; [ \"\$(room_stat $stats $room ended)\" = True ]" || FAIL=1
  events=$(pg "SELECT string_agg(e.kind, ',' ORDER BY e.seq) FROM attempt_events e JOIN call_attempts a ON a.id = e.attempt_id WHERE a.lead_id=$lead")
  [ "$events" = "agent_starting,agent_ready,dial_started,answered,hangup" ] || { echo "FAIL: $runtime attempt events were '$events'"; FAIL=1; }
  echo "ok: $runtime attempt events $events"
}

echo "== build images =="
docker build -q -t agentdialer:e2e .. >/dev/null || { echo "FAIL: image build"; exit 1; }
$CL build -q livekit-agent pipecat-bot >/dev/null 2>&1 || { echo "FAIL: agent image build"; exit 1; }
sed "s/HOLD_MS/$HOLD_MS/" carrier.xml > "$E2E_OUT/carrier_cluster.xml"
rm -f "$E2E_OUT/cluster_sip_messages.log" "$E2E_OUT/nat.ready"

echo "== start Postgres, kine, LiveKit, the agents, Kamailio, the NAT, the carrier =="
$CL up -d postgres kine livekit livekit-agent pipecat-bot kamailio kamailio-route nat carrier 2>&1 | tail -1
wait_for 60 "the NAT is ready" test -e "$E2E_OUT/nat.ready" || exit 1
wait_for 30 "Kamailio routes to the carrier through the NAT" route_added kamailio-route || exit 1
docker run --rm --network "$E2E_NET" -e DATABASE_URL="$E2E_DATABASE_URL" agentdialer:e2e migrate \
  >"$E2E_OUT/run_e2e.migrate.log" 2>&1 || { echo "FAIL: migrate"; exit 1; }
echo "ok: migrations applied"
wait_for 120 "LiveKit agent worker registered" http_ok http://livekit-agent:8081/ || exit 1
wait_for 120 "pipecat bot server up" http_ok "$PC_STATS" || exit 1

echo "== start one node and its route through the NAT =="
$CL up -d node-1 node-1-route 2>&1 | tail -1
wait_for 30 "node-1 routes to the carrier through the NAT" route_added node-1-route || exit 1
wait_for 60 "the node leads (leader key in kine)" kine_leader_is node-1 || FAIL=1

echo "== seed: a LiveKit-agent campaign and a pipecat-bot campaign, one lead each =="
# One transaction: the running leader never sees a campaign without its leads
# (it completes a running campaign that has no due work).
pg "INSERT INTO campaigns (id, org_id, name, state, concurrency_cap, agent, created_at, updated_at) VALUES
    (1, 1, 'e2e-livekit', 'running', 1, '{\"runtime\":\"livekit\",\"agent_name\":\"e2e-livekit\"}', now(), now()),
    (2, 1, 'e2e-pipecat', 'running', 1, '{\"runtime\":\"pipecat\",\"start_url\":\"http://pipecat-bot:8089/start\"}', now(), now());
    SELECT setval(pg_get_serial_sequence('campaigns','id'), 2, true);
    INSERT INTO leads (id, campaign_id, phone_number, timezone, status, called_count, context, created_at, updated_at) VALUES
    (1, 1, '+15550000001', 'America/New_York', 'new', 0, '{\"name\":\"Ada\"}'::jsonb, now(), now()),
    (2, 2, '+15550000002', 'America/New_York', 'new', 0, '{\"name\":\"Bob\"}'::jsonb, now(), now());
    SELECT setval(pg_get_serial_sequence('leads','id'), 2, true);" >/dev/null
pg_eq "SELECT count(*) FROM leads" 2 || { echo "FAIL: seed"; exit 1; }
echo "ok: seeded"

check_call 1 livekit "$LK_STATS" 'agent-AJ_*'
check_call 2 pipecat "$PC_STATS" 'agent-attempt-*'

wait_for 30 "campaigns completed" pg_eq "SELECT count(*) FROM campaigns WHERE state='completed'" 2 || FAIL=1
wait_for 20 "no call assignment left in kine" kine_none /agentdialer/calls/ || FAIL=1
NODES=$(kine_keys /agentdialer/nodes/)
[ "$NODES" = /agentdialer/nodes/node-1 ] || { echo "FAIL: kine nodes '$NODES'"; FAIL=1; }
echo "ok: kine has node $NODES"
INVITES=$(grep -c "^INVITE sip:" "$E2E_OUT/cluster_sip_messages.log" 2>/dev/null); INVITES=${INVITES:-0}
[ "$INVITES" = 2 ] || { echo "FAIL: the carrier received $INVITES INVITEs, want 2"; FAIL=1; }
echo "ok: the carrier received $INVITES INVITEs through Kamailio"

if [ "$FAIL" -eq 0 ]; then
  echo "E2E: PASS — the node carried two calls itself through Kamailio and a NAT: a LiveKit agent and a pipecat bot each joined their room before the dial, heard their tone back from the carrier, and the carrier's BYE ended each attempt; the leader settled both leads, closed the rooms, and completed both campaigns"
  exit 0
else
  echo "E2E: FAIL"
  exit 1
fi
