#!/usr/bin/env bash
# Live Twilio e2e: the quickstart stack (docker-compose.quickstart.yml), driven
# with the same steps as README.md "Call through Twilio", places a real call
# through Twilio Elastic SIP Trunking to Twilio's "Play" test number
# (+16504894546: free, and reachable only through a trunk).
#
#   (a) e2e/twilio/run.sh setup finds or creates the e2e trunk, its
#       credential list, and the caller ID, and writes .env.twilio;
#   (b) the stack starts; a campaign with the demo agent, one lead, and the
#       start transitions go through the API (the Python client of examples/);
#   (c) the node sends the INVITE through Kamailio to Twilio, answers
#       Twilio's 407 with the trunk credentials, and the call connects;
#   (d) Twilio's audio reaches the agent in the room (loud frames), through
#       Docker's NAT and the trunk's Symmetric RTP;
#   (e) the agent leaves after DEMO_AGENT_MAX_CALL_S; the node sends BYE, the
#       attempt completes, and Twilio records the call as completed.
#
# Needs .env (TWILIO_SID, TWILIO_SECRET), a Twilio account where Elastic SIP
# Trunking is available, and a caller ID: a Twilio number of the account or a
# verified caller ID (or TWILIO_CALLER_ID). A Trial account calls verified
# numbers only. TWILIO_E2E_TO overrides the number to call. Exits non-zero on
# any failure.
# Always tears down the stack; the trunk stays for the next run
# (e2e/twilio/run.sh teardown removes it).
#
# --sim runs the same stack and steps with the carrier simulator in Twilio's
# place (e2e/docker-compose.twilio-sim.yml): no credentials, no cost. CI runs
# it on every change. It then runs every recipe of examples/ (README.md,
# "Recipes") against the simulator, as a user runs them:
#   (f) each recipe makes its campaigns through the API, calls, and checks
#       its own result;
#   (g) an agent-first agent leaves the room while a slow number rings; the
#       node hangs up at the answer, and the outcome is abandoned.
# The repair sweep is 10 min (AD_REPAIR_SWEEP_MS), so a step passes only when
# an event or a due time starts its work.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
cd "$here"
export E2E_NET=agentdialer-quickstart_default
export E2E_DATABASE_URL=postgres://ad:ad@postgres:5432/ad
. ./lib.sh

SIM=0
[ "${1:-}" = --sim ] && SIM=1
TO="${TWILIO_E2E_TO:-+16504894546}"
export DEMO_AGENT_MAX_CALL_S="${DEMO_AGENT_MAX_CALL_S:-20}"
if [ "$SIM" = 1 ]; then
  # A repair sweep of 10 min: the recipes pass only when the events and the
  # due times start all the work (outcomes.py and changes.py check the times).
  export AD_REPAIR_SWEEP_MS="${AD_REPAIR_SWEEP_MS:-600000}"
  ENV_FILE="$E2E_OUT/twilio-sim.env"
  cat > "$ENV_FILE" <<'EOF'
AD_SIP_TRUNK=twilio-sim:5080
AD_SIP_TRUNK_USER=agentdialer
AD_SIP_TRUNK_PASSWORD=E2eTrunkPassw0rd
AD_SIP_CALLER_ID=+15550001000
EOF
  CL="docker compose -f $repo/docker-compose.quickstart.yml -f $here/docker-compose.twilio-sim.yml --env-file $ENV_FILE"
  export E2E_SIM_URL=http://twilio-sim:8080/calls
else
  ENV_FILE="$repo/.env.twilio"
  CL="docker compose -f $repo/docker-compose.quickstart.yml --env-file $ENV_FILE"
fi
STATS=http://agent:8088/stats
FAIL=0

cleanup() {
  echo "== teardown =="
  if [ -f "$ENV_FILE" ]; then
    agent_stats_json_of "$STATS" > "$E2E_OUT/run_twilio_e2e.agent.stats.json" 2>/dev/null || true
    [ "$SIM" = 1 ] && { agent_stats_json_of "$E2E_SIM_URL" > "$E2E_OUT/run_twilio_e2e.sim.calls.json" 2>/dev/null || true; }
    for s in node kamailio api livekit agent pipecat-bot $([ "$SIM" = 1 ] && echo twilio-sim); do
      $CL logs "$s" > "$E2E_OUT/run_twilio_e2e.$s.log" 2>&1 || true
    done
    $CL down -v >/dev/null 2>&1 || true
  fi
  echo "== teardown complete (logs in $E2E_OUT/run_twilio_e2e.*.log) =="
}
trap cleanup EXIT

# The value of one key in .env.twilio, without printing the file.
twilio_env() { sed -n "s/^$1=//p" "$repo/.env.twilio"; }

echo "== (a) the Twilio trunk =="
if [ "$SIM" = 1 ]; then
  echo "ok: the carrier simulator stands in for Twilio (twilio-sim:5080)"
else
  "$here/twilio/run.sh" setup || { echo "FAIL: Twilio setup (see above)"; exit 1; }
  [ "$(twilio_env TWILIO_SYMMETRIC_RTP)" = on ] || { echo "FAIL: the trunk's Symmetric RTP is off (see the ACTION above)"; exit 1; }
  [ -n "$(twilio_env AD_SIP_CALLER_ID)" ] || { echo "FAIL: no caller ID: get a Twilio number, or verify a caller ID in the Twilio Console"; exit 1; }
  [ "$(twilio_env TWILIO_ACCOUNT_TYPE)" = Trial ] && echo "note: a Trial account calls verified numbers only; if Twilio refuses $TO, set TWILIO_E2E_TO to a verified number (that phone rings)"
  echo "ok: trunk $(twilio_env AD_SIP_TRUNK)"
fi

echo "== (b) the stack and a campaign through the API =="
$CL up -d --build 2>&1 | tail -1
wait_for 120 "the api answers" http_ok http://api:8080/health || exit 1
wait_for 120 "the demo agent worker registered" http_ok http://agent:8081/ || exit 1
SINCE=$(date -u +%Y-%m-%dT%H:%M:%SZ)
CAMPAIGN=$(api POST /v1/campaigns \
  '{"org_id": 1, "name": "twilio-play", "concurrency_cap": 1, "agent": {"runtime": "livekit", "agent_name": "demo"}}' \
  | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
[ -n "$CAMPAIGN" ] || { echo "FAIL: the api did not create the campaign"; exit 1; }
api POST "/v1/campaigns/$CAMPAIGN/leads" \
  "{\"leads\": [{\"phone_number\": \"$TO\", \"timezone\": \"America/New_York\"}]}" >/dev/null || FAIL=1
api POST "/v1/campaigns/$CAMPAIGN/transition" '{"transition": "start"}' >/dev/null || FAIL=1
api POST "/v1/campaigns/$CAMPAIGN/transition" '{"transition": "sync_complete"}' >/dev/null || FAIL=1
echo "ok: campaign $CAMPAIGN is running with one lead"

echo "== (c) the call through Kamailio to the carrier =="
wait_for 60 "an attempt" pg_ge "SELECT count(*) FROM call_attempts WHERE campaign_id=$CAMPAIGN" 1 || FAIL=1
wait_for 60 "the carrier answered" pg_eq "SELECT answered_at IS NOT NULL FROM call_attempts WHERE campaign_id=$CAMPAIGN" t || {
  FAIL=1
  echo "  attempt: $(pg "SELECT state || ' ' || coalesce(outcome, '') || ' ' || coalesce(snapshot->'leg'->'outcome'->>'detail', '') FROM call_attempts WHERE campaign_id=$CAMPAIGN")"
  [ "$SIM" = 0 ] && [ "$(twilio_env TWILIO_ACCOUNT_TYPE)" = Trial ] && \
    echo "  hint: on a Trial account, Twilio refuses a destination or a caller ID that is not verified (Twilio error 32100)"
}
ROOM=$(pg "SELECT room FROM call_attempts WHERE campaign_id=$CAMPAIGN")

echo "== (d) the carrier's audio reached the agent =="
wait_for 30 "the agent heard the carrier (loud frames)" bash -c \
  ". ./lib.sh; n=\$(room_stat $STATS $ROOM loud_in); [ \"\${n:-0}\" -gt 0 ]" || FAIL=1

echo "== (e) the agent ends the call; Twilio records it =="
wait_for $((DEMO_AGENT_MAX_CALL_S + 40)) "the attempt completed" pg_eq \
  "SELECT outcome FROM call_attempts WHERE campaign_id=$CAMPAIGN" completed || FAIL=1
if [ "$SIM" = 1 ]; then
  wait_for 30 "the simulator saw the 407, our digest, our caller ID, our tone, and our BYE" sim_eq \
    "c['to'] == '$TO' and c['invites'] == 2 and c['auth'] == 'ok' and c['from'] == '+15550001000'
     and c['loud_in'] > 0 and c['status'] == 'completed' and c['ended_by'] == 'caller'" 1 || FAIL=1
  WHAT="the carrier simulator in Twilio's place"
else
  wait_for 60 "Twilio recorded the call as completed" bash -c \
    "'$here/twilio/run.sh' calls '$TO' '$SINCE' | grep -q '\"status\": \"completed\"'" || FAIL=1
  "$here/twilio/run.sh" calls "$TO" "$SINCE"
  WHAT="Twilio Elastic SIP Trunking"
fi

RECIPES=""
if [ "$SIM" = 1 ]; then
  echo "== (f) the recipes of examples/, against the simulator =="
  export QUICKSTART_NUMBER="$TO"
  wait_for 120 "the pipecat bot answers" http_ok http://pipecat-bot:8089/stats || FAIL=1
  for r in $(recipes); do
    run_recipe "$CL" "$r" || { echo "FAIL: the recipe $r"; FAIL=1; }
  done
  RECIPES="; every recipe passed: $(recipes | tr '\n' ' ')"
fi

if [ "$SIM" = 1 ]; then
  echo "== (g) agent first: the agent leaves during a slow dial; the answered call ends =="
  SLOW=+12025550180  # the simulator answers it after 6 s
  C2=$(api POST /v1/campaigns \
    '{"org_id": 1, "name": "agent-leaves", "concurrency_cap": 1, "agent": {"runtime": "livekit", "agent_name": "demo", "dial_order": "agent_first"}}' \
    | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  api POST "/v1/campaigns/$C2/leads" \
    "{\"leads\": [{\"phone_number\": \"$SLOW\", \"timezone\": \"America/New_York\", \"context\": {\"leave_after_s\": \"2\"}}]}" \
    >"$E2E_OUT/agent-leaves.lead.json" 2>&1 || { FAIL=1; echo "FAIL: the lead: $(cat "$E2E_OUT/agent-leaves.lead.json")"; }
  api POST "/v1/campaigns/$C2/transition" '{"transition": "start"}' >/dev/null || FAIL=1
  api POST "/v1/campaigns/$C2/transition" '{"transition": "sync_complete"}' >/dev/null || FAIL=1
  wait_for 40 "the answered call without its agent ended as abandoned" pg_eq \
    "SELECT outcome || ' / ' || (snapshot->'leg'->'outcome'->>'detail') FROM call_attempts WHERE campaign_id=$C2" \
    "abandoned / the agent left the room before the answer" || FAIL=1
  wait_for 30 "the simulator answered the slow call, and we sent the BYE" sim_eq \
    "c['to'] == '$SLOW' and c['status'] == 'completed' and c['ended_by'] == 'caller'" 1 || FAIL=1
  RECIPES="$RECIPES; an agent-first call whose agent left during the dial ended at the answer"
fi
if [ "$FAIL" -eq 0 ]; then
  echo "TWILIO E2E: PASS — the quickstart stack placed a call through $WHAT to $TO: digest auth, the caller ID, two-way audio, and a clean end$RECIPES"
  exit 0
else
  echo "TWILIO E2E: FAIL"
  exit 1
fi
