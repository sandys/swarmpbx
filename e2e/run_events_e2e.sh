#!/usr/bin/env bash
# Event-driven e2e for the call attempts and the Postgres event layer
# (PLAN.md §0.10.7a, §0.10.8a). Subject: the orchestrator's dispatch, the
# attempt statechart, settlement, the circuit breaker, and the API's campaign
# transitions.
#
# We do NOT need the media stack here (calls are proven by run_e2e.sh and
# run_cluster_e2e.sh). Instead the trunk has only an IPv6 address, which the
# node's IPv4 SIP socket cannot reach, so every dial fails before the INVITE
# leaves (a setup error). That lets us prove, against a REAL Postgres:
#   (a) campaign 1 (capacity 1, 6 leads) dials one lead after the other. Each
#       end writes an `attempt_ended` event, and its NOTIFY starts the next
#       dial. The sweep runs every 5 s, so five dials inside 4 s need NOTIFY;
#   (b) the failures open the breaker, and the campaign is PAUSED in Postgres
#       with one `circuit_breaker_tripped` event;
#   (c) each failed lead waits for its retry (`next_eligible_at`,
#       `retry_reason`);
#   (d) a lead whose retry becomes due after startup is dialed again;
#   (e) a resume through the API resets the breaker and dials the next lead.
#
# Exits non-zero on any failure. Always tears down.
set -uo pipefail
cd "$(dirname "$0")"
export E2E_NET=ad-events_events
export E2E_DATABASE_URL=postgres://ad:ad@postgres:5432/ad
export E2E_ETCD_ENDPOINT=kine:2379
. ./lib.sh

COMPOSE="docker compose -f docker-compose.events.e2e.yml"
PSQL() { pg "$1"; }
API=http://evt-api:18091
FAIL=0

cleanup() {
  echo "== teardown =="
  docker logs evt-orch > "$E2E_OUT/run_events_e2e.orchestrator.log" 2>&1 || true
  docker logs evt-api > "$E2E_OUT/run_events_e2e.api.log" 2>&1 || true
  docker rm -f evt-orch evt-api >/dev/null 2>&1 || true
  $COMPOSE down -v >/dev/null 2>&1 || true
  echo "== teardown complete (logs: $E2E_OUT/run_events_e2e.orchestrator.log, $E2E_OUT/run_events_e2e.api.log) =="
}
trap cleanup EXIT

echo "== start postgres + kine =="
$COMPOSE up -d 2>&1 | tail -2

echo "== build image + migrate + seed =="
export DATABASE_URL="$E2E_DATABASE_URL"
docker build -q -t agentdialer:e2e .. >/dev/null
# migrate waits for Postgres by itself (up to 120 s).
docker run --rm --network "$E2E_NET" -e DATABASE_URL="$DATABASE_URL" agentdialer:e2e migrate \
  >"$E2E_OUT/run_events_e2e.migrate.log" 2>&1 || { echo "FAIL: migrate"; cat "$E2E_OUT/run_events_e2e.migrate.log"; exit 1; }

# Campaign 1: capacity 1 and 6 new leads. Five failures reach the breaker's
# min_calls (5) at a 100% failure rate, so it must trip.
PSQL "INSERT INTO campaigns (id, org_id, name, state, concurrency_cap, created_at, updated_at)
      VALUES (1,1,'evt','running',1,now(),now()) ON CONFLICT (id) DO UPDATE SET state='running';" >/dev/null
for n in 1 2 3 4 5 6; do
  PSQL "INSERT INTO leads (id,campaign_id,phone_number,timezone,status,called_count,context,created_at,updated_at)
        VALUES ($n,1,'+1000000000$n','America/New_York','new',0,'{}'::jsonb,now(),now())
        ON CONFLICT (id) DO NOTHING;" >/dev/null
done
# Campaign 2: one lead that was dialed once. Its retry is due in one hour.
PSQL "INSERT INTO campaigns (id, org_id, name, state, created_at, updated_at)
      VALUES (2,1,'evt-retry','running',now(),now()) ON CONFLICT (id) DO UPDATE SET state='running';" >/dev/null
PSQL "INSERT INTO leads (id,campaign_id,phone_number,timezone,status,called_count,context,
                         next_eligible_at,retry_reason,created_at,updated_at)
      VALUES (7,2,'+10000000007','America/New_York','queued',1,'{}'::jsonb,
              now() + interval '1 hour','no_answer',now(),now())
      ON CONFLICT (id) DO NOTHING;" >/dev/null
PSQL "SELECT setval(pg_get_serial_sequence('campaigns','id'), 2, true);" >/dev/null
PSQL "SELECT setval(pg_get_serial_sequence('leads','id'), 7, true);" >/dev/null
echo "ok: seeded campaign 1 (capacity 1, 6 leads) and campaign 2 (1 retry, due later)"

echo "== run the api (campaign transitions) =="
# The env sets must match the role contracts (crates/ad_bin/src/config.rs).
docker run -d --name evt-api --network "$E2E_NET" \
  -e AD_API_ADDR=0.0.0.0:18091 \
  -e DATABASE_URL="$DATABASE_URL" \
  -e RUST_LOG=info \
  agentdialer:e2e api >/dev/null 2>&1
wait_for 30 "api answers its health check" http_ok "$API/health" || FAIL=1

echo "== run orchestrator with a FAILING trunk (no address that it can reach) =="
docker run -d --name evt-orch --network "$E2E_NET" \
  -e DATABASE_URL="$DATABASE_URL" \
  -e LIVEKIT_URL="ws://127.0.0.1:9" \
  -e LIVEKIT_API_KEY=devkey -e LIVEKIT_API_SECRET=devsecret_devsecret_devsecret_32b \
  -e AD_SIP_TRUNK="[::1]:5060" -e AD_SIP_ADVERTISE_IP=127.0.0.1 -e AD_ROOM_PREFIX=campaign \
  -e AD_CALLS_PER_SEC=50 -e AD_RETRY_SWEEP_MS=5000 -e AD_EVENT_SWEEP_MS=60000 \
  -e AD_BATCH_LIMIT=10 -e AD_ATTEMPT_CHECK_MS=60000 \
  -e AD_ETCD_ENDPOINTS=http://kine:2379 -e HOSTNAME=evt-node \
  -e RUST_LOG=info \
  agentdialer:e2e orchestrator >/dev/null 2>&1

wait_for 60 "the node leads the cluster (leader key in kine)" bash -c ". ./lib.sh; kine_value /agentdialer/leader | grep -q evt-node" || FAIL=1

echo "== assert (a): capacity 1, so the dials follow each other through NOTIFY =="
wait_for 20 ">=5 setup errors in campaign 1" \
  pg_ge "SELECT count(*) FROM call_attempts WHERE campaign_id=1 AND outcome='setup_error'" 5 || FAIL=1
OVERLAP=$(PSQL "SELECT count(*) FROM call_attempts a JOIN call_attempts b
                ON a.campaign_id = 1 AND b.campaign_id = 1 AND a.id < b.id
               AND b.created_at < a.ended_at")
[ "$OVERLAP" = 0 ] || { echo "FAIL: $OVERLAP attempts overlapped in a campaign with capacity 1"; FAIL=1; }
SPAN_MS=$(PSQL "SELECT round(extract(epoch FROM max(ended_at) - min(created_at)) * 1000)
                FROM (SELECT * FROM call_attempts WHERE campaign_id=1 ORDER BY id LIMIT 5) f")
echo "ok: the first 5 dials took ${SPAN_MS} ms (the sweep interval is 5000 ms)"
[ "${SPAN_MS%.*}" -lt 4000 ] || { echo "FAIL: the dials did not follow each other through NOTIFY"; FAIL=1; }

echo "== assert (b): the breaker paused the campaign in Postgres, once =="
wait_for 20 "campaign 1 paused" \
  pg_eq "SELECT state FROM campaigns WHERE id=1" paused || FAIL=1
TRIPS=$(PSQL "SELECT count(*) FROM campaign_events WHERE campaign_id=1 AND kind='circuit_breaker_tripped'")
[ "$TRIPS" = 1 ] || { echo "FAIL: $TRIPS circuit_breaker_tripped events (want 1)"; FAIL=1; }
UNDIALED=$(PSQL "SELECT count(*) FROM leads WHERE campaign_id=1 AND status='new'")
[ "$UNDIALED" -ge 1 ] || { echo "FAIL: the paused campaign dialed every lead"; FAIL=1; }

echo "== assert (c): each failed lead waits for its retry =="
wait_for 20 "failed leads queued with a retry" \
  pg_ge "SELECT count(*) FROM leads WHERE campaign_id=1 AND status='queued' AND retry_reason='error' AND next_eligible_at > now() AND called_count = 1" 5 || FAIL=1
RETRIES=$(PSQL "SELECT count(*) FROM campaign_events WHERE campaign_id=1 AND kind='retry_scheduled'")
[ "$RETRIES" -ge 5 ] || { echo "FAIL: $RETRIES retry_scheduled events (want >= 5)"; FAIL=1; }

echo "== assert (d): a retry that becomes due later is dialed again =="
[ "$(PSQL "SELECT count(*) FROM call_attempts WHERE lead_id=7")" = 0 ] || { echo "FAIL: lead 7 was dialed before its retry was due"; FAIL=1; }
PSQL "UPDATE leads SET next_eligible_at = now() WHERE id=7;" >/dev/null
wait_for 20 "lead 7 dialed a second time" \
  pg_ge "SELECT called_count FROM leads WHERE id=7" 2 || FAIL=1

echo "== assert (e): a resume through the api resets the breaker and dials on =="
BEFORE=$(PSQL "SELECT count(*) FROM call_attempts WHERE campaign_id=1")
code=$(docker run --rm --network "$E2E_NET" "$E2E_PY_IMAGE" python3 -c '
import json, sys, urllib.request
req = urllib.request.Request(sys.argv[1], data=json.dumps({"transition": "resume"}).encode(),
                             headers={"content-type": "application/json"}, method="POST")
print(urllib.request.urlopen(req, timeout=5).status)' "$API/v1/campaigns/1/transition")
[ "$code" = 200 ] || { echo "FAIL: resume returned $code"; FAIL=1; }
# Without the reset, the old failures would pause the campaign again at once,
# and no lead would dial.
wait_for 15 "a lead of campaign 1 dialed after the resume" \
  pg_ge "SELECT count(*) FROM call_attempts WHERE campaign_id=1" $((BEFORE + 1)) || FAIL=1

echo "-- orchestrator logs (tail) --"; docker logs evt-orch 2>&1 | grep -iE "leader|breaker|paus|settled|complet" | tail -10
echo "-- event rows by kind --"
PSQL "SELECT campaign_id, kind, count(*) FROM campaign_events GROUP BY 1,2 ORDER BY 1,2;"

if [ "$FAIL" -eq 0 ]; then
  echo "EVENTS E2E: PASS — dials followed each other through NOTIFY within the capacity, the breaker paused the campaign in Postgres, retries were stored and dialed when due, and a resume through the api reset the breaker, over a real Postgres"
  exit 0
else
  echo "EVENTS E2E: FAIL"
  exit 1
fi
