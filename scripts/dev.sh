#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
database="of-match-dev"
player_count=2
map_size=""
client_args=()
has_client_args=false
client_pids=()
client_pid_count=0

usage() {
  cat <<'EOF'
Usage: ./scripts/dev.sh [PLAYERS [MAP_SIZE]] [options] [-- CLIENT_ARGS...]

Launch a local development match and one client per player.

Options:
  -p, --players N     Number of client windows (2-500, default: 2)
  -m, --map SIZE      Configure the match and auto-join every client.
                      SIZE may be 64, 128, or 192.
  -h, --help          Show this help.

Without --map, clients open normally on the interactive lobby screen. Each
window receives a distinct profile and a prefilled player name, but no seat is
claimed automatically.

Examples:
  ./scripts/dev.sh
  ./scripts/dev.sh --players 4
  ./scripts/dev.sh --players 4 --map 128
  ./scripts/dev.sh 4 128
  ./scripts/dev.sh 2 64 -- --host http://127.0.0.1:3000
EOF
}

is_player_count() {
  [[ "$1" =~ ^[0-9]+$ ]] && (( 10#$1 >= 2 && 10#$1 <= 500 ))
}

normalize_map_size() {
  case "$1" in
    64|small|dev64)
      printf '64'
      ;;
    128|medium|playtest128)
      printf '128'
      ;;
    192|large|validation192)
      printf '192'
      ;;
    *)
      return 1
      ;;
  esac
}

positional=()
while (( $# > 0 )); do
  case "$1" in
    -p|--players)
      if (( $# < 2 )); then
        echo "Missing value for $1" >&2
        usage >&2
        exit 2
      fi
      player_count="$2"
      shift 2
      ;;
    -m|--map)
      if (( $# < 2 )); then
        echo "Missing value for $1" >&2
        usage >&2
        exit 2
      fi
      map_size="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      if (( $# > 0 )); then
        client_args=("$@")
        has_client_args=true
      fi
      break
      ;;
    -*)
      echo "Unknown dev option: $1 (put client options after --)" >&2
      usage >&2
      exit 2
      ;;
    *)
      positional+=("$1")
      shift
      ;;
  esac
done

if (( ${#positional[@]} > 2 )); then
  echo "Expected at most PLAYERS and MAP_SIZE as positional arguments" >&2
  usage >&2
  exit 2
fi
if (( ${#positional[@]} >= 1 )); then
  player_count="${positional[0]}"
fi
if (( ${#positional[@]} == 2 )); then
  map_size="${positional[1]}"
fi

if ! is_player_count "${player_count}"; then
  echo "Player count must be an integer from 2 through 500: ${player_count}" >&2
  exit 2
fi
player_count="$((10#${player_count}))"

if [[ -n "${map_size}" ]]; then
  if ! map_size="$(normalize_map_size "${map_size}")"; then
    echo "Map size must be 64, 128, or 192" >&2
    exit 2
  fi
fi

# Give each background client its own process group so cargo and the game are
# both stopped on restart or exit.
set -m

configure_match() {
  local preset

  case "${map_size}" in
    64)
      preset='{"dev64":{}}'
      ;;
    128)
      preset='{"playtest128":{}}'
      ;;
    192)
      preset='{"validation192":{}}'
      ;;
    *)
      echo "Internal error: unsupported map size ${map_size}" >&2
      return 2
      ;;
  esac

  echo "Configuring ${player_count}-player match on a ${map_size}x${map_size} map..."
  spacetime call --server local "${database}" configure_match "${preset}" "${player_count}"
}

reset_database() {
  echo "Clearing the development database..."
  "${script_dir}/publish-local.sh" --fresh --confirm-delete-of-match-dev
  if [[ -n "${map_size}" ]]; then
    configure_match
  fi
}

start_clients() {
  local player

  if [[ -n "${map_size}" ]]; then
    echo "Starting ${player_count} auto-joined clients..."
  else
    echo "Starting ${player_count} clients in interactive lobby mode..."
  fi

  for ((player = 1; player <= player_count; player++)); do
    if [[ -n "${map_size}" ]]; then
      if [[ "${has_client_args}" == true ]]; then
        OF_PLAYER="${player}" \
        OF_NAME="Player ${player}" \
        OF_PROFILE="dev-player-${player}" \
          "${script_dir}/run-client.sh" "${client_args[@]}" </dev/null &
      else
        OF_PLAYER="${player}" \
        OF_NAME="Player ${player}" \
        OF_PROFILE="dev-player-${player}" \
          "${script_dir}/run-client.sh" </dev/null &
      fi
    else
      if [[ "${has_client_args}" == true ]]; then
        env -u OF_PLAYER -u OF_AUTO_JOIN \
          OF_NAME="Player ${player}" \
          OF_PROFILE="dev-player-${player}" \
          "${script_dir}/run-client.sh" "${client_args[@]}" </dev/null &
      else
        env -u OF_PLAYER -u OF_AUTO_JOIN \
          OF_NAME="Player ${player}" \
          OF_PROFILE="dev-player-${player}" \
          "${script_dir}/run-client.sh" </dev/null &
      fi
    fi
    client_pids+=("$!")
    client_pid_count=$((client_pid_count + 1))
  done
}

stop_clients() {
  local pid

  if (( client_pid_count == 0 )); then
    return
  fi
  for pid in "${client_pids[@]}"; do
    kill -TERM -- "-${pid}" 2>/dev/null || true
  done
  for pid in "${client_pids[@]}"; do
    wait "${pid}" 2>/dev/null || true
  done
  client_pids=()
  client_pid_count=0
}

cleanup() {
  local status=$?

  trap - EXIT INT TERM
  stop_clients
  exit "${status}"
}

trap cleanup EXIT INT TERM

reset_database
start_clients

echo "Ready. Press R to restart ${player_count} clients, C to recreate the match, or Q to quit."
while IFS= read -r -s -n 1 key; do
  case "${key}" in
    r|R)
      echo
      echo "Restarting clients..."
      stop_clients
      start_clients
      ;;
    c|C)
      echo
      echo "Recreating the match..."
      stop_clients
      reset_database
      start_clients
      ;;
    q|Q)
      echo
      exit 0
      ;;
  esac
done
