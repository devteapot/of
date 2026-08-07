#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd -- "${script_dir}/.." && pwd)"
database="${OF_DATABASE:-of-lobby}"
server="${SPACETIMEDB_SERVER:-maincloud}"

"${script_dir}/check-toolchain.sh"

spacetime publish \
  --server "${server}" \
  --module-path "${repo_dir}/modules/lobby" \
  --delete-data=never \
  --yes=remote,migrate,break-clients,skip-login \
  "${database}"

spacetime lock --server "${server}" "${database}"
