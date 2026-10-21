#!/usr/bin/env bash
# WebRTC-bridge e2e. Subject: OUR native WebRTC participant (ad_gateway) carries
# two-way audio into a LiveKit room — NO livekit/sip. CI runs this on every
# change (see .github/workflows/ci.yml).
#
# Topology: LiveKit server + redis + the spike agent (joins room campaign-1,
# subscribes + publishes a tone, reports /stats on :8088). Our probe joins the
# same room as "sip-bridge", publishes a tone via MediaBridge, and counts the
# agent's inbound audio.
#
# Asserts (deadline-polled, non-zero exit on failure):
#   - our probe exits 0: outbound_frames > 0 AND inbound_frames > 0
#     (we published audio AND received the agent's audio);
#   - the agent received OUR audio: /stats frames_in > 0 and subscribed_tracks >= 1.
# Always tears down.
set -uo pipefail
cd "$(dirname "$0")"
. ./lib.sh

COMPOSE="docker compose -f docker-compose.webrtc.e2e.yml"
LK_API_KEY=devkey
LK_API_SECRET=devsecret_devsecret_devsecret_32b
FAIL=0

cleanup() {
  echo "== teardown =="
  docker logs webrtc-probe > "$E2E_OUT/run_webrtc_e2e.probe.log" 2>&1 || true
  docker rm -f webrtc-probe >/dev/null 2>&1 || true
  $COMPOSE down >/dev/null 2>&1 || true
  echo "== teardown complete (probe log: $E2E_OUT/run_webrtc_e2e.probe.log) =="
}
trap cleanup EXIT

echo "== build the webrtc probe (cached libwebrtc) =="
docker build -q -f ../Dockerfile.test -t agentdialer:test-build .. >/dev/null
docker run --rm -v "$PWD/..":/app:ro -v adwebrtc_target:/cache/target -v adwebrtc_cargo:/cache/cargo \
  -e CARGO_TARGET_DIR=/cache/target -e CARGO_HOME=/cache/cargo \
  agentdialer:test-build \
  cargo build -p ad_gateway --example webrtc_bridge_probe 2>&1 | tail -2
echo "ok: probe built"

echo "== start stack (livekit + redis + agent in room campaign-1) =="
$COMPOSE up -d --build 2>&1 | tail -3

wait_for 90 "livekit healthy" bash -c \
  "docker run --rm --network host -e LIVEKIT_URL=http://127.0.0.1:7880 -e LIVEKIT_API_KEY=$LK_API_KEY -e LIVEKIT_API_SECRET=$LK_API_SECRET livekit/livekit-cli:latest room list" || FAIL=1
wait_for 90 "agent connected to room campaign-1" agent_connected || FAIL=1

if [ "$FAIL" -ne 0 ]; then echo "STACK NOT HEALTHY"; exit 1; fi

echo "== run OUR WebRTC participant (the bridge) — publishes a tone, counts agent audio =="
# Run the compiled probe from the build image using the cached target binary.
docker run -d --name webrtc-probe --network host \
  -v "$PWD/..":/app:ro -v adwebrtc_target:/cache/target -v adwebrtc_cargo:/cache/cargo \
  -e CARGO_TARGET_DIR=/cache/target -e CARGO_HOME=/cache/cargo \
  -e LIVEKIT_URL=ws://127.0.0.1:7880 \
  -e LIVEKIT_API_KEY=$LK_API_KEY -e LIVEKIT_API_SECRET=$LK_API_SECRET \
  -e AD_ROOM=campaign-1 -e AD_IDENTITY=sip-bridge -e AD_SECONDS=8 \
  agentdialer:test-build \
  /cache/target/debug/examples/webrtc_bridge_probe >/dev/null 2>&1

echo "== assert: the agent received OUR audio (frames_in > 0, subscribed >= 1) =="
wait_for 40 "agent subscribed to our track" agent_subscribed || FAIL=1
wait_for 40 "agent received our tone (frames_in>0)" agent_frames_in || FAIL=1

echo "== assert: our probe got two-way audio (exit 0: outbound>0 AND inbound>0) =="
# Wait for the probe to finish and capture its exit code.
probe_exit=""
deadline=$(( $(date +%s) + 40 ))
while :; do
  st=$(docker inspect -f '{{.State.Status}}' webrtc-probe 2>/dev/null || echo gone)
  if [ "$st" = "exited" ]; then probe_exit=$(docker inspect -f '{{.State.ExitCode}}' webrtc-probe); break; fi
  if [ "$(date +%s)" -ge "$deadline" ]; then echo "FAIL: probe did not finish"; FAIL=1; break; fi
  sleep 1
done
echo "-- probe output --"; docker logs webrtc-probe 2>&1 | tail -3
echo "-- agent stats --"; agent_stats_json
if [ "${probe_exit:-1}" != "0" ]; then echo "FAIL: probe exit=$probe_exit (need 0)"; FAIL=1; else echo "ok: probe two-way audio (exit 0)"; fi

if [ "$FAIL" -eq 0 ]; then
  echo "WEBRTC E2E: PASS — our native WebRTC participant carried two-way audio into the LiveKit room with the agent; no livekit/sip"
  exit 0
else
  echo "WEBRTC E2E: FAIL"
  exit 1
fi
