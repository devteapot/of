#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd -- "${script_dir}/.." && pwd)"
stage_dir="${repo_dir}/target/vercel"

"${script_dir}/check-toolchain.sh"
spacetime build --module-path "${repo_dir}/modules/match"
"${script_dir}/build-web-production.sh"

mkdir -p "${stage_dir}"
find "${stage_dir}" -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
cp -R "${repo_dir}/target/web/." "${stage_dir}/"
mv "${stage_dir}/index.html" "${stage_dir}/game.html"
cp -R "${repo_dir}/deployment/vercel/." "${stage_dir}/"
mkdir -p "${stage_dir}/assets"
cp \
  "${repo_dir}/modules/match/target/wasm32-unknown-unknown/release/match_module.wasm" \
  "${stage_dir}/assets/match_module.wasm"
