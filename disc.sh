#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Extract --env <name> from args (default: local)
ENV="local"
PASS_ARGS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --env)
      [[ $# -ge 2 ]] || { echo "disc.sh: --env requires an argument" >&2; exit 1; }
      ENV="$2"
      shift 2
      ;;
    *)
      PASS_ARGS+=("$1")
      shift
      ;;
  esac
done

# Load .env.<env> file
ENV_FILE="${SCRIPT_DIR}/.env.${ENV}"
if [[ -f "${ENV_FILE}" ]]; then
  set -a
  # shellcheck source=/dev/null
  source "${ENV_FILE}"
  set +a
else
  echo "disc.sh: ${ENV_FILE} not found — create it from .env.example" >&2
  exit 1
fi

# Build CLI flags from env vars (only when non-empty)
EXTRA_ARGS=()

[[ -n "${DISC_HTTP_BASE_URL:-}" ]] && EXTRA_ARGS+=("--http-base-url" "${DISC_HTTP_BASE_URL}")
[[ -n "${DISC_WS_URL:-}"        ]] && EXTRA_ARGS+=("--ws-url"        "${DISC_WS_URL}")
[[ -n "${DISC_CLIENT_ID:-}"     ]] && EXTRA_ARGS+=("--client-id"     "${DISC_CLIENT_ID}")
[[ -n "${DISC_API_KEY:-}"       ]] && EXTRA_ARGS+=("--api-key"       "${DISC_API_KEY}")

exec cargo run --bin disc -- "${EXTRA_ARGS[@]}" "${PASS_ARGS[@]}"
