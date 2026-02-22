#!/usr/bin/env bash
set -euo pipefail

TARGET="${1:-8.8.8.8}"
PING_COUNT="${PING_COUNT:-30}"
REPORT_TIME="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

have_cmd() {
  command -v "$1" >/dev/null 2>&1
}

section() {
  printf '\n===== %s =====\n' "$1"
}

safe_run_cmd() {
  local label="$1"
  shift
  local cmd="$1"
  section "$label"
  if ! have_cmd "$cmd"; then
    echo "$cmd not available"
    return 0
  fi
  if "$@"; then
    return 0
  fi
  echo "Command failed: $*"
}

echo "Network triage report"
echo "Generated: ${REPORT_TIME}"
echo "Target: ${TARGET}"

safe_run_cmd "Host identity" hostnamectl
safe_run_cmd "Interface summary" ip -br addr
safe_run_cmd "Link counters" ip -s link
safe_run_cmd "Routing table" ip route
safe_run_cmd "Socket summary" ss -s

if have_cmd ping; then
  section "Ping ${TARGET} (${PING_COUNT} probes)"
  ping -c "${PING_COUNT}" "${TARGET}" || true
else
  section "Ping"
  echo "ping not available"
fi

if have_cmd netstat; then
  safe_run_cmd "TCP/IP counters (netstat -s)" netstat -s
elif have_cmd nstat; then
  safe_run_cmd "TCP/IP counters (nstat)" nstat -az
else
  section "TCP/IP counters"
  echo "Neither netstat nor nstat is available"
fi

if have_cmd mtr; then
  safe_run_cmd "Path quality (mtr report)" mtr --report --report-cycles 20 "${TARGET}"
else
  section "Path quality"
  echo "mtr not available"
fi

if [[ -f /proc/net/snmp ]]; then
  section "Raw kernel counters (/proc/net/snmp)"
  cat /proc/net/snmp
fi

cat <<'SUMMARY'

===== Interpretation checklist =====
1. Packet loss in ping output > 0% suggests loss between source and target.
2. Rising retransmission counters in netstat/nstat suggest congestion, drops, or receiver-side pressure.
3. mtr loss that begins at one hop and persists downstream usually indicates a real fault near that hop.
4. High RX/TX errors or drops in `ip -s link` suggest NIC, driver, duplex, or queue issues.
5. Correlate with time: rerun and compare counters to confirm trends.
SUMMARY
