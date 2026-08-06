#!/usr/bin/env bash
# Run a destructive multi-scale match-perf matrix against fresh local databases.
#
# Each cell publishes a unique database with --delete-data, runs match-perf
# run-local, and appends one row to matrix.csv. Existing run directories and the
# matrix CSV are never overwritten in place (matrix.csv is append-only; if it
# already exists the script continues appending after a header check).
#
# Usage:
#   ./scripts/run-match-perf-matrix.sh --confirm-destructive-matrix [--viewer]
#
# --viewer attaches one Bevy client to player 1 of each matrix cell after the
# load worker has created its identity token. Headless load and CSV/JSON output
# remain unchanged.
#
# Environment overrides:
#   OF_PERF_HOST          SpacetimeDB host URI (default http://127.0.0.1:3000)
#   OF_PERF_SERVER        spacetime publish --server target (default: local).
#                         Use this to select host/server publication explicitly
#                         (e.g. a named remote server config) independent of the
#                         client --host URI workers/coordinator dial.
#   OF_PERF_PLAYERS       Space-separated player counts (default: 2 8 32 128 500)
#   OF_PERF_PRESETS       Space-separated presets (default: dev playtest validation)
#   OF_PERF_SHARD_SIZE    Worker shard size (default: 32)
#   OF_PERF_EXPAND_STEPS  Logical expand steps (default: 40)
#   OF_PERF_REBALANCE_STEPS  Logical front-rebalance steps (default: 40)
#   OF_PERF_ATTACK_STEPS  Logical attack steps (default: 0)
#   OF_PERF_REEXPAND_STEPS
#   OF_PERF_WARMUP_STEPS  Shared warmup steps (default: 120)
#   OF_PERF_OUT_ROOT      Root for run dirs + matrix.csv (default: artifacts/performance/matrix-<ts>)
#   OF_PERF_TIMEOUT_SECS  Per-cell run-local timeout (default: 3600)
#   OF_PERF_BIN           match-perf binary (default: cargo run -p match-perf --)
#   OF_PERF_VIEWER        1/true/yes/on to launch a Bevy viewer (default: 0)

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd -- "${script_dir}/.." && pwd)"
cd "${repo_dir}"

confirm=0
viewer_raw="${OF_PERF_VIEWER:-0}"
for arg in "$@"; do
  case "${arg}" in
    --confirm-destructive-matrix) confirm=1 ;;
    --viewer) viewer_raw=1 ;;
    -h|--help)
      sed -n '1,40p' "$0"
      exit 0
      ;;
    *)
      echo "Unknown argument: ${arg}" >&2
      echo "Usage: $0 --confirm-destructive-matrix [--viewer]" >&2
      exit 2
      ;;
  esac
done

if [[ "${confirm}" -ne 1 ]]; then
  echo "Refusing to run: this matrix publishes unique databases with --delete-data." >&2
  echo "Re-run with the explicit flag: $0 --confirm-destructive-matrix" >&2
  exit 2
fi

# Portable truthy check (Bash 3.2-safe; avoids ${var,,}).
viewer=0
viewer_lc="$(printf '%s' "${viewer_raw}" | tr '[:upper:]' '[:lower:]')"
case "${viewer_lc}" in
  1|true|yes|on) viewer=1 ;;
esac

host="${OF_PERF_HOST:-http://127.0.0.1:3000}"
# Explicit publish target, independent of the client host URI.
publish_server="${OF_PERF_SERVER:-local}"
# shellcheck disable=SC2206
players=( ${OF_PERF_PLAYERS:-2 8 32 128 500} )
# shellcheck disable=SC2206
presets=( ${OF_PERF_PRESETS:-dev playtest validation} )
shard_size="${OF_PERF_SHARD_SIZE:-32}"
expand_steps="${OF_PERF_EXPAND_STEPS:-40}"
rebalance_steps="${OF_PERF_REBALANCE_STEPS:-40}"
attack_steps="${OF_PERF_ATTACK_STEPS:-0}"
reexpand_steps="${OF_PERF_REEXPAND_STEPS:-20}"
warmup_steps="${OF_PERF_WARMUP_STEPS:-120}"
timeout_secs="${OF_PERF_TIMEOUT_SECS:-3600}"
ts="$(date +%Y%m%d-%H%M%S)"
out_root="${OF_PERF_OUT_ROOT:-artifacts/performance/matrix-${ts}}"
mkdir -p "${out_root}"

matrix_csv="${out_root}/matrix.csv"
if [[ ! -f "${matrix_csv}" ]]; then
  echo "timestamp,database,preset,players,shard_size,run_dir,status,observed_steps,p50_ms,p95_ms,p99_ms,max_ms,max_packets,max_orders,max_fronts,front_rebalance_attempted,front_rebalance_accepted,front_rebalance_skipped,failures,early_completion" \
    > "${matrix_csv}"
fi

# Track the foreground run-local PID (launched in background + wait) so the trap
# can tear it down on EXIT/INT/TERM without leaving orphans. Viewer uses a
# copied player-1 identity token under .spacetime-data for profile reuse.
run_local_pid=""
viewer_pid=""
viewer_token=""
stop_pid() {
  local pid="${1:-}"
  if [[ -n "${pid}" ]] && kill -0 "${pid}" 2>/dev/null; then
    kill "${pid}" 2>/dev/null || true
    # Give a cooperative exit a moment, then force if still alive.
    local _i
    for _i in 1 2 3 4 5; do
      kill -0 "${pid}" 2>/dev/null || break
      sleep 0.1
    done
    if kill -0 "${pid}" 2>/dev/null; then
      kill -9 "${pid}" 2>/dev/null || true
    fi
    wait "${pid}" 2>/dev/null || true
  fi
}
cleanup_viewer() {
  stop_pid "${viewer_pid:-}"
  viewer_pid=""
  if [[ -n "${viewer_token:-}" ]]; then
    rm -f "${viewer_token}" || true
    viewer_token=""
  fi
}
cleanup() {
  cleanup_viewer
  stop_pid "${run_local_pid:-}"
  run_local_pid=""
}
trap cleanup EXIT INT TERM

run_match_perf() {
  if [[ -n "${OF_PERF_BIN:-}" ]]; then
    "${OF_PERF_BIN}" "$@"
  else
    cargo run -q -p match-perf -- "$@"
  fi
}

echo "Matrix output root: ${out_root}"
echo "Host: ${host}"
echo "Publish server: ${publish_server}"
echo "Players: ${players[*]}"
echo "Presets: ${presets[*]}"
echo "Steps: expand=${expand_steps} rebalance=${rebalance_steps} attack=${attack_steps} reexpand=${reexpand_steps} warmup=${warmup_steps}"
echo "Shard size: ${shard_size}"
echo "Viewer: $([[ "${viewer}" -eq 1 ]] && echo enabled || echo disabled)"

"${script_dir}/check-toolchain.sh"
spacetime build --module-path "${repo_dir}/modules/match"
if [[ "${viewer}" -eq 1 ]]; then
  cargo build -q -p game-client
fi

overall_status=0
for preset in "${presets[@]}"; do
  for player_count in "${players[@]}"; do
    db="of-match-perf-m-${preset}-${player_count}p-${ts}"
    run_dir="${out_root}/${preset}-${player_count}p"
    token_dir="${out_root}/tokens-${preset}-${player_count}p"
    mkdir -p "${token_dir}"
    cell_status="ok"
    echo
    echo "=== matrix cell preset=${preset} players=${player_count} db=${db} ==="

    if ! spacetime publish \
      --server "${publish_server}" \
      --module-path "${repo_dir}/modules/match" \
      --delete-data=always \
      --yes \
      "${db}"; then
      cell_status="publish_failed"
      overall_status=1
      echo "$(date -u +%Y-%m-%dT%H:%M:%SZ),${db},${preset},${player_count},${shard_size},${run_dir},${cell_status},,,,,,,,,,,,,," \
        >> "${matrix_csv}"
      continue
    fi

    set +e
    # Ensure no leftover viewer from a prior cell holds the profile token.
    cleanup_viewer
    run_match_perf run-local \
      --host "${host}" \
      --database "${db}" \
      --token-dir "${token_dir}" \
      --preset "${preset}" \
      --players "${player_count}" \
      --shard-size "${shard_size}" \
      --output-dir "${run_dir}" \
      --expand-steps "${expand_steps}" \
      --rebalance-steps "${rebalance_steps}" \
      --attack-steps "${attack_steps}" \
      --reexpand-steps "${reexpand_steps}" \
      --warmup-steps "${warmup_steps}" \
      --timeout-secs "${timeout_secs}" \
      &
    run_local_pid=$!

    if [[ "${viewer}" -eq 1 ]]; then
      # Unique profile per cell so token reuse never collides across presets.
      viewer_profile="perf-viewer-${preset}-${player_count}p-${ts}"
      viewer_token="${repo_dir}/.spacetime-data/client-${viewer_profile}.token"
      worker_token="${token_dir}/player-1.token"
      viewer_bin="${CARGO_TARGET_DIR:-${repo_dir}/target}/debug/game-client"
      if [[ ! -x "${viewer_bin}" ]]; then
        # Fall back to cargo run when the prebuilt binary path differs.
        viewer_bin=""
      fi
      (
        waited=0
        while [[ "${waited}" -lt 600 ]]; do
          if [[ -s "${worker_token}" && -f "${run_dir}/ready.marker" ]]; then
            mkdir -p "$(dirname -- "${viewer_token}")"
            # Copy the worker seat token so the viewer reuses the same identity.
            cp "${worker_token}" "${viewer_token}"
            chmod 600 "${viewer_token}" 2>/dev/null || true
            if [[ -n "${viewer_bin}" ]]; then
              exec "${viewer_bin}" \
                --host "${host}" \
                --database "${db}" \
                --player 1 \
                --name "Perf Viewer" \
                --profile "${viewer_profile}"
            else
              exec cargo run -q -p game-client -- \
                --host "${host}" \
                --database "${db}" \
                --player 1 \
                --name "Perf Viewer" \
                --profile "${viewer_profile}"
            fi
          fi
          sleep 0.25
          waited=$((waited + 1))
        done
        echo "Viewer did not start: timed out waiting for player-1 token/readiness" >&2
        exit 1
      ) &
      viewer_pid=$!
    fi

    wait "${run_local_pid}"
    exit_code=$?
    run_local_pid=""
    cleanup_viewer
    set -e

    if [[ "${exit_code}" -ne 0 ]]; then
      cell_status="run_failed_${exit_code}"
      overall_status=1
    fi

    summary="${run_dir}/summary.json"
    if [[ -f "${summary}" ]]; then
      # Prefer python for stable JSON field extraction; fall back to blanks.
      row="$(
        python3 - "${summary}" "${db}" "${preset}" "${player_count}" "${shard_size}" "${run_dir}" "${cell_status}" <<'PY' || true
import datetime, glob, json, os, sys
path, db, preset, players, shard, run_dir, status = sys.argv[1:8]
try:
    s = json.load(open(path))
except Exception:
    print(f"{datetime.datetime.utcnow().strftime('%Y-%m-%dT%H:%M:%SZ')},{db},{preset},{players},{shard},{run_dir},{status},,,,,,,,,,,,,")
    raise SystemExit(0)
front = {"attempted": 0, "accepted": 0, "skipped": 0}
for worker_path in glob.glob(os.path.join(run_dir, "worker-*.jsonl")):
    try:
        with open(worker_path) as worker:
            for line in worker:
                event = json.loads(line)
                if event.get("event") == "front_rebalance_summary":
                    for key in front:
                        front[key] += int(event.get(key, 0))
    except Exception:
        pass
print(
    f"{datetime.datetime.utcnow().strftime('%Y-%m-%dT%H:%M:%SZ')},"
    f"{db},{preset},{players},{shard},{run_dir},{status},"
    f"{s.get('observed_steps','')},"
    f"{s.get('observed_ms_per_step_p50','')},"
    f"{s.get('observed_ms_per_step_p95','')},"
    f"{s.get('observed_ms_per_step_p99','')},"
    f"{s.get('observed_ms_per_step_max','')},"
    f"{s.get('max_packets','')},"
    f"{s.get('max_active_orders','')},"
    f"{s.get('max_fronts','')},"
    f"{front['attempted']},"
    f"{front['accepted']},"
    f"{front['skipped']},"
    f"{s.get('failures','')},"
    f"{s.get('early_completion','')}"
)
PY
      )"
      echo "${row}" >> "${matrix_csv}"
    else
      echo "$(date -u +%Y-%m-%dT%H:%M:%SZ),${db},${preset},${player_count},${shard_size},${run_dir},${cell_status},,,,,,,,,,,,,," \
        >> "${matrix_csv}"
    fi
  done
done

echo
echo "Matrix finished with status ${overall_status}"
echo "matrix.csv: ${matrix_csv}"
exit "${overall_status}"
