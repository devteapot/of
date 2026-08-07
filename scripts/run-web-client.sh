#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
client_dir="$(cd -- "${script_dir}/../crates/game-client" && pwd)"

# Trunk expects this conventional presence-based variable to contain a Boolean.
if [[ -n "${NO_COLOR:-}" && "${NO_COLOR:-}" != "true" && "${NO_COLOR:-}" != "false" ]]; then
  export NO_COLOR=true
fi

if ! command -v trunk >/dev/null 2>&1; then
  echo "Trunk is required. Install it with: cargo install trunk --version 0.21.14 --locked" >&2
  exit 1
fi

cd "${client_dir}"
exec trunk serve "$@"
