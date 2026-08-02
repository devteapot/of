#!/usr/bin/env bash
set -euo pipefail

required_spacetime="2.7.1"
required_rust="1.95.0"

if ! command -v spacetime >/dev/null 2>&1; then
  echo "SpacetimeDB CLI is missing. Install ${required_spacetime}." >&2
  exit 1
fi

spacetime_version="$(spacetime --version)"
if [[ "${spacetime_version}" != *"spacetimedb tool version ${required_spacetime};"* ]]; then
  echo "Expected SpacetimeDB CLI ${required_spacetime}." >&2
  echo "Run: spacetime version install ${required_spacetime} --use --yes" >&2
  exit 1
fi

rust_version="$(rustc --version)"
if [[ "${rust_version}" != "rustc ${required_rust} "* ]]; then
  echo "Expected Rust ${required_rust}; rust-toolchain.toml should install it automatically." >&2
  exit 1
fi

echo "Toolchain ready: Rust ${required_rust}, SpacetimeDB ${required_spacetime}."
