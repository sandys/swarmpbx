#!/usr/bin/env bash
# Click-to-call through Twilio (README.md, "Click-to-call through Twilio"):
# the quickstart stack with AD_DIAL_PROVIDER=twilio_click_to_call. Twilio
# calls the phone; when the phone answers, Twilio's <Dial><Sip> sends an
# INVITE to Kamailio over TCP, Kamailio routes it to the node by x-ad-node,
# and the node answers the INVITE that carries its token.
#
# --sim: the carrier simulator is Twilio (the Calls API and the phones). No
# credentials, no cost; CI runs it on every change.
#   (a) two campaigns go through the API: two leads that answer, and a busy
#       lead with a lead that does not answer;
#   (b) the node asks the Calls API to call each phone, with <Dial><Sip> to
#       Kamailio over TCP and the call's token;
#   (c) the phones answer, and the INVITEs reach the node through Kamailio;
#   (d) each side hears the other (the phone echoes the agent's tone);
#   (e) one phone hangs up (its BYE reaches the node), and on the other call
#       the agent leaves (our BYE reaches the phone);
#   (f) busy and no answer come from the call's status at the Calls API.
#
# Live (no flag): real Twilio, through an ngrok TCP tunnel to Kamailio
# (e2e/docker-compose.ngrok.yml), to TWILIO_C2C_TO (default: Twilio's free
# "Play" number +16504894546). Needs .env with TWILIO_SID, TWILIO_SECRET, and
# NGROK_AUTHTOKEN, and a caller ID at Twilio (e2e/twilio/run.sh setup-c2c).
# Needs an upgraded Twilio account: a Trial account blocks <Dial><Sip>.
#   (a) the tunnel, setup-c2c, the stack, and a campaign with one lead;
#   (b) Twilio calls the phone, then the node through the tunnel;
#   (c) the agent hears the phone; the agent leaves, and Twilio records the
#       call as completed.
#
# Exits non-zero on any failure. Always tears down the stack.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
cd "$here"
export E2E_NET=agentdialer-quickstart_default
export E2E_DATABASE_URL=postgres://ad:ad@postgres:5432/ad
. ./lib.sh

SIM=0
[ "${1:-}" = --sim ] && SIM=1
# --sim: the agent stays long enough for step (e) to hang up one call first.
export DEMO_AGENT_MAX_CALL_S="${DEMO_AGENT_MAX_CALL_S:-$([ "$SIM" = 1 ] && echo 45 || echo 20)}"
QS="$repo/docker-compose.quickstart.yml"
API="$repo/scripts/api.sh"
STATS=http://agent:8088/stats
export E2E_SIM_URL=http://twilio-sim:8080/calls
TWILIO_CALLS=http://twilio-sim:8080/twilio/calls
FAIL=0
if [ "$SIM" = 1 ]; then
  ENV_FILE="$E2E_OUT/twilio-c2c-sim.env"
  cat > "$ENV_FILE" <<'EOF'
AD_DIAL_PROVIDER=twilio_click_to_call
AD_TWILIO_API_URL=http://twilio-sim:8080
AD_TWILIO_ACCOUNT_SID=ACsimulator
AD_TWILIO_API_KEY=SKsimulator
AD_TWILIO_API_SECRET=SimulatorSecret
AD_TWILIO_SIP_URI=sip:agentdialer@kamailio:5060;transport=tcp
AD_SIP_CALLER_ID=+15550001000
EOF
  CL="docker compose -f $QS -f $here/docker-compose.twilio-sim.yml --env-file $ENV_FILE"
  SERVICES="node kamailio api livekit agent twilio-sim"
else
  TO="${TWILIO_C2C_TO:-+16504894546}"
  ENV_FILE="$E2E_OUT/twilio-c2c-live.env"
  BASE="docker compose -f $QS -f $here/docker-compose.ngrok.yml --env-file $repo/.env"
  CL="$BASE --env-file $ENV_FILE"
  SERVICES="node kamailio api livekit agent ngrok"
fi

cleanup() {
  echo "== teardown =="
  agent_stats_json_of "$STATS" > "$E2E_OUT/run_twilio_c2c_e2e.agent.stats.json" 2>/dev/null || true
  if [ "$SIM" = 1 ]; then
    agent_stats_json_of "$E2E_SIM_URL" > "$E2E_OUT/run_twilio_c2c_e2e.sim.calls.json" 2>/dev/null || true
    agent_stats_json_of "$TWILIO_CALLS" > "$E2E_OUT/run_twilio_c2c_e2e.twilio.calls.json" 2>/dev/null || true
  else
    [ -f "$ENV_FILE" ] || CL=$BASE
  fi
  for s in $SERVICES; do
    $CL logs "$s" > "$E2E_OUT/run_twilio_c2c_e2e.$s.log" 2>&1 || true
  done
  $CL down -v >/dev/null 2>&1 || true
  [ "$SIM" = 1 ] || rm -f "$ENV_FILE"
  echo "== teardown complete (logs in $E2E_OUT/run_twilio_c2c_e2e.*.log) =="
}
trap cleanup EXIT

# twilio_eq <expr over c> <n>: exactly n calls at the simulator's Calls API match.
twilio_eq() { E2E_SIM_URL=$TWILIO_CALLS sim_eq "$1" "$2"; }
# The simulator's Calls API SID of the call to a number.
twilio_sid() {
  agent_stats_json_of "$TWILIO_CALLS" | docker run --rm -i "$E2E_PY_IMAGE" python3 -c '
import json, sys
print(next(c["sid"] for c in json.load(sys.stdin) if c["to"] == sys.argv[1]))' "$1" 2>/dev/null
}
# The outcome of the attempt to a number, with its detail.
attempt_of() {
  pg "SELECT a.outcome || ' ' || coalesce(a.snapshot->'leg'->'outcome'->>'detail', '')
      FROM call_attempts a JOIN leads l ON l.id = a.lead_id WHERE l.phone_number = '$1'"
}
outcome_is() { [ "$(pg "SELECT a.outcome FROM call_attempts a JOIN leads l ON l.id = a.lead_id WHERE l.phone_number = '$1'")" = "$2" ]; }
# campaign <name> <cap> <number>...: a running campaign with the demo agent.
campaign() {
  local name=$1 cap=$2; shift 2
  local id leads="" n
  id=$("$API" POST /v1/campaigns \
    "{\"org_id\": 1, \"name\": \"$name\", \"concurrency_cap\": $cap, \"agent\": {\"runtime\": \"livekit\", \"agent_name\": \"demo\"}}" \
    | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  [ -n "$id" ] || return 1
  for n in "$@"; do leads="$leads${leads:+, }{\"phone_number\": \"$n\", \"timezone\": \"America/New_York\"}"; done
  "$API" POST "/v1/campaigns/$id/leads" "{\"leads\": [$leads]}" >/dev/null || return 1
  "$API" POST "/v1/campaigns/$id/transition" '{"transition": "start"}' >/dev/null || return 1
  "$API" POST "/v1/campaigns/$id/transition" '{"transition": "sync_complete"}' >/dev/null || return 1
  echo "$id"
}
# The agent heard the other side in every room of a campaign.
agent_heard_all() {
  local room
  for room in $(pg "SELECT room FROM call_attempts WHERE campaign_id=$1"); do
    wait_for 30 "the agent heard the phone in $room" bash -c \
      ". ./lib.sh; n=\$(room_stat $STATS $room loud_in); [ \"\${n:-0}\" -gt 0 ]" || return 1
  done
}
# The public host:port of the ngrok TCP tunnel.
ngrok_addr() {
  agent_stats_json_of http://ngrok:4040/api/tunnels | docker run --rm -i "$E2E_PY_IMAGE" python3 -c '
import json, sys
for t in json.load(sys.stdin)["tunnels"]:
    if t["public_url"].startswith("tcp://"):
        print(t["public_url"][len("tcp://"):])
        break' 2>/dev/null
}
ngrok_up() { [ -n "$(ngrok_addr)" ]; }

run_sim() {
  local answer_a=+12025550101 answer_b=+12025550102 busy=+12025550486 noanswer=+12025550408 answers fails sid_a
  echo "== (a) the stack, with click-to-call, and the campaigns through the API =="
  $CL up -d --build 2>&1 | tail -1
  wait_for 120 "the api answers" http_ok http://api:8080/health || exit 1
  wait_for 120 "the demo agent worker registered" http_ok http://agent:8081/ || exit 1
  wait_for 60 "the Calls API answers" http_ok http://twilio-sim:8080/healthz || exit 1
  answers=$(campaign c2c-answers 2 "$answer_a" "$answer_b") || { echo "FAIL: the api did not create the campaign"; exit 1; }
  fails=$(campaign c2c-fails 2 "$busy" "$noanswer") || { echo "FAIL: the api did not create the campaign"; exit 1; }
  echo "ok: campaigns $answers (two answers) and $fails (busy, no answer) are running"

  echo "== (b) the node asks the Calls API to call each phone =="
  wait_for 60 "four calls at the Calls API, from our caller ID, with <Dial><Sip> to Kamailio over TCP and a token" twilio_eq \
    "c['from'] == '+15550001000' and c['sip_uri'].startswith('sip:agentdialer@kamailio:5060;transport=tcp?x-ad-call=')
     and '&x-ad-node=' in c['sip_uri']" 4 || FAIL=1

  echo "== (c) the phones answer; the INVITEs reach the node through Kamailio =="
  wait_for 60 "both answering leads connected" pg_eq \
    "SELECT count(*) FROM call_attempts WHERE campaign_id=$answers AND answered_at IS NOT NULL" 2 || {
    FAIL=1; echo "  attempts: $(attempt_of "$answer_a"); $(attempt_of "$answer_b")"; }
  wait_for 30 "the Calls API bridged both calls (200 to its INVITE)" twilio_eq \
    "c['to'] in ('$answer_a', '$answer_b') and c.get('dial_code') == 200 and c['status'] == 'in-progress'" 2 || FAIL=1

  echo "== (d) the audio crossed both ways =="
  wait_for 30 "each phone heard the agent's tone" sim_eq \
    "c['to'] in ('$answer_a', '$answer_b') and c['loud_in'] > 0" 2 || FAIL=1
  agent_heard_all "$answers" || FAIL=1

  echo "== (e) one phone hangs up; on the other call the agent leaves =="
  sid_a=$(twilio_sid "$answer_a")
  docker run --rm --network "$E2E_NET" curlimages/curl:8.11.1 -sS --fail-with-body -u SKsimulator:SimulatorSecret \
    -X POST --data Status=completed "http://twilio-sim:8080/2010-04-01/Accounts/ACsimulator/Calls/$sid_a.json" >/dev/null || FAIL=1
  wait_for 30 "the phone's BYE ended its attempt (the callee hung up)" bash -c \
    ". ./lib.sh; [ \"\$(pg \"SELECT a.outcome || ' ' || (a.snapshot->'leg'->'outcome'->>'detail') FROM call_attempts a JOIN leads l ON l.id = a.lead_id WHERE l.phone_number = '$answer_a'\")\" = 'completed the callee hung up' ]" || {
    FAIL=1; echo "  attempt: $(attempt_of "$answer_a")"; }
  wait_for $((DEMO_AGENT_MAX_CALL_S + 40)) "the agent left the other call, and it completed" \
    outcome_is "$answer_b" completed || FAIL=1
  wait_for 30 "our BYE reached the other phone; the first phone hung up itself" twilio_eq \
    "(c['to'] == '$answer_b' and c['status'] == 'completed' and c['ended_by'] == 'sip')
     or (c['to'] == '$answer_a' and c['status'] == 'completed' and c['ended_by'] == 'api')" 2 || FAIL=1

  echo "== (f) busy and no answer, from the Calls API =="
  wait_for 30 "the busy lead is busy" outcome_is "$busy" busy || { FAIL=1; echo "  attempt: $(attempt_of "$busy")"; }
  wait_for 60 "the lead that does not answer is no_answer" outcome_is "$noanswer" no_answer || {
    FAIL=1; echo "  attempt: $(attempt_of "$noanswer")"; }
  RESULT="the simulator as Twilio called each phone and then the node: the INVITE reached the node through Kamailio over TCP, the audio crossed both ways, each side's BYE ended its call, and busy and no answer became their outcomes"
}

run_live() {
  local addr host port since id
  echo "== (a) the ngrok tunnel, the Twilio account, the stack, and a campaign =="
  grep -q '^NGROK_AUTHTOKEN=.' "$repo/.env" || { echo "FAIL: add NGROK_AUTHTOKEN to .env (ngrok dashboard, Your Authtoken)"; exit 1; }
  $BASE up -d ngrok 2>&1 | tail -1
  wait_for 60 "the ngrok TCP tunnel is up" ngrok_up || {
    echo "  hint: an ngrok TCP endpoint needs a payment method on the ngrok account"
    $BASE logs ngrok 2>&1 | tail -5; exit 1; }
  addr=$(ngrok_addr); host=${addr%:*}; port=${addr##*:}
  echo "ok: the tunnel is tcp://$addr"
  "$here/twilio/run.sh" setup-c2c || { echo "FAIL: Twilio setup (see above)"; exit 1; }
  ( umask 077
    sed -n 's/^\(AD_[A-Z_]*=.*\)$/\1/p' "$repo/.env.twilio-c2c"
    echo "AD_DIAL_PROVIDER=twilio_click_to_call"
    echo "AD_TWILIO_SIP_URI=sip:agentdialer@$host:$port;transport=tcp"
    echo "AD_EDGE_TCP_HOST=$host"
    echo "AD_EDGE_TCP_PORT=$port" ) > "$ENV_FILE"
  echo "ok: Twilio reaches Kamailio at sip:agentdialer@$host:$port;transport=tcp"
  $CL up -d --build 2>&1 | tail -1
  wait_for 120 "the api answers" http_ok http://api:8080/health || exit 1
  wait_for 120 "the demo agent worker registered" http_ok http://agent:8081/ || exit 1
  since=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  id=$(campaign c2c-live 1 "$TO") || { echo "FAIL: the api did not create the campaign"; exit 1; }
  echo "ok: campaign $id is running with one lead ($TO)"

  echo "== (b) Twilio calls the phone, then the node through the tunnel =="
  wait_for 60 "an attempt" pg_ge "SELECT count(*) FROM call_attempts WHERE campaign_id=$id" 1 || FAIL=1
  wait_for 90 "the phone answered, and Twilio's INVITE reached the node" pg_eq \
    "SELECT answered_at IS NOT NULL FROM call_attempts WHERE campaign_id=$id" t || {
    FAIL=1; echo "  attempt: $(attempt_of "$TO")"; return; }

  echo "== (c) the audio, and the end =="
  agent_heard_all "$id" || {
    FAIL=1; echo "  hint: Twilio sends RTP to the private address in our SDP unless it latches to our NAT address"; }
  wait_for $((DEMO_AGENT_MAX_CALL_S + 40)) "the agent left, and the attempt completed" outcome_is "$TO" completed || FAIL=1
  wait_for 60 "Twilio recorded the call as completed" bash -c \
    "'$here/twilio/run.sh' calls '$TO' '$since' | grep -q '\"status\": \"completed\"'" || FAIL=1
  "$here/twilio/run.sh" calls "$TO" "$since"
  RESULT="Twilio called $TO and then the node through the ngrok tunnel: the INVITE reached the node, the agent heard the phone, and the call ended cleanly"
}

if [ "$SIM" = 1 ]; then run_sim; else run_live; fi
if [ "$FAIL" -eq 0 ]; then
  echo "TWILIO C2C E2E: PASS — $RESULT"
  exit 0
else
  echo "TWILIO C2C E2E: FAIL"
  exit 1
fi
