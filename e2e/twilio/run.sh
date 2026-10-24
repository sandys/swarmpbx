#!/usr/bin/env bash
# Run e2e/twilio/twilio.py in a Python container with the repository's .env.
# The host needs only the Docker CLI. Usage:
#   e2e/twilio/run.sh whoami | inspect | setup | calls <to> <since> | teardown
# TWILIO_CREDS=2 uses TWILIO_SID2 and TWILIO_SECRET2 from .env.
# `setup` writes .env.twilio at the repository root (git-ignored).
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
[ -f "$repo/.env" ] || { echo "twilio: $repo/.env is missing (TWILIO_SID, TWILIO_SECRET)" >&2; exit 1; }
exec docker run --rm --user "$(id -u):$(id -g)" \
  --env-file "$repo/.env" ${TWILIO_CREDS:+-e TWILIO_CREDS} \
  -v "$here":/w:ro -v "$repo":/repo \
  python:3.12-slim python3 -I /w/twilio.py "$@"
