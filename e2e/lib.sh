# Shared helpers for the e2e scripts. Source this file; do not run it.
#
# Docker-first: every tool runs in a container. The host needs only bash, the
# Docker CLI, and basic shell utilities (grep, sleep, date, seq).

# Logs that a script keeps (migrate output and the like). Git-ignored.
E2E_OUT="${E2E_OUT:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/.out}"
mkdir -p "$E2E_OUT"

E2E_PY_IMAGE="${E2E_PY_IMAGE:-python:3.12-slim}"
E2E_AGENT_STATS_URL="${E2E_AGENT_STATS_URL:-http://127.0.0.1:8088/stats}"

# wait_for <timeout_s> <desc> <cmd...>
# Run <cmd> once a second until it succeeds or the deadline passes.
wait_for() {
  local timeout=$1 desc=$2; shift 2
  local deadline=$(( $(date +%s) + timeout ))
  until "$@" >/dev/null 2>&1; do
    if [ "$(date +%s)" -ge "$deadline" ]; then echo "FAIL: timed out: $desc"; return 1; fi
    sleep 1
  done
  echo "ok: $desc"
}

# agent_stats_json
# Print the agent /stats JSON, or nothing when the agent does not answer.
agent_stats_json() {
  docker run --rm --network host "$E2E_PY_IMAGE" python3 -c '
import sys, urllib.request
try:
    print(urllib.request.urlopen(sys.argv[1], timeout=2).read().decode())
except Exception:
    pass
' "$E2E_AGENT_STATS_URL" 2>/dev/null
}

# agent_stat <field>
# Print one field of the agent /stats JSON, or nothing.
agent_stat() {
  docker run --rm --network host "$E2E_PY_IMAGE" python3 -c '
import json, sys, urllib.request
try:
    print(json.load(urllib.request.urlopen(sys.argv[1], timeout=2)).get(sys.argv[2], ""))
except Exception:
    print("")
' "$E2E_AGENT_STATS_URL" "$1" 2>/dev/null
}

# Predicates for wait_for.
agent_connected() { [ "$(agent_stat connected)" = "True" ]; }
agent_subscribed() { local n; n=$(agent_stat subscribed_tracks); [ "${n:-0}" -ge 1 ] 2>/dev/null; }
agent_frames_in() { local n; n=$(agent_stat frames_in); [ "${n:-0}" -gt 0 ] 2>/dev/null; }

# Pull the helper image once, so the first poll does not spend its deadline on
# the download.
docker image inspect "$E2E_PY_IMAGE" >/dev/null 2>&1 || docker pull -q "$E2E_PY_IMAGE" >/dev/null
