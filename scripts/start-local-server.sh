#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd -- "${script_dir}/.." && pwd)"

"${script_dir}/check-toolchain.sh"

exec spacetime start \
  --listen-addr 127.0.0.1:3000 \
  --data-dir "${repo_dir}/.spacetime-data" \
  --non-interactive
