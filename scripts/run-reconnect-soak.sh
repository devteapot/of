#!/usr/bin/env bash
# Headless reconnect soak against a live match database.
#
# Proves identity reclaim + subscription rebuild latency under optional
# concurrent client load. This is the native SpacetimeDB path; browser
# localStorage/WebSocket quirks still need a manual browser pass (see
# docs/browser-gates.md).
#
# Prerequisites:
#   1. Local SpacetimeDB running (./scripts/start-local-server.sh)
#   2. A published match database that already has a configured, running
#      match with at least two claimed seats — OR let this script publish a
#      fresh of-match-reconnect-soak and run match-e2e setup via --fresh.
#
# Examples:
#   # Isolated functional soak (publishes a fresh DB, joins, starts, cycles)
#   ./scripts/run-reconnect-soak.sh --fresh --cycles 20
#
#   # Against an in-flight match-perf database while workers are busy
#   ./scripts/run-reconnect-soak.sh --database of-match-perf --cycles 30 \
#     --player-one-token .match-perf-tokens/player-1.token \
#     --player-two-token .match-perf-tokens/player-2.token
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd -- "${script_dir}/.." && pwd)"

host="${OF_RECONNECT_HOST:-http://127.0.0.1:3000}"
server="${OF_RECONNECT_SERVER:-local}"
database="${OF_RECONNECT_DATABASE:-of-match-reconnect-soak}"
cycles="${OF_RECONNECT_CYCLES:-20}"
timeout_secs="${OF_RECONNECT_TIMEOUT_SECS:-60}"
fresh=0
out_path=""
player_one_token=""
player_two_token=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --host) host="$2"; shift 2 ;;
    --server) server="$2"; shift 2 ;;
    --database) database="$2"; shift 2 ;;
    --cycles) cycles="$2"; shift 2 ;;
    --timeout-secs) timeout_secs="$2"; shift 2 ;;
    --fresh) fresh=1; shift ;;
    --player-one-token) player_one_token="$2"; shift 2 ;;
    --player-two-token) player_two_token="$2"; shift 2 ;;
    --skip-setup)
      # Accepted for compatibility; soak always uses reconnect-only.
      shift
      ;;
    --out) out_path="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,24p' "$0"
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if ! [[ "${cycles}" =~ ^[0-9]+$ ]] || [[ "${cycles}" -lt 1 ]]; then
  echo "--cycles must be a positive integer" >&2
  exit 2
fi

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
if [[ -z "${out_path}" ]]; then
  out_dir="${repo_dir}/artifacts/browser"
  mkdir -p "${out_dir}"
  out_path="${out_dir}/reconnect-soak-${timestamp}.json"
else
  mkdir -p "$(dirname -- "${out_path}")"
fi

if [[ "${fresh}" -eq 1 ]]; then
  if ! command -v spacetime >/dev/null 2>&1; then
    echo "spacetime CLI required for --fresh" >&2
    exit 2
  fi
  echo "publishing fresh database ${database} on server ${server}"
  spacetime publish --server "${server}" --module-path "${repo_dir}/modules/match" \
    --delete-data=always --yes "${database}"
fi

extra_args=(
  --host "${host}"
  --database "${database}"
  --timeout-secs "${timeout_secs}"
  --reconnect-only
  --reconnect-cycles "${cycles}"
  --reconnect-report "${out_path}"
)
if [[ -n "${player_one_token}" ]]; then
  extra_args+=(--player-one-token "${player_one_token}")
fi
if [[ -n "${player_two_token}" ]]; then
  extra_args+=(--player-two-token "${player_two_token}")
fi

echo "running reconnect soak: database=${database} cycles=${cycles}"
cargo run -q -p match-e2e -- "${extra_args[@]}"
echo "reconnect soak report: ${out_path}"
