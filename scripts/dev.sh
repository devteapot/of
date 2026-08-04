#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
client_pids=()

# Give each background client its own process group so cargo and the game are
# both stopped on restart or exit.
set -m

clear_database() {
  echo "Clearing the development database..."
  "${script_dir}/publish-local.sh" --fresh --confirm-delete-of-match-dev
}

start_clients() {
  echo "Starting Player One and Player Two..."
  OF_PLAYER=1 OF_NAME="Player One" OF_PROFILE=player-one \
    "${script_dir}/run-client.sh" "$@" </dev/null &
  client_pids+=("$!")

  OF_PLAYER=2 OF_NAME="Player Two" OF_PROFILE=player-two \
    "${script_dir}/run-client.sh" "$@" </dev/null &
  client_pids+=("$!")
}

stop_clients() {
  local pid

  for pid in "${client_pids[@]}"; do
    kill -TERM -- "-${pid}" 2>/dev/null || true
  done
  for pid in "${client_pids[@]}"; do
    wait "${pid}" 2>/dev/null || true
  done
  client_pids=()
}

cleanup() {
  local status=$?

  trap - EXIT INT TERM
  stop_clients
  exit "${status}"
}

trap cleanup EXIT INT TERM

clear_database
start_clients "$@"

echo "Ready. Press R to restart both clients, C to clear the database, or Q to quit."
while IFS= read -r -s -n 1 key; do
  case "${key}" in
    r|R)
      echo
      echo "Restarting clients..."
      stop_clients
      start_clients "$@"
      ;;
    c|C)
      echo
      clear_database
      ;;
    q|Q)
      echo
      exit 0
      ;;
  esac
done
