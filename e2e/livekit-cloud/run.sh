#!/usr/bin/env bash
# Run e2e/livekit-cloud/lkcloud.py in its tool image (the official livekit-api
# SDK and the lk CLI) with the repository's .env. The host needs only
# the Docker CLI. Usage:
#   e2e/livekit-cloud/run.sh whoami | inspect | setup | call | teardown
# `setup` writes .env.lkcloud at the repository root (git-ignored).
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
[ -f "$repo/.env" ] || { echo "lkcloud: $repo/.env is missing (LIVEKIT_URL, LIVEKIT_API_KEY, LIVEKIT_API_SECRET, LIVEKIT_SIP_URI)" >&2; exit 1; }
docker build -q -t ad-e2e-lkcloud-tool "$here" >/dev/null || exit 1
exec docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp \
  --env-file "$repo/.env" \
  -v "$here":/w:ro -v "$repo":/repo \
  ad-e2e-lkcloud-tool python3 -I /w/lkcloud.py "$@"
