# Shared helpers for the e2e scripts. Source this file; do not run it.
#
# Docker-first: every tool runs in a container. The host needs only bash, the
# Docker CLI, and basic shell utilities (grep, sleep, date, seq).

# Logs that a script keeps (migrate output and the like). Git-ignored.
E2E_OUT="${E2E_OUT:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/.out}"
mkdir -p "$E2E_OUT"

E2E_PY_IMAGE="${E2E_PY_IMAGE:-python:3.12-slim}"
# The Docker network of the probe containers. A stack on its own bridge
# network sets it, and its URLs then use service names.
E2E_NET="${E2E_NET:-host}"
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
  docker run --rm --network "$E2E_NET" "$E2E_PY_IMAGE" python3 -c '
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
  docker run --rm --network "$E2E_NET" "$E2E_PY_IMAGE" python3 -c '
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

# room_stat <stats_url> <room> <field>
# Print one field of one room in an agent /stats JSON ({"rooms": {...}}), or
# nothing. Used for the e2e LiveKit agent (:8088) and pipecat bot (:8089).
room_stat() {
  docker run --rm --network "$E2E_NET" "$E2E_PY_IMAGE" python3 -c '
import json, sys, urllib.request
try:
    rooms = json.load(urllib.request.urlopen(sys.argv[1], timeout=2))["rooms"]
    print(rooms.get(sys.argv[2], {}).get(sys.argv[3], ""))
except Exception:
    print("")
' "$1" "$2" "$3" 2>/dev/null
}

# http_ok <url>: the URL answers 2xx.
http_ok() {
  docker run --rm --network "$E2E_NET" "$E2E_PY_IMAGE" python3 -c '
import sys, urllib.request
urllib.request.urlopen(sys.argv[1], timeout=2)' "$1" >/dev/null 2>&1
}

# kine_keys <prefix>: the keys under a prefix in kine (127.0.0.1:2379), one per
# line. kine (v0.13.17) refuses --keys-only, so read whole entries: etcdctl
# prints each key, then its value, on its own line. A failed read fails here,
# so an empty answer means "no keys", never "no answer".
E2E_ETCD_IMAGE="${E2E_ETCD_IMAGE:-quay.io/coreos/etcd:v3.5.17}"
E2E_ETCD_ENDPOINT="${E2E_ETCD_ENDPOINT:-127.0.0.1:2379}"
kine_keys() {
  local out
  out=$(docker run --rm --network "$E2E_NET" --entrypoint etcdctl "$E2E_ETCD_IMAGE" \
    --endpoints="$E2E_ETCD_ENDPOINT" get --prefix "$1" 2>/dev/null) || {
    echo "kine read of $1 failed" >&2
    return 1
  }
  printf '%s\n' "$out" | sed -n 'p;n' | grep -v '^$'
  return 0
}

# kine_none <prefix>: kine answered, and has no key under the prefix.
kine_none() { local k; k=$(kine_keys "$1") && [ -z "$k" ]; }

# kine_count <prefix> <n>: kine answered, and has exactly n keys under it.
kine_count() { local k; k=$(kine_keys "$1") && [ "$(printf '%s' "$k" | grep -c .)" = "$2" ]; }

# kine_value <key>: the value of one key in kine.
kine_value() {
  docker run --rm --network "$E2E_NET" --entrypoint etcdctl "$E2E_ETCD_IMAGE" \
    --endpoints="$E2E_ETCD_ENDPOINT" get "$1" --print-value-only 2>/dev/null
}

# pg <sql>: run one query against $E2E_DATABASE_URL with a psql client
# container, as against a managed Postgres (no access to the server host).
E2E_PSQL_IMAGE="${E2E_PSQL_IMAGE:-postgres:16-alpine}"
E2E_DATABASE_URL="${E2E_DATABASE_URL:-postgres://ad:ad@127.0.0.1:5432/ad}"
pg() {
  docker run --rm --network "$E2E_NET" "$E2E_PSQL_IMAGE" psql "$E2E_DATABASE_URL" -tAc "$1" 2>/dev/null
}

# pg_eq <sql> <want>: the query prints exactly <want>.
pg_eq() { [ "$(pg "$1")" = "$2" ]; }

# pg_ge <sql> <n>: the query prints a number >= n.
pg_ge() { local v; v=$(pg "$1"); [ "${v:-0}" -ge "$2" ] 2>/dev/null; }

# kine_leader_is <node>: the leader key in kine names this node.
kine_leader_is() { kine_value /agentdialer/leader | grep -q "\"node\":\"$1\""; }
# agent_stats_json_of <url>: the whole /stats JSON of a test agent, or nothing.
agent_stats_json_of() {
  docker run --rm --network "$E2E_NET" "$E2E_PY_IMAGE" python3 -c '
import sys, urllib.request
try:
    print(urllib.request.urlopen(sys.argv[1], timeout=5).read().decode())
except Exception:
    pass
' "$1" 2>/dev/null
}
