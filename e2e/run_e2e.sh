#!/usr/bin/env bash
# Automated e2e: one agentdialer node carries calls itself on the stack of
# docker-compose.cluster.yml: Kamailio, a NAT, the carrier simulator
# (e2e/carrier-sim, which behaves like Twilio Elastic SIP Trunking), LiveKit,
# and the test agents. No host networking, no livekit/sip, no LiveKit webhook.
#
# Campaigns 1, 2, 5, and 6 have an agent: each runtime (a LiveKit agent by
# explicit dispatch, a pipecat bot by a start request) with each dial order.
# Customer first (campaigns 1 and 5): the node sends the INVITE, and on the
# answer it starts the agent, which must be in the room within 2 s. Agent
# first (campaigns 2 and 6): the node starts the agent, waits until it is in
# the room, and only then dials.
# The INVITE goes through Kamailio. The carrier challenges it like Twilio
# (407 digest); the node answers with the trunk credentials and the caller
# ID. On the answer the node joins the room and carries the audio: the
# agent's tone goes out as RTP through the NAT, the carrier sends it back
# (symmetric RTP), and the agent hears it. The carrier hangs up after 8 s; its
# BYE comes back through Kamailio and ends the attempt on the node. The leader
# settles the lead, closes the room, and completes the campaign.
#
# Campaign 3 has no agent. Its leads call the carrier's busy, failing, and
# unanswered numbers, and each must end with the matching outcome (the
# unanswered one at the ring timeout, by CANCEL).
#
# Campaign 4 dials the customer first with a bot whose start request takes
# 4 s and that never joins. Its first lead answers, and the node hangs up 2 s
# after the answer, during the start request: the outcome is "abandoned".
# The leader closes the room, and an agent that joins late cannot open it
# again. The campaign's rate is then above the 3% cap, so its second lead
# starts the agent first; no agent joins, and the carrier never sees that
# call.
#
# Campaign 7 tests join-paced dialing (GAPS.md DIAL-9). Its bot answers the
# start request after 700 ms. Through the API, the test sets the pool's
# target to 300 ms, its back-off to 600 ms, and its first limit to 2, and then
# resumes the campaign. The first two calls start at once; their slow joins
# halve the window to 1. No call is abandoned, no claim goes above its limit,
# and the faster pools grow their limit above 1.
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
API=http://api:8080
FAIL=0

cleanup() {
  echo "== teardown =="
  agent_stats_json_of "$LK_STATS" > "$E2E_OUT/run_e2e.livekit-agent.stats.json" 2>/dev/null || true
  agent_stats_json_of "$PC_STATS" > "$E2E_OUT/run_e2e.pipecat-bot.stats.json" 2>/dev/null || true
  agent_stats_json_of "$E2E_SIM_URL" > "$E2E_OUT/run_e2e.carrier.calls.json" 2>/dev/null || true
  for s in node-1 api kamailio carrier livekit livekit-agent pipecat-bot kine nat; do
    $CL logs "$s" > "$E2E_OUT/run_e2e.$s.log" 2>&1 || true
  done
  $CL down -v >/dev/null 2>&1 || true
  echo "== teardown complete (logs in $E2E_OUT/run_e2e.*.log) =="
}
trap cleanup EXIT

route_added() { [ "$($CL ps -a --format '{{.Service}} {{.State}} {{.ExitCode}}' | grep "^$1 " | cut -d' ' -f2-)" = "exited 0" ]; }

# check_call <lead> <runtime> <stats URL> <agent identity glob> <number> <dial order>
check_call() {
  local lead=$1 runtime=$2 stats=$3 pattern=$4 number=$5 order=$6 room agent events want order_sql gap
  echo "== $runtime ($order): lead $lead =="
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
  if [ "$order" = customer_first ]; then
    want=dial_started,answered,agent_starting,agent_ready,hangup
    order_sql="(a.snapshot->>'agent_ready_at')::timestamptz - a.answered_at BETWEEN interval '0' AND interval '2 seconds'"
  else
    want=agent_starting,agent_ready,dial_started,answered,hangup
    order_sql="(a.snapshot->>'agent_ready_at')::timestamptz <= a.dialed_at"
  fi
  [ "$events" = "$want" ] && echo "ok: $runtime attempt events $events" \
    || { echo "FAIL: $runtime attempt events were '$events', want '$want'"; FAIL=1; }
  if [ "$order" = customer_first ]; then
    gap="$(pg "SELECT round(extract(epoch FROM (snapshot->>'agent_ready_at')::timestamptz - answered_at) * 1000) FROM call_attempts WHERE lead_id=$lead") ms after the answer"
  else
    gap="$(pg "SELECT round(extract(epoch FROM dialed_at - (snapshot->>'agent_ready_at')::timestamptz) * 1000) FROM call_attempts WHERE lead_id=$lead") ms before the dial"
  fi
  pg_eq "SELECT $order_sql FROM call_attempts a WHERE a.lead_id=$lead" t \
    && echo "ok: $runtime: the agent was in the room $gap" \
    || { echo "FAIL: $runtime: the agent was in the room $gap; wrong for $order"; FAIL=1; }
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

echo "== start one node and its route through the NAT, and the API =="
$CL up -d node-1 node-1-route api 2>&1 | tail -1
wait_for 30 "node-1 routes to the carrier through the NAT" route_added node-1-route || exit 1
wait_for 60 "the node leads (leader key in kine)" kine_leader_is node-1 || FAIL=1
wait_for 60 "the API answers" http_ok "$API/health" || exit 1

echo "== seed: four agent campaigns with one lead each, an outcome campaign, an abandonment campaign, and a paused pacing campaign =="
# One transaction: the running leader never sees a campaign without its leads
# (it completes a running campaign that has no due work).
pg "INSERT INTO campaigns (id, org_id, name, state, concurrency_cap, agent, created_at, updated_at) VALUES
    (1, 1, 'e2e-livekit', 'running', 1, '{\"runtime\":\"livekit\",\"agent_name\":\"e2e-livekit\"}', now(), now()),
    (2, 1, 'e2e-pipecat', 'running', 1, '{\"runtime\":\"pipecat\",\"start_url\":\"http://pipecat-bot:8089/start\",\"dial_order\":\"agent_first\"}', now(), now()),
    (3, 1, 'e2e-outcomes', 'running', 3, NULL, now(), now()),
    (4, 1, 'e2e-abandon', 'running', 1, '{\"runtime\":\"pipecat\",\"start_url\":\"http://pipecat-bot:8089/start?hold_ms=4000&join=0\",\"join_timeout_s\":3}', now(), now()),
    (5, 1, 'e2e-pipecat-cf', 'running', 1, '{\"runtime\":\"pipecat\",\"start_url\":\"http://pipecat-bot:8089/start\"}', now(), now()),
    (6, 1, 'e2e-livekit-af', 'running', 1, '{\"runtime\":\"livekit\",\"agent_name\":\"e2e-livekit\",\"dial_order\":\"agent_first\"}', now(), now()),
    (7, 1, 'e2e-slow-pool', 'paused', 6, '{\"runtime\":\"pipecat\",\"start_url\":\"http://pipecat-bot:8089/start?hold_ms=700\"}', now(), now());
    SELECT setval(pg_get_serial_sequence('campaigns','id'), 7, true);
    INSERT INTO leads (id, campaign_id, phone_number, timezone, status, called_count, context, created_at, updated_at) VALUES
    (1, 1, '+15550000001', 'America/New_York', 'new', 0, '{\"name\":\"Ada\"}'::jsonb, now(), now()),
    (2, 2, '+15550000002', 'America/New_York', 'new', 0, '{\"name\":\"Bob\"}'::jsonb, now(), now()),
    (3, 3, '+15550000486', 'America/New_York', 'new', 0, '{}'::jsonb, now(), now()),
    (4, 3, '+15550000503', 'America/New_York', 'new', 0, '{}'::jsonb, now(), now()),
    (5, 3, '+15550000408', 'America/New_York', 'new', 0, '{}'::jsonb, now(), now()),
    (6, 4, '+15550000011', 'America/New_York', 'new', 0, '{}'::jsonb, now(), now()),
    (7, 4, '+15550000012', 'America/New_York', 'new', 0, '{}'::jsonb, now(), now()),
    (8, 5, '+15550000003', 'America/New_York', 'new', 0, '{\"name\":\"Cy\"}'::jsonb, now(), now()),
    (9, 6, '+15550000004', 'America/New_York', 'new', 0, '{\"name\":\"Di\"}'::jsonb, now(), now()),
    (10, 7, '+15550000021', 'America/New_York', 'new', 0, '{}'::jsonb, now(), now()),
    (11, 7, '+15550000022', 'America/New_York', 'new', 0, '{}'::jsonb, now(), now()),
    (12, 7, '+15550000023', 'America/New_York', 'new', 0, '{}'::jsonb, now(), now()),
    (13, 7, '+15550000024', 'America/New_York', 'new', 0, '{}'::jsonb, now(), now()),
    (14, 7, '+15550000025', 'America/New_York', 'new', 0, '{}'::jsonb, now(), now()),
    (15, 7, '+15550000026', 'America/New_York', 'new', 0, '{}'::jsonb, now(), now());
    SELECT setval(pg_get_serial_sequence('leads','id'), 15, true);" >/dev/null
pg_eq "SELECT count(*) FROM leads" 15 || { echo "FAIL: seed"; exit 1; }
echo "ok: seeded"

check_call 1 livekit "$LK_STATS" 'agent-AJ_*' +15550000001 customer_first
check_call 2 pipecat "$PC_STATS" 'agent-attempt-*' +15550000002 agent_first
check_call 8 pipecat "$PC_STATS" 'agent-attempt-*' +15550000003 customer_first
check_call 9 livekit "$LK_STATS" 'agent-AJ_*' +15550000004 agent_first

echo "== customer first: no agent in the room 2 s after the answer abandons the call =="
wait_for 60 "lead 6 ends as abandoned" pg_eq "SELECT outcome FROM call_attempts WHERE lead_id=6" abandoned || FAIL=1
# The bot's start request takes 4 s. The limit runs during it, so the node
# hangs up before the request answers.
EVENTS=$(pg "SELECT string_agg(e.kind, ',' ORDER BY e.seq) FROM attempt_events e JOIN call_attempts a ON a.id = e.attempt_id WHERE a.lead_id=6")
[ "$EVENTS" = dial_started,answered,agent_failed ] || { echo "FAIL: lead 6 events were '$EVENTS'"; FAIL=1; }
HELD=$(pg "SELECT round(extract(epoch FROM ended_at - answered_at) * 1000) FROM call_attempts WHERE lead_id=6")
[ "${HELD:-0}" -ge 2000 ] && [ "${HELD:-0}" -le 3000 ] \
  && echo "ok: the node hung up $HELD ms after the answer, while the start request was in flight: $(pg "SELECT detail FROM call_attempts WHERE lead_id=6")" \
  || { echo "FAIL: lead 6 ended $HELD ms after the answer, want 2000-3000"; FAIL=1; }
sim_eq "c['to'] == '+15550000011' and c['status'] == 'completed' and c['ended_by'] == 'caller'" 1 \
  && echo "ok: the carrier saw our BYE for the abandoned call" \
  || { echo "FAIL: the carrier's record of +15550000011 is not 'completed' by us"; FAIL=1; }
wait_for 15 "lead 6 waits for a retry as abandoned" pg_eq \
  "SELECT status || ':' || retry_reason || ':' || called_count FROM leads WHERE id=6" queued:abandoned:1 || FAIL=1
ROOM6=$(pg "SELECT room FROM call_attempts WHERE lead_id=6")
wait_for 30 "the leader closed $ROOM6" lk_room_gone "$ROOM6" || FAIL=1
LATE=$(lk_late_join "$ROOM6")
[ "$LATE" = refused ] && lk_room_gone "$ROOM6" && echo "ok: an agent that joins late cannot open $ROOM6 again" \
  || { echo "FAIL: a late join of $ROOM6 was '$LATE'; the room must stay closed"; FAIL=1; }

echo "== above the cap, the next lead starts the agent first, and no customer waits =="
wait_for 60 "lead 7 was not dialed: no agent joined before the dial" pg_ge \
  "SELECT count(*) FROM call_attempts WHERE lead_id=7 AND outcome = 'not_dialed'" 1 || FAIL=1
EVENTS=$(pg "SELECT string_agg(e.kind, ',' ORDER BY e.seq) FROM attempt_events e WHERE e.attempt_id = (SELECT min(id) FROM call_attempts WHERE lead_id=7)")
[[ "$EVENTS" == *agent_failed && "$EVENTS" != *dial_started* ]] && echo "ok: lead 7 events $EVENTS" \
  || { echo "FAIL: lead 7 events were '$EVENTS'"; FAIL=1; }
sim_eq "c['to'] == '+15550000012'" 0 && echo "ok: the carrier never saw lead 7" \
  || { echo "FAIL: the carrier saw a call to lead 7"; FAIL=1; }
pg "UPDATE campaigns SET state = 'paused', updated_at = now() WHERE id = 4" >/dev/null

echo "== the carrier's failure answers become dialer outcomes =="
check_outcome 3 +15550000486 busy busy 60
check_outcome 4 +15550000503 failed failed 60
# No answer: the node cancels at its ring timeout (30 s), and the carrier
# answers the INVITE with 487.
check_outcome 5 +15550000408 no_answer canceled 90

wait_for 30 "the agent campaigns completed" pg_eq "SELECT count(*) FROM campaigns WHERE state='completed' AND id IN (1, 2, 5, 6)" 4 || FAIL=1

echo "== join-paced dialing: a slow agent pool gets a smaller window; the API sets its values at run time =="
SLOW_POOL='pipecat:http://pipecat-bot:8089/start?hold_ms=700'
P7=$API/v1/campaigns/7/pacing
OUT=$(api_send PUT "$P7" '{"pool": {"target_ms": 300, "backoff_ms": 600, "first_k": 2}, "reset": true}')
[ "${OUT%% *}" = 200 ] && echo "ok: the API set the pool of campaign 7: target 300 ms, back-off 600 ms, first limit 2" \
  || { echo "FAIL: PUT $P7 answered '$OUT'"; FAIL=1; }
[ "$(api_field "$P7" "v['pool']['pool']")" = "$SLOW_POOL" ] && [ "$(api_field "$P7" "v['limit']")" = 2 ] \
  && echo "ok: campaign 7 dials through the pool $SLOW_POOL with a limit of 2" \
  || { echo "FAIL: the pacing of campaign 7 is '$(api_send GET "$P7")'"; FAIL=1; }
OUT=$(api_send POST "$API/v1/campaigns/7/transition" '{"transition": "resume"}')
[ "${OUT%% *}" = 200 ] || { echo "FAIL: resume of campaign 7 answered '$OUT'"; FAIL=1; }
wait_for 150 "campaign 7 completed" pg_eq "SELECT state FROM campaigns WHERE id=7" completed || FAIL=1
pg_eq "SELECT count(*) FILTER (WHERE outcome = 'completed') || ':' || count(*) FILTER (WHERE outcome = 'abandoned')
       FROM call_attempts WHERE campaign_id=7" 6:0 \
  && echo "ok: all six calls of campaign 7 completed, and none was abandoned" \
  || { echo "FAIL: campaign 7 outcomes: $(pg "SELECT string_agg(outcome, ',' ORDER BY id) FROM call_attempts WHERE campaign_id=7")"; FAIL=1; }
LIMITS=$(pg "SELECT string_agg(pool_window || '/' || pool_limit, ' ' ORDER BY id) FROM call_attempts WHERE campaign_id=7")
pg_eq "SELECT count(*) FROM call_attempts WHERE campaign_id=7 AND (pool IS DISTINCT FROM '$SLOW_POOL' OR pool_window > pool_limit)" 0 \
  && echo "ok: no claim went above its limit (window/limit at each claim: $LIMITS)" \
  || { echo "FAIL: a claim went above its limit (window/limit at each claim: $LIMITS)"; FAIL=1; }
# The same check from the times alone: at each claim, the earlier attempts of
# the pool whose agent was not in the room yet, and this one, fit the limit.
# 200 ms covers the time from the start of a claim's or a join's transaction
# to its count or commit; every join takes longer.
pg_eq "SELECT count(*) FROM call_attempts a WHERE a.campaign_id=7 AND 1 + (
         SELECT count(*) FROM call_attempts b WHERE b.pool = a.pool AND b.id < a.id
           AND $(left_window b) > a.created_at + interval '200 milliseconds') > a.pool_limit" 0 \
  && echo "ok: the attempt times agree: no claim found a full window" \
  || { echo "FAIL: the attempt times show a claim into a full window"; FAIL=1; }
JOINS=$(pg "SELECT string_agg(round(extract(epoch FROM agent_ready_at - answered_at) * 1000)::text, ',' ORDER BY id) FROM call_attempts WHERE campaign_id=7")
STATE="k=$(api_field "$P7" "v['pool']['k']") slow=$(api_field "$P7" "v['pool']['slow']") limit=$(api_field "$P7" "v['limit']")"
[ "$STATE" = "k=1 slow=6 limit=1" ] && [[ "$LIMITS" == "1/2 2/2 "* && "$LIMITS" == *" 1/1" ]] \
  && echo "ok: the joins ($JOINS ms after the answer) were above the 600 ms back-off; the limit went from 2 to 1 ($STATE)" \
  || { echo "FAIL: pool state '$STATE', claims '$LIMITS', joins '$JOINS' ms; want the limit to go from 2 to 1"; FAIL=1; }
FAST=$(api_field "$API/v1/campaigns/5/pacing" "v['pool']['k']")
[ "${FAST:-0}" -ge 2 ] 2>/dev/null \
  && echo "ok: the fast pipecat pool grew its limit to $FAST (p50 $(api_field "$API/v1/campaigns/5/pacing" "v['pool']['recent']['p50_ms']") ms)" \
  || { echo "FAIL: the fast pipecat pool has k='$FAST', want at least 2"; FAIL=1; }
echo "ok: the pools: $(api_field "$API/v1/agent-pools" "'; '.join(p['pool'] + ' k=' + str(p['k']) + ' joins=' + str(p['samples']) for p in v)")"
wait_for 20 "no call assignment left in kine" kine_none /agentdialer/calls/ || FAIL=1
NODES=$(kine_keys /agentdialer/nodes/)
[ "$NODES" = /agentdialer/nodes/node-1 ] || { echo "FAIL: kine nodes '$NODES'"; FAIL=1; }
echo "ok: kine has node $NODES"
CALLS=$(sim_count "True")
[ "$CALLS" = 14 ] || { echo "FAIL: the carrier saw $CALLS calls, want 14"; FAIL=1; }
echo "ok: the carrier saw $CALLS calls through Kamailio, one per dialed lead"

if [ "$FAIL" -eq 0 ]; then
  echo "E2E: PASS — the node carried its calls itself through Kamailio and a NAT to a Twilio-like carrier: a LiveKit agent and a pipecat bot each joined after the answer (customer first) and before the dial (agent first); each call passed the 407 challenge with the caller ID, heard its tone back, and the carrier's BYE ended each attempt; busy, failure, and no answer became their outcomes; an answer with no agent in 2 s was abandoned, and the campaign then dialed agent first; a slow agent pool got a smaller window from the values set through the API, and no claim went above its limit"
  exit 0
else
  echo "E2E: FAIL"
  exit 1
fi
