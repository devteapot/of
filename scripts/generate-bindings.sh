#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd -- "${script_dir}/.." && pwd)"

"${script_dir}/check-toolchain.sh"

mkdir -p "${repo_dir}/crates/match-bindings/src/module_bindings"
spacetime generate \
  --lang rust \
  --out-dir "${repo_dir}/crates/match-bindings/src/module_bindings" \
  --module-path "${repo_dir}/modules/match" \
  --yes

# SpacetimeDB codegen is syntactically valid but does not currently emit the
# exact rustfmt layout used by the pinned toolchain. Keep generated output both
# reproducible and compatible with the workspace formatting gate.
cargo fmt \
  --manifest-path "${repo_dir}/Cargo.toml" \
  --package match-bindings
