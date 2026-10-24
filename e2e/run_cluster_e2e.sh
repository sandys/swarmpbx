#!/usr/bin/env bash
# Cluster e2e (docker-compose.cluster.yml): three agentdialer nodes carry the
# calls themselves, SIP and RTP, behind Kamailio, with a LiveKit agent and a
# pipecat bot. No host networking, no livekit/sip, no LiveKit webhook.
#
#   (a) every node registers in kine, and exactly one leads;
#   (b) 24 calls (12 per agent runtime) at a node capacity of 2 spread over all
#       three nodes, never more than 2 per node. (Six calls at once keep the
#       Python test agents, one process per runtime, within their CPU.)
#   (c) each call goes node -> Kamailio -> carrier. The carrier's BYE goes
#       back through Kamailio to the node that owns the call, and ends it;
#   (d) the audio crosses the NAT both ways: the agent's tone goes out as RTP,
#       the carrier (SIPp, -rtp_echo, latching) sends it back to the address it
#       saw, and the agent hears its own tone in the room;
#   (e) a node that dies drops its live calls (its media dies with it); they
#       end and are never dialed again. The leader dies too; the last node
#       leads and finishes the work;
#   (f) every lead is dialed exactly once (Postgres and the carrier agree), and
#       both campaigns complete.
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
N=24
export E2E_HOLD_MS=20000
LK_STATS=http://livekit-agent:8088/stats
PC_STATS=http://pipecat-bot:8089/stats
# A call ended by the carrier: its hangup event names the callee.
CARRIER_BYE="e.kind = 'hangup' AND e.payload->>'detail' = 'the callee hung up'"
FAIL=0

cleanup() {
  echo "== teardown =="
  agent_stats_json_of "$LK_STATS" > "$E2E_OUT/run_cluster_e2e.livekit-agent.stats.json" 2>/dev/null || true
  agent_stats_json_of "$PC_STATS" > "$E2E_OUT/run_cluster_e2e.pipecat-bot.stats.json" 2>/dev/null || true
  agent_stats_json_of "$E2E_SIM_URL" > "$E2E_OUT/run_cluster_e2e.carrier.calls.json" 2>/dev/null || true
  for s in node-1 node-2 node-3 kamailio carrier livekit livekit-agent pipecat-bot kine nat; do
    $CL logs "$s" > "$E2E_OUT/run_cluster_e2e.$s.log" 2>&1 || true
  done
  $CL down -v >/dev/null 2>&1 || true
  echo "== teardown complete (logs in $E2E_OUT/run_cluster_e2e.*.log) =="
}
trap cleanup EXIT

node_cid() { $CL ps -q "$1"; }
leader_node() { kine_value /agentdialer/leader | sed -n 's/.*"node":"\([^"]*\)".*/\1/p'; }
# route_added <service>: the one-shot route sidecar exited with 0.
route_added() { [ "$($CL ps -a --format '{{.Service}} {{.State}} {{.ExitCode}}' | grep "^$1 " | cut -d' ' -f2-)" = "exited 0" ]; }

echo "== build images =="
docker build -q -t agentdialer:e2e .. >/dev/null || { echo "FAIL: image build"; exit 1; }
$CL build -q livekit-agent pipecat-bot carrier >/dev/null 2>&1 || { echo "FAIL: test image build"; exit 1; }
rm -f "$E2E_OUT/nat.ready"

echo "== start Postgres, kine, LiveKit, the agents, Kamailio, the NAT, the carrier =="
$CL up -d postgres kine livekit livekit-agent pipecat-bot kamailio kamailio-route nat carrier 2>&1 | tail -1
wait_for 60 "the NAT is ready" test -e "$E2E_OUT/nat.ready" || exit 1
wait_for 30 "Kamailio routes to the carrier through the NAT" route_added kamailio-route || exit 1
wait_for 60 "the carrier answers its API" http_ok http://nat:8080/healthz || exit 1

echo "== migrate (waits for Postgres by itself) =="
docker run --rm --network "$E2E_NET" -e DATABASE_URL="$E2E_DATABASE_URL" agentdialer:e2e migrate \
  >"$E2E_OUT/run_cluster_e2e.migrate.log" 2>&1 || { echo "FAIL: migrate"; exit 1; }
echo "ok: migrations applied"

echo "== the test agents are ready (their own HTTP endpoints) =="
wait_for 120 "LiveKit agent worker registered" http_ok http://livekit-agent:8081/ || exit 1
wait_for 120 "pipecat bot server up" http_ok "$PC_STATS" || exit 1

echo "== start three nodes and their routes through the NAT =="
$CL up -d node-1 node-2 node-3 node-1-route node-2-route node-3-route 2>&1 | tail -1
for n in node-1 node-2 node-3; do
  wait_for 30 "$n routes to the carrier through the NAT" route_added "$n-route" || exit 1
done

echo "== assert (a): three nodes in kine, one leader =="
wait_for 60 "three node keys" kine_count /agentdialer/nodes/ 3 || FAIL=1
wait_for 60 "a leader" bash -c ". ./lib.sh; [ -n \"\$(kine_value /agentdialer/leader)\" ]" || FAIL=1
LEADER=$(leader_node)
echo "ok: nodes $(kine_keys /agentdialer/nodes/ | sed 's|.*/||' | tr '\n' ' ')leader $LEADER"

echo "== seed: a LiveKit-agent campaign and a pipecat-bot campaign, $N leads =="
# One transaction: the running leader never sees a campaign without its leads
# (it completes a running campaign that has no due work).
pg "INSERT INTO campaigns (id, org_id, name, state, concurrency_cap, agent, created_at, updated_at) VALUES
    (1, 1, 'cluster-livekit', 'running', 12, '{\"runtime\":\"livekit\",\"agent_name\":\"e2e-livekit\"}', now(), now()),
    (2, 1, 'cluster-pipecat', 'running', 12, '{\"runtime\":\"pipecat\",\"start_url\":\"http://pipecat-bot:8089/start\"}', now(), now());
    SELECT setval(pg_get_serial_sequence('campaigns','id'), 2, true);
    INSERT INTO leads (campaign_id, phone_number, timezone, status, called_count, context, created_at, updated_at)
    SELECT 1 + (g % 2), '+1555000' || lpad(g::text, 4, '0'), 'America/New_York', 'new', 0, '{}'::jsonb, now(), now()
    FROM generate_series(1, $N) g;" >/dev/null
pg_eq "SELECT count(*) FROM leads" "$N" || { echo "FAIL: seed"; exit 1; }
echo "ok: seeded $N leads"

echo "== assert (b): calls spread over all nodes within the capacity =="
CAPACITY=2
wait_for 120 "$((3 * CAPACITY)) calls connected" pg_ge "SELECT count(*) FROM call_attempts WHERE state='connected'" $((3 * CAPACITY)) || FAIL=1
pg "SELECT node, count(*) FROM call_attempts WHERE state <> 'ended' GROUP BY node ORDER BY node;" | tee "$E2E_OUT/cluster_spread.txt"
SPREAD=$(pg "SELECT count(DISTINCT node) FROM call_attempts WHERE state <> 'ended'")
MAXN=$(pg "SELECT coalesce(max(c), 0) FROM (SELECT count(*) c FROM call_attempts WHERE state <> 'ended' GROUP BY node) x")
[ "$SPREAD" = 3 ] || { echo "FAIL: live calls on $SPREAD nodes, want 3"; FAIL=1; }
[ "${MAXN:-99}" -le "$CAPACITY" ] || { echo "FAIL: a node ran $MAXN calls, capacity $CAPACITY"; FAIL=1; }
echo "ok: live calls on $SPREAD nodes, at most $MAXN per node"

echo "== assert (c, d): the carrier's BYE ended calls through Kamailio; the agents heard the round trip =="
wait_for 90 "a call ended by the carrier's BYE" pg_ge \
  "SELECT count(*) FROM call_attempts a WHERE outcome='completed'
     AND EXISTS (SELECT 1 FROM attempt_events e WHERE e.attempt_id = a.id AND $CARRIER_BYE)" 1 || FAIL=1
FIRST=$(pg "SELECT a.id, a.campaign_id, a.room FROM call_attempts a WHERE outcome='completed'
              AND EXISTS (SELECT 1 FROM attempt_events e WHERE e.attempt_id = a.id AND $CARRIER_BYE)
            ORDER BY a.id LIMIT 1")
IFS='|' read -r FIRST_ID FIRST_C FIRST_ROOM <<<"$FIRST"
STATS=$([ "$FIRST_C" = 1 ] && echo "$LK_STATS" || echo "$PC_STATS")
LOUD=$(room_stat "$STATS" "$FIRST_ROOM" loud_in)
[ "${LOUD:-0}" -gt 0 ] 2>/dev/null || { echo "FAIL: the agent in $FIRST_ROOM heard no tone back (loud_in=${LOUD:-none})"; FAIL=1; }
echo "ok: attempt $FIRST_ID ended by the carrier's BYE; the agent heard ${LOUD:-0} loud frames back through the NAT"

echo "== assert (e): kill a node that does not lead; its calls end, never dialed again =="
VICTIM=$(for n in node-1 node-2 node-3; do [ "$n" != "$LEADER" ] && echo "$n" && break; done)
wait_for 30 "every live call of $VICTIM connected" pg_eq \
  "SELECT count(*) FROM call_attempts WHERE node='$VICTIM' AND state IN ('pending','dialing','uncertain')" 0 || FAIL=1
DROPPED=$(pg "SELECT string_agg(id::text, ',') FROM call_attempts WHERE node='$VICTIM' AND state <> 'ended'")
docker kill "$(node_cid "$VICTIM")" >/dev/null
echo "ok: killed $VICTIM with live calls ${DROPPED:-none}"
wait_for 20 "$VICTIM node key expired" kine_count /agentdialer/nodes/ 2 || FAIL=1
if [ -n "$DROPPED" ]; then
  wait_for 60 "the calls of $VICTIM ended" pg_eq \
    "SELECT count(*) FROM call_attempts WHERE id IN ($DROPPED) AND state <> 'ended'" 0 || FAIL=1
  pg "SELECT id, node, assign_gen, outcome FROM call_attempts WHERE id IN ($DROPPED) ORDER BY id;"
fi

echo "== assert (e): kill the leader; the last node leads =="
wait_for 60 "every live call connected" pg_eq \
  "SELECT count(*) FROM call_attempts WHERE state IN ('pending','dialing','uncertain')" 0 || FAIL=1
docker kill "$(node_cid "$LEADER")" >/dev/null
echo "ok: killed the leader $LEADER"
SURVIVOR=$(for n in node-1 node-2 node-3; do [ "$n" != "$LEADER" ] && [ "$n" != "$VICTIM" ] && echo "$n"; done)
wait_for 30 "$SURVIVOR leads" kine_leader_is "$SURVIVOR" || FAIL=1

echo "== assert (f): every lead dialed once; every call ended =="
wait_for 300 "all $N attempts ended and settled" pg_eq \
  "SELECT count(*) FROM call_attempts WHERE state='ended' AND settled_at IS NOT NULL" "$N" || FAIL=1
pg "SELECT outcome, count(*) FROM call_attempts GROUP BY 1 ORDER BY 1;" | tee "$E2E_OUT/cluster_outcomes.txt"
DIALS=$(pg "SELECT count(*) FROM call_attempts WHERE dialed_at IS NOT NULL")
TWICE=$(pg "SELECT count(*) FROM (SELECT lead_id FROM call_attempts WHERE dialed_at IS NOT NULL GROUP BY lead_id HAVING count(*) > 1) x")
BYES=$(pg "SELECT count(*) FROM call_attempts a WHERE EXISTS (SELECT 1 FROM attempt_events e WHERE e.attempt_id = a.id AND $CARRIER_BYE)")
CALLS=$(sim_count "True")
AUTHED=$(sim_count "c['auth'] == 'ok' and c['invites'] == 2")
[ "$DIALS" = "$N" ] && [ "$TWICE" = 0 ] || { echo "FAIL: $DIALS dials, $TWICE leads dialed twice"; FAIL=1; }
[ "$CALLS" = "$N" ] || { echo "FAIL: the carrier saw $CALLS calls, want $N"; FAIL=1; }
[ "$AUTHED" = "$N" ] || { echo "FAIL: $AUTHED calls passed the 407 challenge, want $N"; FAIL=1; }
echo "ok: $DIALS dials, $CALLS calls at the carrier (each challenged, then authenticated), $TWICE leads dialed twice, $BYES calls ended by the carrier's BYE"
for c in 1 2; do
  AGENTS=$(pg "SELECT count(*) FROM call_attempts WHERE campaign_id=$c AND agent_identity IS NOT NULL")
  [ "$AGENTS" = $((N / 2)) ] || { echo "FAIL: campaign $c had $AGENTS calls with an agent"; FAIL=1; }
done
echo "ok: both runtimes joined every call; nodes used: $(pg "SELECT string_agg(DISTINCT node, ' ') FROM call_attempts")"
# Every call that the carrier ended carried the agent's tone both ways.
QUIET=0
while IFS='|' read -r c room; do
  [ -n "$room" ] || continue
  s=$([ "$c" = 1 ] && echo "$LK_STATS" || echo "$PC_STATS")
  l=$(room_stat "$s" "$room" loud_in)
  [ "${l:-0}" -gt 0 ] 2>/dev/null || { echo "  no tone back in $room"; QUIET=$((QUIET + 1)); }
done < <(pg "SELECT a.campaign_id, a.room FROM call_attempts a
             WHERE EXISTS (SELECT 1 FROM attempt_events e WHERE e.attempt_id = a.id AND $CARRIER_BYE)")
if [ "$QUIET" = 0 ]; then
  echo "ok: every call that the carrier ended carried audio both ways"
else
  echo "FAIL: $QUIET calls ended by the carrier carried no tone back"
  FAIL=1
fi
wait_for 30 "both campaigns completed" pg_eq "SELECT count(*) FROM campaigns WHERE state='completed'" 2 || FAIL=1
wait_for 30 "no call assignment left in kine" kine_none /agentdialer/calls/ || FAIL=1

if [ "$FAIL" -eq 0 ]; then
  echo "CLUSTER E2E: PASS — three nodes carried $N calls themselves through Kamailio and a NAT, with a LiveKit agent and a pipecat bot; the carrier's BYE reached each call's node, the audio crossed both ways, dead nodes' calls ended without a second dial, and every lead was dialed exactly once"
  exit 0
else
  echo "CLUSTER E2E: FAIL"
  exit 1
fi
