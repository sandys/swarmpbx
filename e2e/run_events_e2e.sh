#!/usr/bin/env bash
# Event-driven e2e for the Postgres event layer (PLAN.md §0.10.8a, decision
# D-3). Subject: the orchestrator's event loop (event table + LISTEN/NOTIFY),
# the circuit breaker over call_outcomes, and retries on the lead row.
#
# We do NOT need the media stack here (origination is proven by run_e2e.sh).
# Instead we point the gateway at an unreachable LiveKit, so every call fails.
# That lets us prove, deterministically, against a REAL Postgres:
#   (a) failures are recorded in call_outcomes and the breaker TRIPS
#       (a circuit_breaker_tripped event row is written);
#   (b) a failed call stores its retry on the lead (next_eligible_at,
#       retry_reason) and writes a retry_scheduled event row;
#   (c) a lead whose retry becomes due after startup is re-admitted and dialed
#       again. The event sweep is 60 s, so a re-dial inside the deadline proves
#       that NOTIFY woke the consumer.
#
# Exits non-zero on any failure. Always tears down.
set -uo pipefail
cd "$(dirname "$0")"
. ./lib.sh

COMPOSE="docker compose -f docker-compose.events.e2e.yml"
PSQL() { docker exec evt-postgres psql -U ad -d ad -tAc "$1"; }
FAIL=0

cleanup() {
  echo "== teardown =="
  docker logs evt-orch > "$E2E_OUT/run_events_e2e.orchestrator.log" 2>&1 || true
  docker rm -f evt-orch evt-postgres >/dev/null 2>&1 || true
  $COMPOSE down >/dev/null 2>&1 || true
  echo "== teardown complete (orchestrator log: $E2E_OUT/run_events_e2e.orchestrator.log) =="
}
trap cleanup EXIT

echo "== start postgres =="
$COMPOSE up -d 2>&1 | tail -2
docker rename "$($COMPOSE ps -q postgres)" evt-postgres >/dev/null 2>&1 || true
wait_for 60 "postgres ready" bash -c "docker exec evt-postgres pg_isready -U ad" || exit 1

echo "== build image + migrate + seed =="
export DATABASE_URL="postgres://ad:ad@127.0.0.1:5432/ad"
docker build -q -t agentdialer:e2e .. >/dev/null
migrated=0
for i in $(seq 1 10); do
  docker run --rm --network host -e DATABASE_URL="$DATABASE_URL" agentdialer:e2e migrate >"$E2E_OUT/run_events_e2e.migrate.log" 2>&1 && { migrated=1; break; }
  sleep 2
done
[ "$migrated" = 1 ] || { echo "FAIL: migrate"; cat "$E2E_OUT/run_events_e2e.migrate.log"; exit 1; }

# Campaign 1: 6 fresh leads + 6 hopper rows. Six failures exceed the breaker's
# min_calls (5) and 50% threshold, so it must trip.
PSQL "INSERT INTO campaigns (id, org_id, name, state, created_at, updated_at)
      VALUES (1,1,'evt','running',now(),now()) ON CONFLICT (id) DO UPDATE SET state='running';" >/dev/null
for n in 1 2 3 4 5 6; do
  PSQL "INSERT INTO leads (id,campaign_id,phone_number,timezone,status,called_count,context,created_at,updated_at)
        VALUES ($n,1,'+1000000000$n','America/New_York','queued',0,'{}'::jsonb,now(),now())
        ON CONFLICT (id) DO NOTHING;" >/dev/null
  PSQL "INSERT INTO hopper (lead_id,campaign_id,priority,claimed_by,claimed_at)
        VALUES ($n,1,0,NULL,NULL) ON CONFLICT DO NOTHING;" >/dev/null
done
# Campaign 2: one lead that was dialed once. Its retry is due in one hour, so
# the startup reconcile cannot dial it. Its hopper row is still claimed by the
# old attempt, so only the retry sweep can make it claimable again.
PSQL "INSERT INTO campaigns (id, org_id, name, state, created_at, updated_at)
      VALUES (2,1,'evt-retry','running',now(),now()) ON CONFLICT (id) DO UPDATE SET state='running';" >/dev/null
PSQL "INSERT INTO leads (id,campaign_id,phone_number,timezone,status,called_count,context,
                         next_eligible_at,retry_reason,created_at,updated_at)
      VALUES (7,2,'+10000000007','America/New_York','queued',1,'{}'::jsonb,
              now() + interval '1 hour','no_answer',now(),now())
      ON CONFLICT (id) DO NOTHING;" >/dev/null
PSQL "INSERT INTO hopper (lead_id,campaign_id,priority,claimed_by,claimed_at)
      VALUES (7,2,0,'old-attempt',now() - interval '30 minutes') ON CONFLICT DO NOTHING;" >/dev/null
echo "ok: seeded campaign 1 (6 leads) and campaign 2 (1 retry, due later)"

echo "== run orchestrator with a FAILING gateway (unreachable LiveKit, bad trunk) =="
# The env set must match the orchestrator contract (crates/ad_bin/src/config.rs).
docker run -d --name evt-orch --network host \
  -e DATABASE_URL="$DATABASE_URL" \
  -e LIVEKIT_URL="ws://127.0.0.1:9" \
  -e LIVEKIT_API_KEY=devkey -e LIVEKIT_API_SECRET=devsecret_devsecret_devsecret_32b \
  -e AD_SIP_TRUNK_ID="ST_nonexistent" -e AD_ROOM_PREFIX=campaign \
  -e AD_CALLS_PER_SEC=50 -e AD_RETRY_SWEEP_MS=500 -e AD_EVENT_SWEEP_MS=60000 \
  -e AD_BATCH_LIMIT=10 \
  -e RUST_LOG=info \
  agentdialer:e2e orchestrator >/dev/null 2>&1

wait_for 30 "orchestrator became leader" bash -c "docker logs evt-orch 2>&1 | grep -q 'became leader'" || FAIL=1

echo "== assert (a): outcomes recorded as failures in Postgres =="
wait_for 40 ">=5 failed call_outcomes recorded" bash -c \
  "[ \"\$(docker exec evt-postgres psql -U ad -d ad -tAc \"SELECT count(*) FROM call_outcomes WHERE campaign_id=1 AND outcome='failed'\")\" -ge 5 ]" || FAIL=1

echo "== assert (a): the circuit breaker tripped (event row) =="
wait_for 40 "circuit_breaker_tripped written" bash -c \
  "[ \"\$(docker exec evt-postgres psql -U ad -d ad -tAc \"SELECT count(*) FROM campaign_events WHERE campaign_id=1 AND kind='circuit_breaker_tripped'\")\" -ge 1 ]" || FAIL=1

echo "== assert (b): the retry is stored on the lead and announced =="
wait_for 30 "retry_scheduled written" bash -c \
  "[ \"\$(docker exec evt-postgres psql -U ad -d ad -tAc \"SELECT count(*) FROM campaign_events WHERE campaign_id=1 AND kind='retry_scheduled'\")\" -ge 1 ]" || FAIL=1
wait_for 20 "a failed lead waits for its retry" bash -c \
  "[ \"\$(docker exec evt-postgres psql -U ad -d ad -tAc \"SELECT count(*) FROM leads WHERE campaign_id=1 AND status='queued' AND retry_reason='failed' AND next_eligible_at > now()\")\" -ge 1 ]" || FAIL=1

echo "== assert (c): a retry that becomes due later is re-admitted and dialed again =="
# The startup reconcile is done with campaign 2 when it reports no work.
wait_for 30 "reconcile found no work in campaign 2" bash -c \
  "[ \"\$(docker exec evt-postgres psql -U ad -d ad -tAc \"SELECT count(*) FROM campaign_events WHERE campaign_id=2 AND kind='campaign_completed'\")\" -ge 1 ]" || FAIL=1
[ "$(PSQL "SELECT called_count FROM leads WHERE id=7")" = 1 ] || { echo "FAIL: lead 7 was dialed before its retry was due"; FAIL=1; }
# Make the retry due now. The sweep re-admits the lead and publishes a kick.
# The event sweep is 60 s, so a dial inside 30 s needs the NOTIFY wake-up.
PSQL "UPDATE leads SET next_eligible_at = now() WHERE id=7;" >/dev/null
wait_for 30 "lead 7 dialed a second time" bash -c \
  "[ \"\$(docker exec evt-postgres psql -U ad -d ad -tAc \"SELECT called_count FROM leads WHERE id=7\")\" -ge 2 ]" || FAIL=1
wait_for 10 "re-admission logged" bash -c \
  "docker logs evt-orch 2>&1 | grep -q 'retries due; leads re-admitted'" || FAIL=1

echo "-- orchestrator logs (tail) --"; docker logs evt-orch 2>&1 | grep -iE "leader|breaker|paus|retr" | tail -10
echo "-- event rows by kind --"
PSQL "SELECT campaign_id, kind, count(*) FROM campaign_events GROUP BY 1,2 ORDER BY 1,2;"

if [ "$FAIL" -eq 0 ]; then
  echo "EVENTS E2E: PASS — failures tripped the circuit breaker, retries were stored on the lead, and a due retry was re-admitted and re-dialed through LISTEN/NOTIFY, over a real Postgres"
  exit 0
else
  echo "EVENTS E2E: FAIL"
  exit 1
fi
