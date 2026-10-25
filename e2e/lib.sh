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

# api_send <method> <url> [json]: send one request; print the HTTP status,
# a space, and the body on one line.
api_send() {
  docker run --rm --network "$E2E_NET" "$E2E_PY_IMAGE" python3 -c '
import sys, urllib.error, urllib.request
data = sys.argv[3].encode() if len(sys.argv) > 3 else None
req = urllib.request.Request(sys.argv[2], data=data, method=sys.argv[1],
                             headers={"content-type": "application/json"})
try:
    with urllib.request.urlopen(req, timeout=5) as r:
        print(r.status, r.read().decode())
except urllib.error.HTTPError as e:
    print(e.code, e.read().decode())
except Exception as e:
    print(0, e)
' "$@" 2>/dev/null
}

# api_field <url> <python expression over v>: GET the JSON at the URL and
# print the expression, or nothing.
api_field() {
  docker run --rm --network "$E2E_NET" "$E2E_PY_IMAGE" python3 -c '
import json, sys, urllib.request
try:
    v = json.load(urllib.request.urlopen(sys.argv[1], timeout=5))
    print(eval(sys.argv[2], {}, {"v": v}))
except Exception:
    print("")
' "$1" "$2" 2>/dev/null
}

# SQL for checks that compare an attempt with a claim (`created_at`, the
# Postgres time). The node times on a row (`ended_at`, `agent_ready_at`) come
# from the node's clock, which runs on the monotonic clock between two reads of
# the Postgres clock (on WSL2 it gains about 0.6 s in 30 s); these use the
# save times of the events.
# saved_end <alias>: the save of the attempt's last event (its end, once it ended).
saved_end() { echo "(SELECT max(e.created_at) FROM attempt_events e WHERE e.attempt_id = $1.id)"; }
# left_window <alias>: the save of the attempt's agent_ready event, else its end.
left_window() {
  echo "COALESCE((SELECT min(e.created_at) FROM attempt_events e WHERE e.attempt_id = $1.id AND e.kind = 'agent_ready'), $(saved_end "$1"))"
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
# The carrier simulator (e2e/carrier-sim) reports what it saw at /calls.
E2E_SIM_URL="${E2E_SIM_URL:-http://nat:8080/calls}"
# sim_count <expr>: the number of calls at the carrier for which the Python
# expression <expr> over the call record `c` is true.
sim_count() {
  docker run --rm --network "$E2E_NET" "$E2E_PY_IMAGE" python3 -c '
import json, sys, urllib.request
try:
    calls = json.load(urllib.request.urlopen(sys.argv[1], timeout=5))
    expr = "(" + sys.argv[2] + ")"  # the parentheses allow a multi-line expression
    print(sum(1 for c in calls if eval(expr, {}, {"c": c})))
except Exception:
    print("")
' "$E2E_SIM_URL" "$1" 2>/dev/null
}
# sim_eq <expr> <n>: exactly n calls at the carrier match <expr>.
sim_eq() { [ "$(sim_count "$1")" = "$2" ]; }

# The number of rooms in an agent's /stats whose record passes a Python
# expression over r (for example "r['loud_in'] > 0").
rooms_count() {
  docker run --rm --network "$E2E_NET" "$E2E_PY_IMAGE" python3 -c '
import json, sys, urllib.request
try:
    rooms = json.load(urllib.request.urlopen(sys.argv[1], timeout=5))["rooms"]
    expr = "(" + sys.argv[2] + ")"
    print(sum(1 for r in rooms.values() if eval(expr, {}, {"r": r})))
except Exception:
    print("")
' "$1" "$2" 2>/dev/null
}
rooms_ge() { local n; n=$(rooms_count "$1" "$2"); [ "${n:-0}" -ge "$3" ] 2>/dev/null; }

# The LiveKit server of the dialer's rooms, for the probes below. They run the
# LiveKit SDK of the e2e agent image (`ad-e2e-livekit-agent`).
E2E_LK_URL="${E2E_LK_URL:-ws://livekit:7880}"
E2E_LK_KEY="${E2E_LK_KEY:-devkey}"
E2E_LK_SECRET="${E2E_LK_SECRET:-devsecret_devsecret_devsecret_32b}"

lk_py() {
  docker run --rm --network "$E2E_NET" --entrypoint python3 \
    -e LK_URL="$E2E_LK_URL" -e LK_KEY="$E2E_LK_KEY" -e LK_SECRET="$E2E_LK_SECRET" \
    ad-e2e-livekit-agent -c "$1" "${@:2}" 2>/dev/null
}

# lk_room_gone <room>: LiveKit has no room of this name.
lk_room_gone() {
  [ "$(lk_py '
import asyncio, os, sys
from livekit import api
async def main():
    lk = api.LiveKitAPI(os.environ["LK_URL"].replace("ws", "http", 1), os.environ["LK_KEY"], os.environ["LK_SECRET"])
    try:
        rooms = await lk.room.list_rooms(api.ListRoomsRequest(names=[sys.argv[1]]))
        print(len(rooms.rooms))
    finally:
        await lk.aclose()
asyncio.run(main())
' "$1")" = 0 ]
}

# lk_late_join <room>: join the room with a valid token, as an agent that
# starts late does. Prints "refused" or "joined".
lk_late_join() {
  lk_py '
import asyncio, os, sys
from livekit import api, rtc
async def main():
    token = (api.AccessToken(os.environ["LK_KEY"], os.environ["LK_SECRET"])
             .with_identity("e2e-late-agent")
             .with_grants(api.VideoGrants(room_join=True, room=sys.argv[1]))
             .to_jwt())
    room = rtc.Room()
    try:
        await asyncio.wait_for(room.connect(os.environ["LK_URL"], token), 15)
    except Exception:
        print("refused")
        return
    print("joined")
    await room.disconnect()
asyncio.run(main())
' "$1"
}
