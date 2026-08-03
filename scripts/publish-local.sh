#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd -- "${script_dir}/.." && pwd)"
database="of-match-dev"

delete_data=()
if [[ $# -ne 0 ]]; then
  if [[ $# -eq 2 && "$1" == "--fresh" && "$2" == "--confirm-delete-of-match-dev" ]]; then
    echo "WARNING: permanently deleting all match data in the local '${database}' database."
    echo "Player slots, terrain state, orders, and receipts will be recreated from scratch."
    delete_data=(--delete-data=always)
  else
    echo "Usage: $0 [--fresh --confirm-delete-of-match-dev]" >&2
    echo "Without arguments, publishing preserves the current local match." >&2
    echo "The fresh form permanently deletes only the local '${database}' database state." >&2
    exit 2
  fi
fi

"${script_dir}/check-toolchain.sh"

spacetime build --module-path "${repo_dir}/modules/match"
spacetime publish \
  --server local \
  --module-path "${repo_dir}/modules/match" \
  "${delete_data[@]}" \
  --yes \
  "${database}"

"${script_dir}/generate-bindings.sh"
