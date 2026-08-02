#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd -- "${script_dir}/.." && pwd)"

"${script_dir}/check-toolchain.sh"

spacetime build --module-path "${repo_dir}/modules/match"
spacetime publish \
  --server local \
  --module-path "${repo_dir}/modules/match" \
  --yes \
  of-match-dev

"${script_dir}/generate-bindings.sh"
