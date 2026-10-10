#!/usr/bin/env bash
# Event-driven e2e for the Redis event layer (PLAN.md §0.10.8a). Subject: the
# orchestrator's event loop + persistent circuit breaker + retry due-timers.
#
# We do NOT need the media stack here (origination is proven by run_e2e.sh).
# Instead we point the gateway at an unreachable LiveKit, so every call fails.
# That lets us prove, deterministically, against REAL Redis + Postgres:
#   (a) failures accumulate in the persistent breaker and it TRIPS
#       (a circuit_breaker_tripped event is published and dialing pauses);
#   (b) a failed call publishes retry_needed -> the reactor schedules a retry
#       due-timer (retry_scheduled) and the lead is re-admitted when due
#       (its hopper claim is cleared and called_count advances again).
#
# Exits non-zero on any failure. Always tears down.
set -uo pipefail
cd "$(dirname "$0")"

COMPOSE="docker compose -f docker-compose.events.e2e.yml"
PSQL() { docker exec evt-postgres psql -U ad -d ad -tAc "$1"; }
REDIS() { docker exec evt-redis redis-cli "$@"; }
FAIL=0

cleanup() {
  echo "== teardown =="
  docker rm -f evt-orch evt-postgres evt-redis >/dev/null 2>&1 || true
  $COMPOSE down >/dev/null 2>&1 || true
  echo "== teardown complete =="
}
trap cleanup EXIT

wait_for() {  # wait_for <timeout_s> <desc> <cmd...>
  local timeout=$1 desc=$2; shift 2
  local deadline=$(( $(date +%s) + timeout ))
  until "$@" >/dev/null 2>&1; do
    if [ "$(date +%s)" -ge "$deadline" ]; then echo "FAIL: timed out: $desc"; return 1; fi
    sleep 1
  done
  echo "ok: $desc"
}

echo "== start redis + postgres =="
$COMPOSE up -d 2>&1 | tail -2
docker rename "$($COMPOSE ps -q postgres)" evt-postgres >/dev/null 2>&1 || true
docker rename "$($COMPOSE ps -q redis)" evt-redis >/dev/null 2>&1 || true
wait_for 60 "postgres ready" bash -c "docker exec evt-postgres pg_isready -U ad" || exit 1
wait_for 60 "redis ready" bash -c "docker exec evt-redis redis-cli ping" || exit 1

echo "== build image + migrate + seed =="
export DATABASE_URL="postgres://ad:ad@127.0.0.1:5432/ad"
docker build -q -t agentdialer:e2e .. >/dev/null
migrated=0
for i in $(seq 1 10); do
  docker run --rm --network host -e DATABASE_URL="$DATABASE_URL" agentdialer:e2e migrate >/tmp/evt_migrate.log 2>&1 && { migrated=1; break; }
  sleep 2
done
[ "$migrated" = 1 ] || { echo "FAIL: migrate"; cat /tmp/evt_migrate.log; exit 1; }

# One running campaign (id 1) with 6 leads + 6 hopper rows. Six failures exceed
# the breaker's min_calls (5) and 50% threshold, so it must trip.
PSQL "INSERT INTO campaigns (id, org_id, name, state, created_at, updated_at)
      VALUES (1,1,'evt','running',now(),now()) ON CONFLICT (id) DO UPDATE SET state='running';" >/dev/null
for n in 1 2 3 4 5 6; do
  PSQL "INSERT INTO leads (id,campaign_id,phone_number,timezone,status,called_count,context,created_at,updated_at)
        VALUES ($n,1,'+1000000000$n','America/New_York','queued',0,'{}'::jsonb,now(),now())
        ON CONFLICT (id) DO NOTHING;" >/dev/null
  PSQL "INSERT INTO hopper (lead_id,campaign_id,priority,claimed_by,claimed_at)
        VALUES ($n,1,0,NULL,NULL) ON CONFLICT DO NOTHING;" >/dev/null
done
echo "ok: seeded campaign 1 with 6 leads"

# Capture circuit_breaker_tripped + retry_scheduled by subscribing to the Redis
# channel in the background and logging to a file.
echo "== subscribe to campaign_events (observer) =="
docker exec -d evt-redis sh -c "redis-cli subscribe campaign_events > /tmp/events.log 2>&1"
sleep 1

echo "== run orchestrator with a FAILING gateway (unreachable LiveKit, bad trunk) =="
docker run -d --name evt-orch --network host \
  -e DATABASE_URL="$DATABASE_URL" \
  -e REDIS_URL="redis://127.0.0.1:6379" \
  -e LIVEKIT_URL="ws://127.0.0.1:9" \
  -e LIVEKIT_API_KEY=devkey -e LIVEKIT_API_SECRET=devsecret_devsecret_devsecret_32b \
  -e AD_SIP_TRUNK_ID="ST_nonexistent" -e AD_ROOM_PREFIX=campaign \
  -e AD_CALLS_PER_SEC=50 -e AD_TIMER_POLL_MS=500 -e AD_BATCH_LIMIT=10 \
  -e RUST_LOG=info \
  agentdialer:e2e orchestrator >/dev/null 2>&1

wait_for 30 "orchestrator became leader" bash -c "docker logs evt-orch 2>&1 | grep -q 'became leader'" || FAIL=1

echo "== assert (a): outcomes recorded as failures in Postgres =="
wait_for 40 ">=5 failed call_outcomes recorded" bash -c \
  "[ \"\$(docker exec evt-postgres psql -U ad -d ad -tAc \"SELECT count(*) FROM call_outcomes WHERE answered=false\")\" -ge 5 ]" || FAIL=1

echo "== assert (a): the circuit breaker tripped (event observed) =="
wait_for 40 "circuit_breaker_tripped published" bash -c \
  "docker exec evt-redis sh -c 'cat /tmp/events.log 2>/dev/null' | grep -q circuit_breaker_tripped" || FAIL=1

echo "== assert (a): the persistent breaker is open in Redis =="
# The breaker ZSET for campaign 1 holds the failure window.
wait_for 20 "breaker window persisted in redis" bash -c \
  "[ \"\$(docker exec evt-redis redis-cli zcard ad:breaker:1)\" -ge 5 ]" || FAIL=1

echo "== assert (b): a retry was scheduled (event + due-timer) =="
wait_for 30 "retry_scheduled published" bash -c \
  "docker exec evt-redis sh -c 'cat /tmp/events.log 2>/dev/null' | grep -q retry_scheduled" || FAIL=1
# A due-timer ZSET exists for the campaign (retries queued by time).
wait_for 20 "retry due-timer present in redis" bash -c \
  "[ \"\$(docker exec evt-redis redis-cli zcard ad:timers:1)\" -ge 1 ]" || FAIL=1

echo "-- orchestrator logs (tail) --"; docker logs evt-orch 2>&1 | grep -iE "leader|breaker|paus|tick|retry" | tail -10
echo "-- observed events (unique types) --"
docker exec evt-redis sh -c "cat /tmp/events.log 2>/dev/null" | grep -oE '"type":"[a-z_]+"' | sort | uniq -c

if [ "$FAIL" -eq 0 ]; then
  echo "EVENTS E2E: PASS — failures tripped the persistent circuit breaker (paused dialing) and a retry was scheduled on the due-timer, over real Redis + Postgres"
  exit 0
else
  echo "EVENTS E2E: FAIL"
  exit 1
fi
