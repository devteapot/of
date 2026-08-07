#!/usr/bin/env bash
# Measure Trunk web-client download size and optionally enforce engineering budgets.
#
# Usage:
#   ./scripts/measure-web-bundle.sh [--build] [--enforce] [--dist DIR] [--out PATH]
#
# Defaults:
#   --dist target/web
#   --out  artifacts/browser/bundle-<timestamp>.json
#   Gzip transfer budget: 14 MiB (OF_WEB_GZIP_BUDGET_BYTES)
#   Raw .wasm budget:     50 MiB (OF_WEB_RAW_WASM_BUDGET_BYTES)
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd -- "${script_dir}/.." && pwd)"

build=0
enforce=0
dist_dir="${repo_dir}/target/web"
out_path=""
timestamp="$(date -u +%Y%m%dT%H%M%SZ)"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --build)
      build=1
      shift
      ;;
    --enforce)
      enforce=1
      shift
      ;;
    --dist)
      dist_dir="$(cd -- "$2" && pwd)"
      shift 2
      ;;
    --out)
      out_path="$2"
      shift 2
      ;;
    -h|--help)
      sed -n '2,14p' "$0"
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if [[ "${build}" -eq 1 ]]; then
  "${script_dir}/build-web-production.sh"
fi

if [[ ! -d "${dist_dir}" ]]; then
  echo "missing web dist directory: ${dist_dir}" >&2
  echo "Run with --build, or ./scripts/build-web-production.sh first." >&2
  exit 2
fi

wasm_count="$(find "${dist_dir}" -type f -name '*.wasm' | wc -l | tr -d ' ')"
if [[ "${wasm_count}" -eq 0 ]]; then
  echo "no .wasm artifacts under ${dist_dir}" >&2
  exit 2
fi

gzip_budget="${OF_WEB_GZIP_BUDGET_BYTES:-14680064}"   # 14 MiB
raw_wasm_budget="${OF_WEB_RAW_WASM_BUDGET_BYTES:-52428800}" # 50 MiB

if [[ -z "${out_path}" ]]; then
  out_dir="${repo_dir}/artifacts/browser"
  mkdir -p "${out_dir}"
  out_path="${out_dir}/bundle-${timestamp}.json"
else
  mkdir -p "$(dirname -- "${out_path}")"
fi

git_head="$(git -C "${repo_dir}" rev-parse HEAD 2>/dev/null || echo unknown)"
git_dirty="$(git -C "${repo_dir}" status --porcelain 2>/dev/null | grep -q . && echo true || echo false)"

python3 - "${dist_dir}" "${out_path}" "${gzip_budget}" "${raw_wasm_budget}" "${enforce}" "${git_head}" "${git_dirty}" "${timestamp}" <<'PY'
import gzip
import hashlib
import json
import os
import sys
from pathlib import Path

dist_dir = Path(sys.argv[1])
out_path = Path(sys.argv[2])
gzip_budget = int(sys.argv[3])
raw_wasm_budget = int(sys.argv[4])
enforce = sys.argv[5] == "1"
git_head = sys.argv[6]
git_dirty = sys.argv[7] == "true"
timestamp = sys.argv[8]

files = []
for path in sorted(p for p in dist_dir.rglob("*") if p.is_file()):
    data = path.read_bytes()
    files.append(
        {
            "path": str(path.relative_to(dist_dir)).replace(os.sep, "/"),
            "raw_bytes": len(data),
            "gzip9_bytes": len(gzip.compress(data, compresslevel=9)),
            "sha256_16": hashlib.sha256(data).hexdigest()[:16],
        }
    )

total_raw = sum(item["raw_bytes"] for item in files)
total_gzip = sum(item["gzip9_bytes"] for item in files)
wasm_files = [item for item in files if item["path"].endswith(".wasm")]
largest_wasm_raw = max((item["raw_bytes"] for item in wasm_files), default=0)
largest_wasm_gzip = max((item["gzip9_bytes"] for item in wasm_files), default=0)

gzip_ok = total_gzip <= gzip_budget
raw_ok = largest_wasm_raw <= raw_wasm_budget
passed = gzip_ok and raw_ok

report = {
    "kind": "web-bundle-size",
    "measured_at_utc": timestamp,
    "git_head": git_head,
    "git_dirty": git_dirty,
    "dist_dir": str(dist_dir),
    "budgets": {
        "total_gzip9_bytes": gzip_budget,
        "largest_wasm_raw_bytes": raw_wasm_budget,
    },
    "totals": {
        "raw_bytes": total_raw,
        "gzip9_bytes": total_gzip,
        "file_count": len(files),
        "largest_wasm_raw_bytes": largest_wasm_raw,
        "largest_wasm_gzip9_bytes": largest_wasm_gzip,
    },
    "status": {
        "total_gzip9_ok": gzip_ok,
        "largest_wasm_raw_ok": raw_ok,
        "passed": passed,
    },
    "files": files,
}

out_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

def fmt(n: int) -> str:
    return f"{n:,} ({n / (1024 * 1024):.2f} MiB)"

print(f"web bundle: {dist_dir}")
print(f"  files:              {len(files)}")
print(f"  total raw:          {fmt(total_raw)}")
print(f"  total gzip-9:       {fmt(total_gzip)}  budget {fmt(gzip_budget)}  {'OK' if gzip_ok else 'OVER'}")
print(f"  largest wasm raw:   {fmt(largest_wasm_raw)}  budget {fmt(raw_wasm_budget)}  {'OK' if raw_ok else 'OVER'}")
print(f"  largest wasm gzip:  {fmt(largest_wasm_gzip)}")
print(f"  report:             {out_path}")
print(f"  gate:               {'PASS' if passed else 'FAIL'}")

if enforce and not passed:
    sys.exit(1)
PY
