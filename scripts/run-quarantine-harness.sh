#!/usr/bin/env bash
# Live quarantine integration harness against an isolated SpacetimeDB match.
#
# Publishes a fresh isolated database (never of-match-dev), enables the private
# debug harness in Lobby, then runs match-e2e --quarantine-live to prove:
#   tick → attributable conservation fault → Quarantined order
#   strength conserved at physical cells
#   subsequent ticks continue (logical_step advances)
#
# Prerequisites:
#   Local SpacetimeDB on 127.0.0.1:3000 (./scripts/start-local-server.sh)
#
# Example:
#   ./scripts/run-quarantine-harness.sh
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd -- "${script_dir}/.." && pwd)"

host="${OF_QUARANTINE_HOST:-http://127.0.0.1:3000}"
server="${OF_QUARANTINE_SERVER:-local}"
database="${OF_QUARANTINE_DATABASE:-of-match-e2e-quarantine}"
timeout_secs="${OF_QUARANTINE_TIMEOUT_SECS:-60}"
token_dir="${OF_QUARANTINE_TOKEN_DIR:-.match-e2e-quarantine-tokens}"

if [[ "${database}" == "of-match-dev" ]]; then
  echo "Refusing to run against of-match-dev; set OF_QUARANTINE_DATABASE to an isolated test database." >&2
  exit 2
fi

"${script_dir}/check-toolchain.sh"

if ! curl -s -o /dev/null -w '' "${host}/" 2>/dev/null; then
  echo "Local SpacetimeDB is not reachable at ${host}." >&2
  echo "Start it in another terminal: ./scripts/start-local-server.sh" >&2
  exit 1
fi

echo "==> Building match module and publishing fresh isolated database '${database}'"
spacetime build --module-path "${repo_dir}/modules/match"
spacetime publish \
  --server "${server}" \
  --module-path "${repo_dir}/modules/match" \
  --delete-data=always \
  --yes \
  "${database}"

echo "==> Running live quarantine harness"
rm -rf "${repo_dir}/${token_dir}"
cd "${repo_dir}"
cargo run -p match-e2e -- \
  --host "${host}" \
  --database "${database}" \
  --token-dir "${token_dir}" \
  --timeout-secs "${timeout_secs}" \
  --quarantine-live

echo "==> PASS"
