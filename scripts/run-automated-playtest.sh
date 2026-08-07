#!/usr/bin/env bash
# Fully automated no-human cluster-controls playtest against a real local match.
#
# Publishes a fresh isolated database (never of-match-dev), runs match-playtest,
# and writes machine-generated evidence under docs/playtests/ and artifacts/playtests/.
#
# Prerequisites:
#   Local SpacetimeDB on 127.0.0.1:3000 (./scripts/start-local-server.sh)
#
# Examples:
#   ./scripts/run-automated-playtest.sh
#   OF_PLAYTEST_DATABASE=of-match-e2e-auto ./scripts/run-automated-playtest.sh
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd -- "${script_dir}/.." && pwd)"

host="${OF_PLAYTEST_HOST:-http://127.0.0.1:3000}"
server="${OF_PLAYTEST_SERVER:-local}"
database="${OF_PLAYTEST_DATABASE:-of-match-e2e-auto}"
timeout_secs="${OF_PLAYTEST_TIMEOUT_SECS:-30}"
contact_budget_secs="${OF_PLAYTEST_CONTACT_BUDGET_SECS:-240}"
token_dir="${OF_PLAYTEST_TOKEN_DIR:-.match-playtest-tokens}"

if [[ "${database}" == "of-match-dev" ]]; then
  echo "Refusing to run against of-match-dev; set OF_PLAYTEST_DATABASE to an isolated test database." >&2
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

echo "==> Running automated cluster-controls playtest"
rm -rf "${repo_dir}/${token_dir}"
cd "${repo_dir}"
set +e
cargo run -p match-playtest -- \
  --host "${host}" \
  --database "${database}" \
  --token-dir "${token_dir}" \
  --timeout-secs "${timeout_secs}" \
  --contact-budget-secs "${contact_budget_secs}"
exit_code=$?
set -e

if [[ "${exit_code}" -eq 0 ]]; then
  echo "==> PASS (exit 0)"
elif [[ "${exit_code}" -eq 2 ]]; then
  echo "==> FAIL (exit 2): see docs/playtests/cluster-controls-v1-automated-*.md"
else
  echo "==> FATAL (exit ${exit_code})"
fi

exit "${exit_code}"
