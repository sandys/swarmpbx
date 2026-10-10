#!/usr/bin/env bash
# Automated e2e: OUR orchestrator is the subject. It wins leadership (pg advisory
# lock), claims the hopper, and originates a call through ad_gateway (LiveKit
# outbound SIP). The call reaches a local SIP UAS that answers and streams
# audio; the agent subscribed in the room must receive it. The hopper row must
# end claimed. Exits non-zero on any failure. Always tears down.
set -uo pipefail
cd "$(dirname "$0")"

COMPOSE="docker compose -f docker-compose.e2e.yml"
LK_ENV="-e LIVEKIT_URL=http://127.0.0.1:7880 -e LIVEKIT_API_KEY=devkey -e LIVEKIT_API_SECRET=devsecret_devsecret_devsecret_32b"
LKCLI() { docker run --rm --network host $LK_ENV -v "$PWD":/w livekit/livekit-cli:latest "$@"; }
PSQL() { docker exec e2e-postgres psql -U ad -d ad -tAc "$1"; }
FAIL=0

cleanup() {
  echo "== teardown =="
  docker rm -f e2e-uas e2e-orch e2e-postgres >/dev/null 2>&1 || true
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
agent_stat() { curl -s --max-time 2 http://127.0.0.1:8088/stats 2>/dev/null \
  | python3 -c "import sys,json;print(json.load(sys.stdin).get('$1',''))" 2>/dev/null; }

echo "== build + start stack =="
# Name the postgres container so we can psql into it.
$COMPOSE up -d --build 2>&1 | tail -3
# Rename the postgres + ensure a stable handle via container_name is simpler:
PG_CID=$($COMPOSE ps -q postgres)
docker rename "$PG_CID" e2e-postgres >/dev/null 2>&1 || true

wait_for 60 "postgres ready" bash -c "docker exec e2e-postgres pg_isready -U ad" || { echo FATAL; exit 1; }
wait_for 60 "livekit healthy" bash -c "LKCLI(){ docker run --rm --network host $LK_ENV livekit/livekit-cli:latest \"\$@\"; }; LKCLI room list" || FAIL=1
wait_for 90 "agent connected to room" bash -c \
  "[ \"\$(curl -s http://127.0.0.1:8088/stats | python3 -c 'import sys,json;print(json.load(sys.stdin)[\"connected\"])')\" = True ]" || FAIL=1

if [ "$FAIL" -ne 0 ]; then echo "STACK NOT HEALTHY"; exit 1; fi

echo "== migrate + seed DB =="
export DATABASE_URL="postgres://ad:ad@127.0.0.1:5432/ad"
# Apply migrations with our own binary (build the image target once).
docker build -q -t agentdialer:e2e .. >/dev/null
migrated=0
for i in $(seq 1 10); do
  if docker run --rm --network host -e DATABASE_URL="$DATABASE_URL" agentdialer:e2e migrate >/tmp/e2e_migrate.log 2>&1; then
    migrated=1; break
  fi
  sleep 2
done
[ "$migrated" = 1 ] || { echo "FAIL: migrate"; cat /tmp/e2e_migrate.log; exit 1; }
echo "ok: migrations applied"

# One running campaign (id 1), one queued lead, one hopper row.
PSQL "INSERT INTO campaigns (id, org_id, name, state, created_at, updated_at)
      VALUES (1, 1, 'e2e', 'running', now(), now())
      ON CONFLICT (id) DO UPDATE SET state='running';" >/dev/null
PSQL "SELECT setval(pg_get_serial_sequence('campaigns','id'), 1, true);" >/dev/null 2>&1 || true
PSQL "INSERT INTO leads (id, campaign_id, phone_number, timezone, status, called_count, context, created_at, updated_at)
      VALUES (1, 1, '+10000000000', 'America/New_York', 'queued', 0, '{}'::jsonb, now(), now())
      ON CONFLICT (id) DO NOTHING;" >/dev/null
PSQL "INSERT INTO hopper (lead_id, campaign_id, priority, claimed_by, claimed_at)
      VALUES (1, 1, 0, NULL, NULL) ON CONFLICT DO NOTHING;" >/dev/null
echo "ok: seeded 1 running campaign, 1 lead, 1 hopper row"

echo "== create LiveKit outbound trunk -> local UAS =="
# The gateway dials this trunk; the trunk points at our sipp UAS on 5062.
TRUNK_JSON=$(LKCLI sip outbound create \
  --name e2e-out --address 127.0.0.1:5080 --transport udp --numbers +10000000000 2>&1)
TRUNK_ID=$(echo "$TRUNK_JSON" | grep -oE "ST_[A-Za-z0-9]+" | head -1)
[ -n "$TRUNK_ID" ] || { echo "FAIL: no outbound trunk id (got: $TRUNK_JSON)"; exit 1; }
echo "ok: outbound trunk $TRUNK_ID"

echo "== start SIP UAS (answers the originated call, streams PCMA) =="
# No -bg: sipp stays in the foreground so the container stays alive. -m 1 means
# it handles one call then exits; -timeout bounds the wait for the call.
docker rm -f e2e-uas >/dev/null 2>&1 || true
docker run -d --name e2e-uas --network host -v "$PWD/uas.xml":/sipp/uas.xml \
  --entrypoint sipp ctaloi/sipp:latest \
  -sf /sipp/uas.xml -p 5080 -mi 127.0.0.1 -mp 7000 -m 1 -timeout 60s >/dev/null 2>&1
wait_for 15 "UAS server listening on 5080" bash -c \
  "docker logs e2e-uas 2>&1 | grep -q 'Sipp Server Mode'" || { echo "FAIL: UAS not up"; docker logs e2e-uas 2>&1 | tail; exit 1; }

echo "== run OUR orchestrator (leader -> claim -> gateway originate) =="
docker run -d --name e2e-orch --network host \
  -e DATABASE_URL="$DATABASE_URL" \
  -e LIVEKIT_URL=ws://127.0.0.1:7880 \
  -e LIVEKIT_API_KEY=devkey -e LIVEKIT_API_SECRET=devsecret_devsecret_devsecret_32b \
  -e AD_SIP_TRUNK_ID="$TRUNK_ID" -e AD_ROOM_PREFIX=campaign \
  -e AD_TICK_MS=1000 -e AD_BATCH_LIMIT=10 -e AD_MAX_CONCURRENT=10 \
  -e RUST_LOG=info \
  agentdialer:e2e orchestrator >/dev/null 2>&1

wait_for 30 "orchestrator became leader" bash -c "docker logs e2e-orch 2>&1 | grep -q 'became leader'" || FAIL=1

echo "== assert: the orchestrator claimed the hopper row =="
wait_for 20 "hopper row claimed by orchestrator" bash -c \
  "[ \"\$(docker exec e2e-postgres psql -U ad -d ad -tAc \"SELECT claimed_by IS NOT NULL FROM hopper WHERE lead_id=1\")\" = t ]" || FAIL=1

echo "== assert: a SIP participant landed in room campaign-1 =="
wait_for 30 "sip participant in room campaign-1" bash -c \
  "docker run --rm --network host $LK_ENV livekit/livekit-cli:latest room participants list campaign-1 2>/dev/null | grep -qi sip_" || FAIL=1

echo "== assert: the agent received caller audio =="
wait_for 25 "agent subscribed to a track" bash -c \
  "[ \"\$(curl -s http://127.0.0.1:8088/stats | python3 -c 'import sys,json;print(json.load(sys.stdin)[\"subscribed_tracks\"])')\" -ge 1 ]" || FAIL=1
wait_for 25 "agent received audio frames" bash -c \
  "[ \"\$(curl -s http://127.0.0.1:8088/stats | python3 -c 'import sys,json;print(json.load(sys.stdin)[\"frames_in\"])')\" -gt 0 ]" || FAIL=1

echo "-- orchestrator logs (tail) --"; docker logs e2e-orch 2>&1 | tail -8
echo "-- agent stats --"; curl -s http://127.0.0.1:8088/stats; echo

if [ "$FAIL" -eq 0 ]; then
  echo "E2E: PASS — orchestrator won leadership, claimed the hopper, and ad_gateway originated a call that landed in the LiveKit room with the agent receiving audio"
  exit 0
else
  echo "E2E: FAIL"
  exit 1
fi
