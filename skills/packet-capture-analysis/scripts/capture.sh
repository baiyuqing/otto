#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# capture.sh — Capture network packets with tcpdump
#
# Environment variables:
#   IFACE       Network interface (default: any)
#   DURATION    Capture duration in seconds (default: 30)
#   SNAP_LEN    Snapshot length in bytes, 0 = unlimited (default: 0)
#   BPF_FILTER  BPF filter expression (default: empty — all traffic)
#   OUTDIR      Output directory for pcap file (default: .)
#   RING_SIZE   Max file size in MB before rotation (default: 100)
# ---------------------------------------------------------------------------

IFACE="${IFACE:-any}"
DURATION="${DURATION:-30}"
SNAP_LEN="${SNAP_LEN:-0}"
BPF_FILTER="${BPF_FILTER:-}"
OUTDIR="${OUTDIR:-.}"
RING_SIZE="${RING_SIZE:-100}"

REPORT_TIME="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
OUTFILE="${OUTDIR}/capture-$(date +%s).pcap"

have_cmd() {
  command -v "$1" >/dev/null 2>&1
}

# --- Pre-flight checks -----------------------------------------------------

if ! have_cmd tcpdump; then
  echo "ERROR: tcpdump is not installed or not in PATH." >&2
  echo "Install with: apt-get install tcpdump  (Debian/Ubuntu)" >&2
  echo "              yum install tcpdump       (RHEL/CentOS)" >&2
  exit 1
fi

if [[ "$DURATION" -le 0 ]]; then
  echo "ERROR: DURATION must be a positive integer (got: ${DURATION})." >&2
  exit 1
fi

mkdir -p "${OUTDIR}"

# --- Build tcpdump command --------------------------------------------------

TCPDUMP_ARGS=(
  -i "${IFACE}"
  -s "${SNAP_LEN}"
  -C "${RING_SIZE}"
  -w "${OUTFILE}"
  -G "${DURATION}"
  -W 1
)

# Add the BPF filter if specified.
if [[ -n "${BPF_FILTER}" ]]; then
  TCPDUMP_ARGS+=( ${BPF_FILTER} )
fi

# --- Run capture ------------------------------------------------------------

echo "Packet capture parameters"
echo "========================="
echo "  Time:       ${REPORT_TIME}"
echo "  Interface:  ${IFACE}"
echo "  Duration:   ${DURATION}s"
echo "  Snap len:   ${SNAP_LEN} (0 = unlimited)"
echo "  BPF filter: ${BPF_FILTER:-<none>}"
echo "  Output:     ${OUTFILE}"
echo "  Max size:   ${RING_SIZE} MB"
echo ""
echo "Starting capture (will stop after ${DURATION}s) ..."
echo ""

# tcpdump -G rotates by time; -W 1 limits to a single rotation (i.e. stop
# after DURATION seconds). Some older versions do not support -W with -G.
# Fall back to timeout-based capture if -G/-W fails.
if ! tcpdump "${TCPDUMP_ARGS[@]}" 2>&1; then
  echo ""
  echo "Falling back to timeout-based capture ..."
  TCPDUMP_FALLBACK_ARGS=(
    -i "${IFACE}"
    -s "${SNAP_LEN}"
    -w "${OUTFILE}"
  )
  if [[ -n "${BPF_FILTER}" ]]; then
    TCPDUMP_FALLBACK_ARGS+=( ${BPF_FILTER} )
  fi
  timeout "${DURATION}" tcpdump "${TCPDUMP_FALLBACK_ARGS[@]}" 2>&1 || true
fi

echo ""

# --- Summary ----------------------------------------------------------------

if [[ -f "${OUTFILE}" ]]; then
  FILE_SIZE="$(du -h "${OUTFILE}" | cut -f1)"
  PACKET_COUNT="$(tcpdump -r "${OUTFILE}" 2>/dev/null | wc -l || echo "unknown")"
  echo "Capture complete"
  echo "  File:    ${OUTFILE}"
  echo "  Size:    ${FILE_SIZE}"
  echo "  Packets: ${PACKET_COUNT}"
  echo ""
  echo "Analyze with:  ./scripts/analyze.sh ${OUTFILE}"
else
  echo "WARNING: No pcap file was produced. Check permissions and interface name." >&2
  exit 1
fi
