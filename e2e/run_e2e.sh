#!/usr/bin/env bash
# Automated e2e: one agentdialer node carries calls itself on the stack of
# docker-compose.cluster.yml: Kamailio, a NAT, the carrier simulator
# (e2e/carrier-sim, which behaves like Twilio Elastic SIP Trunking), LiveKit,
# and the test agents. No host networking, no livekit/sip, no LiveKit webhook.
#
# Campaigns 1 and 2 have an agent: a LiveKit agent (explicit dispatch) and a
# pipecat bot (a start request). For each call the node leads in kine, claims
# the lead as a call attempt, starts the agent in the attempt's room, waits
# until the agent is in the room, and only then sends the INVITE through
# Kamailio. The carrier challenges it like Twilio (407 digest); the node
# answers with the trunk credentials and the caller ID. On the answer the
# node joins the room and carries the audio: the agent's tone goes out as RTP
# through the NAT, the carrier sends it back (symmetric RTP), and the agent
# hears it. The carrier hangs up after 8 s; its BYE comes back through
# Kamailio and ends the attempt on the node. The leader settles the lead,
# closes the room, and completes the campaign.
#
# Campaign 3 has no agent. Its leads call the carrier's busy, failing, and
# unanswered numbers, and each must end with the matching outcome (the
# unanswered one at the ring timeout, by CANCEL).
#
# The checks treat Postgres and kine as managed services: they go through the
# database URL and the kine endpoint, never into a container. The carrier's
# view comes from its API (http://nat:8080/calls).
#
# Exits non-zero on any failure. Always tears down.
set -uo pipefail
cd "$(dirname "$0")"
export E2E_NET=ad-cluster_cluster
export E2E_DATABASE_URL=postgres://ad:ad@postgres:5432/ad
export E2E_ETCD_ENDPOINT=kine:2379
. ./lib.sh

CL="docker compose -f docker-compose.cluster.yml"
export E2E_HOLD_MS=8000
CALLER_ID=+15550001000
LK_STATS=http://livekit-agent:8088/stats
PC_STATS=http://pipecat-bot:8089/stats
FAIL=0

cleanup() {
  echo "== teardown =="
  agent_stats_json_of "$LK_STATS" > "$E2E_OUT/run_e2e.livekit-agent.stats.json" 2>/dev/null || true
  agent_stats_json_of "$PC_STATS" > "$E2E_OUT/run_e2e.pipecat-bot.stats.json" 2>/dev/null || true
  agent_stats_json_of "$E2E_SIM_URL" > "$E2E_OUT/run_e2e.carrier.calls.json" 2>/dev/null || true
  for s in node-1 kamailio carrier livekit livekit-agent pipecat-bot kine nat; do
    $CL logs "$s" > "$E2E_OUT/run_e2e.$s.log" 2>&1 || true
  done
  $CL down -v >/dev/null 2>&1 || true
  echo "== teardown complete (logs in $E2E_OUT/run_e2e.*.log) =="
}
trap cleanup EXIT

route_added() { [ "$($CL ps -a --format '{{.Service}} {{.State}} {{.ExitCode}}' | grep "^$1 " | cut -d' ' -f2-)" = "exited 0" ]; }

check_call() {
  local lead=$1 runtime=$2 stats=$3 pattern=$4 number=$5 room agent events
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
  # The carrier's view: challenged once, then authenticated; the caller ID;
  # the agent's tone arrived; the carrier ended the call.
  sim_eq "c['to'] == '$number' and c['invites'] == 2 and c['auth'] == 'ok' and c['from'] == '$CALLER_ID'
          and c['loud_in'] > 0 and c['status'] == 'completed' and c['ended_by'] == 'carrier'" 1 \
    && echo "ok: $runtime: the carrier saw a 407 challenge, our digest, our caller ID, and our tone" \
    || { echo "FAIL: $runtime: the carrier's record of $number is not as expected"; FAIL=1; }
}

# check_outcome <lead> <number> <outcome> <carrier status> <seconds>
check_outcome() {
  local lead=$1 number=$2 outcome=$3 status=$4 within=$5
  wait_for "$within" "lead $lead ($number) ends as $outcome" pg_eq \
    "SELECT outcome FROM call_attempts WHERE lead_id=$lead" "$outcome" || FAIL=1
  sim_eq "c['to'] == '$number' and c['auth'] == 'ok' and c['status'] == '$status'" 1 \
    && echo "ok: the carrier answered $number with '$status' after our digest" \
    || { echo "FAIL: the carrier's record of $number is not '$status'"; FAIL=1; }
}

echo "== build images =="
docker build -q -t agentdialer:e2e .. >/dev/null || { echo "FAIL: image build"; exit 1; }
$CL build -q livekit-agent pipecat-bot carrier >/dev/null 2>&1 || { echo "FAIL: test image build"; exit 1; }
rm -f "$E2E_OUT/nat.ready"

echo "== start Postgres, kine, LiveKit, the agents, Kamailio, the NAT, the carrier =="
$CL up -d postgres kine livekit livekit-agent pipecat-bot kamailio kamailio-route nat carrier 2>&1 | tail -1
wait_for 60 "the NAT is ready" test -e "$E2E_OUT/nat.ready" || exit 1
wait_for 30 "Kamailio routes to the carrier through the NAT" route_added kamailio-route || exit 1
wait_for 60 "the carrier answers its API" http_ok http://nat:8080/healthz || exit 1
docker run --rm --network "$E2E_NET" -e DATABASE_URL="$E2E_DATABASE_URL" agentdialer:e2e migrate \
  >"$E2E_OUT/run_e2e.migrate.log" 2>&1 || { echo "FAIL: migrate"; exit 1; }
echo "ok: migrations applied"
wait_for 120 "LiveKit agent worker registered" http_ok http://livekit-agent:8081/ || exit 1
wait_for 120 "pipecat bot server up" http_ok "$PC_STATS" || exit 1

echo "== start one node and its route through the NAT =="
$CL up -d node-1 node-1-route 2>&1 | tail -1
wait_for 30 "node-1 routes to the carrier through the NAT" route_added node-1-route || exit 1
wait_for 60 "the node leads (leader key in kine)" kine_leader_is node-1 || FAIL=1

echo "== seed: two agent campaigns with one lead each, and an outcome campaign =="
# One transaction: the running leader never sees a campaign without its leads
# (it completes a running campaign that has no due work).
pg "INSERT INTO campaigns (id, org_id, name, state, concurrency_cap, agent, created_at, updated_at) VALUES
    (1, 1, 'e2e-livekit', 'running', 1, '{\"runtime\":\"livekit\",\"agent_name\":\"e2e-livekit\"}', now(), now()),
    (2, 1, 'e2e-pipecat', 'running', 1, '{\"runtime\":\"pipecat\",\"start_url\":\"http://pipecat-bot:8089/start\"}', now(), now()),
    (3, 1, 'e2e-outcomes', 'running', 3, NULL, now(), now());
    SELECT setval(pg_get_serial_sequence('campaigns','id'), 3, true);
    INSERT INTO leads (id, campaign_id, phone_number, timezone, status, called_count, context, created_at, updated_at) VALUES
    (1, 1, '+15550000001', 'America/New_York', 'new', 0, '{\"name\":\"Ada\"}'::jsonb, now(), now()),
    (2, 2, '+15550000002', 'America/New_York', 'new', 0, '{\"name\":\"Bob\"}'::jsonb, now(), now()),
    (3, 3, '+15550000486', 'America/New_York', 'new', 0, '{}'::jsonb, now(), now()),
    (4, 3, '+15550000503', 'America/New_York', 'new', 0, '{}'::jsonb, now(), now()),
    (5, 3, '+15550000408', 'America/New_York', 'new', 0, '{}'::jsonb, now(), now());
    SELECT setval(pg_get_serial_sequence('leads','id'), 5, true);" >/dev/null
pg_eq "SELECT count(*) FROM leads" 5 || { echo "FAIL: seed"; exit 1; }
echo "ok: seeded"

check_call 1 livekit "$LK_STATS" 'agent-AJ_*' +15550000001
check_call 2 pipecat "$PC_STATS" 'agent-attempt-*' +15550000002

echo "== the carrier's failure answers become dialer outcomes =="
check_outcome 3 +15550000486 busy busy 60
check_outcome 4 +15550000503 failed failed 60
# No answer: the node cancels at its ring timeout (30 s), and the carrier
# answers the INVITE with 487.
check_outcome 5 +15550000408 no_answer canceled 90

wait_for 30 "the agent campaigns completed" pg_eq "SELECT count(*) FROM campaigns WHERE state='completed' AND id IN (1, 2)" 2 || FAIL=1
wait_for 20 "no call assignment left in kine" kine_none /agentdialer/calls/ || FAIL=1
NODES=$(kine_keys /agentdialer/nodes/)
[ "$NODES" = /agentdialer/nodes/node-1 ] || { echo "FAIL: kine nodes '$NODES'"; FAIL=1; }
echo "ok: kine has node $NODES"
CALLS=$(sim_count "True")
[ "$CALLS" = 5 ] || { echo "FAIL: the carrier saw $CALLS calls, want 5"; FAIL=1; }
echo "ok: the carrier saw $CALLS calls through Kamailio, one per lead"

if [ "$FAIL" -eq 0 ]; then
  echo "E2E: PASS — the node carried its calls itself through Kamailio and a NAT to a Twilio-like carrier: a LiveKit agent and a pipecat bot each joined their room before the dial, passed the 407 challenge with the caller ID, heard their tone back, and the carrier's BYE ended each attempt; busy, failure, and no answer became their outcomes"
  exit 0
else
  echo "E2E: FAIL"
  exit 1
fi
