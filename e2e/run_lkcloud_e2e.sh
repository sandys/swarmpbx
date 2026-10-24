#!/usr/bin/env bash
# The README quickstart ("Quickstart: place calls through LiveKit Cloud SIP")
# as a live test: the quickstart stack (docker-compose.quickstart.yml with
# docker-compose.livekit-cloud.yml) places a real call over the internet to
# the project's LiveKit phone number, at the project's SIP URI.
#
#   (a) e2e/livekit-cloud/run.sh setup rents or reuses the number, and finds
#       or creates the inbound trunk (digest auth), the dispatch rule (a room
#       per call, with the callee agent; the number routes to it too), and
#       the outbound trunk, and writes .env.lkcloud;
#   (b) the stack and the callee agent start; a campaign with the demo agent,
#       one lead, and the start transitions go through the API;
#   (c) the node sends the INVITE through Kamailio to LiveKit Cloud, answers
#       its 407 with the trunk credentials, and the call connects;
#   (d) each agent hears the other's tone: the demo agent here (through the
#       node and our local LiveKit) and the callee agent in LiveKit Cloud;
#   (e) the demo agent leaves after DEMO_AGENT_MAX_CALL_S; the node sends BYE,
#       the attempt completes, and LiveKit Cloud ends the callee's room;
#   (f) for a reference, LiveKit's own SIP client calls the same number
#       through the LiveKit outbound trunk (run.sh call): the callee agent
#       joins both rooms of that call, and each side hears the other's tone.
#
# Needs .env with LIVEKIT_URL, LIVEKIT_API_KEY, LIVEKIT_API_SECRET, and
# LIVEKIT_SIP_URI (LiveKit Cloud, Project Settings). Exits non-zero on any
# failure. Always tears down the stack; the trunk and the dispatch rule stay
# for the next run (e2e/livekit-cloud/run.sh teardown removes them).
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
cd "$here"
export E2E_NET=agentdialer-quickstart_default
export E2E_DATABASE_URL=postgres://ad:ad@postgres:5432/ad
. ./lib.sh

export DEMO_AGENT_MAX_CALL_S="${DEMO_AGENT_MAX_CALL_S:-20}"
ENV_FILE="$repo/.env.lkcloud"
CL="docker compose -f $repo/docker-compose.quickstart.yml -f $repo/docker-compose.livekit-cloud.yml --env-file $repo/.env --env-file $ENV_FILE"
API="$repo/scripts/api.sh"
STATS=http://agent:8088/stats
CALLEE_STATS=http://callee:8088/stats
FAIL=0

cleanup() {
  echo "== teardown =="
  if [ -f "$ENV_FILE" ]; then
    agent_stats_json_of "$STATS" > "$E2E_OUT/run_lkcloud_e2e.agent.stats.json" 2>/dev/null || true
    agent_stats_json_of "$CALLEE_STATS" > "$E2E_OUT/run_lkcloud_e2e.callee.stats.json" 2>/dev/null || true
    for s in node kamailio api livekit agent callee; do
      $CL logs "$s" > "$E2E_OUT/run_lkcloud_e2e.$s.log" 2>&1 || true
    done
    $CL down -v >/dev/null 2>&1 || true
  fi
  echo "== teardown complete (logs in $E2E_OUT/run_lkcloud_e2e.*.log) =="
}
trap cleanup EXIT

lkcloud_env() { sed -n "s/^$1=//p" "$ENV_FILE"; }

echo "== (a) the LiveKit Cloud trunk and dispatch rule =="
"$here/livekit-cloud/run.sh" setup || { echo "FAIL: LiveKit Cloud setup (see above)"; exit 1; }
TO="$(lkcloud_env LKCLOUD_TO)"
echo "ok: trunk sip:$(lkcloud_env AD_SIP_TRUNK)"

echo "== (b) the stack, the callee agent, and a campaign through the API =="
$CL up -d --build 2>&1 | tail -1
wait_for 120 "the api answers" http_ok http://api:8080/health || exit 1
wait_for 120 "the demo agent worker registered" http_ok http://agent:8081/ || exit 1
wait_for 120 "the callee agent registered with LiveKit Cloud" http_ok http://callee:8081/ || exit 1
CAMPAIGN=$("$API" POST /v1/campaigns \
  '{"org_id": 1, "name": "livekit-cloud", "concurrency_cap": 1, "agent": {"runtime": "livekit", "agent_name": "demo"}}' \
  | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
[ -n "$CAMPAIGN" ] || { echo "FAIL: the api did not create the campaign"; exit 1; }
"$API" POST "/v1/campaigns/$CAMPAIGN/leads" \
  "{\"leads\": [{\"phone_number\": \"$TO\", \"timezone\": \"America/New_York\"}]}" >/dev/null || FAIL=1
"$API" POST "/v1/campaigns/$CAMPAIGN/transition" '{"transition": "start"}' >/dev/null || FAIL=1
"$API" POST "/v1/campaigns/$CAMPAIGN/transition" '{"transition": "sync_complete"}' >/dev/null || FAIL=1
echo "ok: campaign $CAMPAIGN is running with one lead"

echo "== (c) the call through Kamailio to LiveKit Cloud =="
wait_for 60 "an attempt" pg_ge "SELECT count(*) FROM call_attempts WHERE campaign_id=$CAMPAIGN" 1 || FAIL=1
wait_for 60 "LiveKit Cloud answered" pg_eq "SELECT answered_at IS NOT NULL FROM call_attempts WHERE campaign_id=$CAMPAIGN" t || {
  FAIL=1
  echo "  attempt: $(pg "SELECT state || ' ' || coalesce(outcome, '') || ' ' || coalesce(snapshot->'leg'->'outcome'->>'detail', '') FROM call_attempts WHERE campaign_id=$CAMPAIGN")"
}
ROOM=$(pg "SELECT room FROM call_attempts WHERE campaign_id=$CAMPAIGN")

echo "== (d) the audio crossed both ways =="
wait_for 30 "the callee agent in LiveKit Cloud heard our tone" rooms_ge "$CALLEE_STATS" "r['loud_in'] > 0" 1 || FAIL=1
wait_for 30 "the demo agent heard the callee's tone" bash -c \
  ". ./lib.sh; n=\$(room_stat $STATS $ROOM loud_in); [ \"\${n:-0}\" -gt 0 ]" || {
  FAIL=1
  echo "  hint: LiveKit Cloud sends RTP to the private address in our SDP unless it latches (symmetric RTP)"
}

echo "== (e) the demo agent ends the call; LiveKit Cloud ends the callee's room =="
wait_for $((DEMO_AGENT_MAX_CALL_S + 40)) "the attempt completed" pg_eq \
  "SELECT outcome FROM call_attempts WHERE campaign_id=$CAMPAIGN" completed || FAIL=1
wait_for 30 "our BYE reached LiveKit Cloud (the callee left the room)" rooms_ge "$CALLEE_STATS" "r['ended']" 1 || FAIL=1

echo "  LiveKit routed our call as: $(agent_stats_json_of "$CALLEE_STATS" | docker run --rm -i "$E2E_PY_IMAGE" python3 -c '
import json, sys
rooms = json.load(sys.stdin)["rooms"]
print(json.dumps([r.get("sip") for r in rooms.values()]))' 2>/dev/null)"

echo "== (f) reference: LiveKit's SIP client calls $TO through the outbound trunk =="
"$here/livekit-cloud/run.sh" call 15 > "$E2E_OUT/run_lkcloud_e2e.lkcall.json" 2>&1 &
LKCALL=$!
wait_for 60 "LiveKit's outbound call answered, and the callee agent joined both rooms" rooms_ge "$CALLEE_STATS" "r['frames_out'] > 0" 3 || FAIL=1
wait_for 30 "each side of LiveKit's call heard the other's tone" rooms_ge "$CALLEE_STATS" "r['loud_in'] > 0" 3 || FAIL=1
wait $LKCALL || { FAIL=1; echo "  LiveKit's call: $(cat "$E2E_OUT/run_lkcloud_e2e.lkcall.json")"; }
wait_for 30 "LiveKit's call ended in both rooms" rooms_ge "$CALLEE_STATS" "r['ended']" 3 || FAIL=1

if [ "$FAIL" -eq 0 ]; then
  echo "LKCLOUD E2E: PASS — the quickstart stack called the LiveKit number $TO over the internet: digest auth, two-way audio through NAT, and a clean end; LiveKit's own outbound trunk reached the same number"
  exit 0
else
  echo "LKCLOUD E2E: FAIL"
  exit 1
fi
