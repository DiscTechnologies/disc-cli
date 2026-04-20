#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

DISC_LOCAL_HTTP_BASE_URL="${DISC_LOCAL_HTTP_BASE_URL:-http://localhost:3001}"
DISC_LOCAL_WS_URL="${DISC_LOCAL_WS_URL:-ws://localhost:8097}"
DISC_LOCAL_CLIENT_ID="${DISC_LOCAL_CLIENT_ID:-disc-cli-local}"
DISC_LOCAL_API_KEY="${DISC_LOCAL_API_KEY:-${DISC_API_KEY:-}}"

EXTRA_ARGS=(
  "--http-base-url" "${DISC_LOCAL_HTTP_BASE_URL}"
  "--ws-url" "${DISC_LOCAL_WS_URL}"
  "--client-id" "${DISC_LOCAL_CLIENT_ID}"
)

if [[ -n "${DISC_LOCAL_API_KEY}" ]]; then
  EXTRA_ARGS+=("--api-key" "${DISC_LOCAL_API_KEY}")
fi

exec cargo run --bin disc -- "${EXTRA_ARGS[@]}" "$@"
