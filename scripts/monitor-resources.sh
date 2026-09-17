#!/usr/bin/env bash
# ==============================================================================
# simplebash-pos: Real-Time Process Resource Monitor (CPU time, RSS, FDs)
#
# Usage: ./scripts/monitor-resources.sh <OUTPUT_CSV> <NAME=PID> [NAME=PID ...]
#   e.g. ./scripts/monitor-resources.sh out.csv backend=123 document-server=456
#
# Records *cumulative CPU time* per process, not `ps pcpu`: on macOS `pcpu` is
# an average over the process's whole lifetime, which silently understates a
# burst of work during a benchmark. CPU used by a phase is then the difference
# between its first and last sample, which is what scripts/bench-logging.sh does.
#
# Back-compatible with the old positional form (<PID1> <PID2> <CSV>).
# ==============================================================================

set -euo pipefail

TARGETS=()
OUT_CSV=""

if [[ "${1:-}" =~ ^[0-9]+$ ]]; then
  # Legacy form: PID1 PID2 CSV
  [ -n "${1:-}" ] && TARGETS+=("pid${1}=${1}")
  [ -n "${2:-}" ] && TARGETS+=("pid${2}=${2}")
  OUT_CSV="${3:-./target/benchmark-reports/resources.csv}"
else
  OUT_CSV="${1:-./target/benchmark-reports/resources.csv}"
  shift || true
  for ARG in "$@"; do
    TARGETS+=("${ARG}")
  done
fi

mkdir -p "$(dirname "${OUT_CSV}")"
echo "timestamp_iso,pid,process_name,cpu_seconds,rss_mb,open_fds" > "${OUT_CSV}"

# `ps -o time=` prints [[DD-]HH:]MM:SS — normalise to seconds.
cpu_seconds() {
  echo "${1}" | awk -F'[:-]' '{
    if (NF == 4)      { print $1*86400 + $2*3600 + $3*60 + $4 }
    else if (NF == 3) { print $1*3600 + $2*60 + $3 }
    else if (NF == 2) { print $1*60 + $2 }
    else              { print $1 + 0 }
  }'
}

while true; do
  TIMESTAMP="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  for TARGET in "${TARGETS[@]}"; do
    NAME="${TARGET%%=*}"
    PID="${TARGET##*=}"
    if [ -n "${PID}" ] && kill -0 "${PID}" 2>/dev/null; then
      PS_OUT="$(ps -p "${PID}" -o time=,rss= 2>/dev/null || true)"
      if [ -n "${PS_OUT}" ]; then
        TIME_RAW="$(echo "${PS_OUT}" | awk '{print $1}')"
        RSS_KB="$(echo "${PS_OUT}" | awk '{print $2}')"
        CPU_S="$(cpu_seconds "${TIME_RAW}")"
        RSS_MB="$(awk "BEGIN {printf \"%.2f\", ${RSS_KB} / 1024}")"
        # File descriptors: a leak shows up here long before it shows up as RSS.
        FDS="$(lsof -p "${PID}" 2>/dev/null | wc -l | tr -d ' ')"
        echo "${TIMESTAMP},${PID},${NAME},${CPU_S},${RSS_MB},${FDS}" >> "${OUT_CSV}"
      fi
    fi
  done

  sleep 1
done
