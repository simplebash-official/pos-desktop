#!/usr/bin/env bash
# ==============================================================================
# simplebash-pos: Backend Benchmarking & Profiling Automation Suite
#
# Workflows:
#   ./scripts/profile-backend.sh bench       # Run Criterion micro-benchmarks with HTML reports
#   ./scripts/profile-backend.sh flamegraph  # Run cargo-flamegraph on backend binary
#   ./scripts/profile-backend.sh instruments # Profile using Apple Instruments on macOS
# ==============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
BACKEND_DIR="${ROOT_DIR}/backend"
PROFILE_OUT="${ROOT_DIR}/target/profiling-reports"

mkdir -p "${PROFILE_OUT}"

ACTION="${1:-bench}"

case "${ACTION}" in
  bench)
    echo "============================================================"
    echo " Running Criterion Micro-Benchmarks for simplebash-backend"
    echo "============================================================"
    cd "${BACKEND_DIR}"
    cargo bench --bench engine_benchmarks

    REPORT_DIR="${BACKEND_DIR}/target/criterion/report"
    if [ -d "${REPORT_DIR}" ]; then
      echo ""
      echo "✅ Benchmarks complete! Interactive HTML report generated at:"
      echo "   ${REPORT_DIR}/index.html"
      if command -v open >/dev/null 2>&1; then
        echo "   (Run 'open ${REPORT_DIR}/index.html' to view in browser)"
      fi
    fi
    ;;

  flamegraph)
    echo "============================================================"
    echo " Generating CPU Flamegraph with cargo-flamegraph"
    echo "============================================================"
    if ! command -v cargo-flamegraph >/dev/null 2>&1; then
      echo "Installing cargo-flamegraph..."
      cargo install cargo-flamegraph
    fi

    cd "${BACKEND_DIR}"
    FLAMEGRAPH_OUT="${PROFILE_OUT}/flamegraph_backend_$(date +%Y%m%d_%H%M%S).svg"
    echo "Writing flamegraph to ${FLAMEGRAPH_OUT}..."
    cargo flamegraph --profile profiling --bin simplebash_pos_backend -o "${FLAMEGRAPH_OUT}"
    echo "✅ Flamegraph generated: ${FLAMEGRAPH_OUT}"
    ;;

  instruments)
    echo "============================================================"
    echo " Profiling via macOS Instruments (Time Profiler)"
    echo "============================================================"
    if [ "$(uname -s)" != "Darwin" ]; then
      echo "Error: Apple Instruments is only available on macOS."
      exit 1
    fi

    cd "${BACKEND_DIR}"
    echo "Building backend with profiling symbols..."
    cargo build --profile profiling --bin simplebash_pos_backend

    TRACE_OUT="${PROFILE_OUT}/backend_$(date +%Y%m%d_%H%M%S).trace"
    BINARY="${BACKEND_DIR}/target/profiling/simplebash_pos_backend"

    echo "Recording trace with xcrun xctrace to ${TRACE_OUT}..."
    echo "Send SIGINT (Ctrl+C) to terminate recording."
    xcrun xctrace record --template 'Time Profiler' --launch -- "${BINARY}" --output "${TRACE_OUT}"
    echo "✅ Trace saved to ${TRACE_OUT}"
    echo "   (Open with 'open ${TRACE_OUT}' to view in Instruments)"
    ;;

  help|-h|--help)
    echo "Usage: $0 [bench|flamegraph|instruments|help]"
    exit 0
    ;;

  *)
    echo "Usage: $0 [bench|flamegraph|instruments|help]"
    exit 1
    ;;
esac
