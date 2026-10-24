#!/usr/bin/env bash
# Call the agentdialer API of a compose stack from a container on the stack's
# network, so the host needs only the Docker CLI. Usage:
#   scripts/api.sh GET  /v1/campaigns
#   scripts/api.sh POST /v1/campaigns '{"org_id": 1, "name": "demo"}'
# AD_API_NETWORK (default agentdialer-quickstart_default, the network of
# docker-compose.quickstart.yml) and AD_API_URL (default http://api:8080) choose
# the stack.
set -euo pipefail
[ $# -ge 2 ] || { sed -n '2,8p' "$0"; exit 2; }
method=$1 path=$2 body=${3:-}
args=(-sS --fail-with-body -X "$method" -H 'content-type: application/json')
[ -n "$body" ] && args+=(--data "$body")
exec docker run --rm --network "${AD_API_NETWORK:-agentdialer-quickstart_default}" \
  curlimages/curl:8.11.1 "${args[@]}" "${AD_API_URL:-http://api:8080}$path"
