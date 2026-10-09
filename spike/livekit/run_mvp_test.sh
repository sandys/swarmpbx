#!/usr/bin/env bash
# Automated MVP e2e test: a REAL LiveKit agent is wired into a room as a
# subscriber FIRST, then a SIP call is driven into it. Asserts the agent wiring
# with real conditions and exits non-zero on any failure. Always tears down.
#
# Order (as required): agent wiring is the subject; SIP is the stimulus applied
# AFTER wiring is confirmed up.
set -uo pipefail
cd "$(dirname "$0")"

COMPOSE="docker compose -f docker-compose.mvp.yml"
LKCLI="docker run --rm --network host \
  -e LIVEKIT_URL=ws://127.0.0.1:7880 \
  -e LIVEKIT_API_KEY=devkey \
  -e LIVEKIT_API_SECRET=devsecret_devsecret_devsecret_32b \
  livekit/livekit-cli:latest"
FAIL=0

cleanup() {
  echo "== teardown =="
  docker rm -f mvp-sipp >/dev/null 2>&1 || true
  $COMPOSE down >/dev/null 2>&1 || true
  echo "== teardown complete =="
}
trap cleanup EXIT

# Poll a command until it succeeds or a deadline passes. No fixed sleeps as
# assertions — we poll a real condition with a deadline.
wait_for() {  # wait_for <timeout_s> <description> <cmd...>
  local timeout=$1 desc=$2; shift 2
  local deadline=$(( $(date +%s) + timeout ))
  until "$@" >/dev/null 2>&1; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
      echo "FAIL: timed out waiting for: $desc"
      return 1
    fi
    sleep 1
  done
  echo "ok: $desc"
}

agent_stat() {  # agent_stat <json-key>
  curl -s --max-time 2 http://127.0.0.1:8088/stats 2>/dev/null \
    | python3 -c "import sys,json; print(json.load(sys.stdin).get('$1',''))" 2>/dev/null
}

echo "== build + start stack (agent wired first) =="
$COMPOSE up -d --build 2>&1 | tail -3

# --- Phase 1: WIRE THE AGENT (subject under test) ---
echo "== phase 1: agent wiring =="
wait_for 60 "livekit server healthy" bash -c "$LKCLI room list" || FAIL=1
wait_for 60 "agent HTTP up" bash -c "curl -sf --max-time 2 http://127.0.0.1:8088/stats" || FAIL=1
wait_for 60 "agent connected to room" bash -c "[ \"\$(curl -s http://127.0.0.1:8088/stats | python3 -c 'import sys,json;print(json.load(sys.stdin)[\"connected\"])')\" = True ]" || FAIL=1

if [ "$FAIL" -ne 0 ]; then
  echo "AGENT WIRING FAILED — not proceeding to SIP."
  exit 1
fi

# Confirm the agent is a real room participant (server-side view).
echo "-- room participants (agent should be present) --"
$LKCLI room participants list spike-room 2>/dev/null | grep -i mvp-agent \
  && echo "ok: agent is a room participant" \
  || { echo "FAIL: agent not in room participants"; FAIL=1; }

# --- Phase 2: SIP STIMULUS (only after wiring is up) ---
echo "== phase 2: SIP call into the wired agent =="
$LKCLI sip inbound create /w/trunk.json >/dev/null 2>&1 || \
  docker run --rm --network host \
    -e LIVEKIT_URL=ws://127.0.0.1:7880 -e LIVEKIT_API_KEY=devkey \
    -e LIVEKIT_API_SECRET=devsecret_devsecret_devsecret_32b \
    -v "$PWD":/w livekit/livekit-cli:latest sip inbound create /w/trunk.json >/dev/null 2>&1
docker run --rm --network host \
  -e LIVEKIT_URL=ws://127.0.0.1:7880 -e LIVEKIT_API_KEY=devkey \
  -e LIVEKIT_API_SECRET=devsecret_devsecret_devsecret_32b \
  livekit/livekit-cli:latest sip dispatch create --name mvp-dispatch --direct spike-room -y >/dev/null 2>&1

# Place the SIP call (PCMA, media port, 10s media window) in the background.
docker run -d --name mvp-sipp --network host -v "$PWD/uac.xml":/sipp/uac.xml \
  --entrypoint sipp ctaloi/sipp:latest \
  -sf /sipp/uac.xml -m 1 -r 1 -l 1 -i 127.0.0.1 -mi 127.0.0.1 -mp 6000 \
  127.0.0.1:5062 -timeout 25s >/dev/null 2>&1

# --- Phase 3: ASSERT the agent received and sent audio ---
echo "== phase 3: assertions =="
# The agent must subscribe to the caller's track and receive audio frames.
wait_for 20 "agent subscribed to caller track" bash -c \
  "[ \"\$(curl -s http://127.0.0.1:8088/stats | python3 -c 'import sys,json;print(json.load(sys.stdin)[\"subscribed_tracks\"])')\" -ge 1 ]" || FAIL=1
wait_for 20 "agent received caller audio frames" bash -c \
  "[ \"\$(curl -s http://127.0.0.1:8088/stats | python3 -c 'import sys,json;print(json.load(sys.stdin)[\"frames_in\"])')\" -gt 0 ]" || FAIL=1

FRAMES_OUT=$(agent_stat frames_out)
[ "${FRAMES_OUT:-0}" -gt 0 ] && echo "ok: agent published audio frames ($FRAMES_OUT)" \
  || { echo "FAIL: agent published no audio"; FAIL=1; }

echo "-- final agent stats --"; curl -s http://127.0.0.1:8088/stats; echo

if [ "$FAIL" -eq 0 ]; then
  echo "MVP E2E: PASS — real LiveKit agent wired as subscriber; SIP call exchanged audio both ways"
  exit 0
else
  echo "MVP E2E: FAIL"
  exit 1
fi
