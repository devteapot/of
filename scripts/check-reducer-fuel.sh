#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 3 || $# -gt 4 ]]; then
  echo "Usage: $0 <database-identity> <reducer> <max-mean-fuel> [metrics-url]" >&2
  echo "Run this against a fresh, isolated performance database so cumulative counters are comparable." >&2
  exit 2
fi

database_identity="$1"
reducer="$2"
max_mean_fuel="$3"
metrics_url="${4:-http://127.0.0.1:3000/v1/metrics}"

curl --fail --silent --show-error "${metrics_url}" | awk \
  -v database_identity="${database_identity}" \
  -v reducer="${reducer}" \
  -v max_mean_fuel="${max_mean_fuel}" '
  function matches_labels(metric) {
    return index(metric, "db=\"" database_identity "\"") > 0 \
      && index(metric, "reducer=\"" reducer "\"") > 0
  }

  $1 ~ /^reducer_wasmtime_fuel_used\{/ && matches_labels($1) {
    fuel = $2
  }

  $1 ~ /^spacetime_reducer_plus_query_duration_sec_count\{/ && matches_labels($1) {
    calls = $2
  }

  END {
    if (fuel == "" || calls == "" || calls <= 0) {
      print "missing fuel or invocation metrics for " reducer " in " database_identity > "/dev/stderr"
      exit 2
    }
    mean = fuel / calls
    printf "%s: calls=%.0f total_fuel=%.0f mean_fuel=%.0f limit=%.0f\n", \
      reducer, calls, fuel, mean, max_mean_fuel
    if (mean > max_mean_fuel) {
      exit 1
    }
  }
'
