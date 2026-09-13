#!/usr/bin/env bash
# ==============================================================================
# jana2u-pos: Real-Time Process Resource Monitor (RSS, CPU%, FDs)
# Usage: ./scripts/monitor-resources.sh <PID1> <PID2> <OUTPUT_CSV>
# ==============================================================================

set -euo pipefail

PID1="${1:-}"
PID2="${2:-}"
OUT_CSV="${3:-./target/benchmark-reports/resources.csv}"

mkdir -p "$(dirname "${OUT_CSV}")"
echo "timestamp_iso,pid,process_name,cpu_percent,rss_mb,open_fds" > "${OUT_CSV}"

while true; do
  TIMESTAMP="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  for PID in "${PID1}" "${PID2}"; do
    if [ -n "${PID}" ] && kill -0 "${PID}" 2>/dev/null; then
      PS_OUT="$(ps -p "${PID}" -o pid=,pcpu=,rss=,comm= 2>/dev/null || true)"
      if [ -n "${PS_OUT}" ]; then
        CPU="$(echo "${PS_OUT}" | awk '{print $2}')"
        RSS_KB="$(echo "${PS_OUT}" | awk '{print $3}')"
        NAME="$(echo "${PS_OUT}" | awk '{print $4}' | xargs basename 2>/dev/null || echo "proc")"
        RSS_MB="$(awk "BEGIN {printf \"%.2f\", ${RSS_KB} / 1024}")"
        
        # Count open file descriptors via lsof
        FDS="$(lsof -p "${PID}" 2>/dev/null | wc -l | tr -d ' ')"
        
        echo "${TIMESTAMP},${PID},${NAME},${CPU},${RSS_MB},${FDS}" >> "${OUT_CSV}"
      fi
    fi
  done

  sleep 1
done
