#!/usr/bin/env bash
set -uo pipefail

TARGET=""
PING_COUNT=20
PING_INTERVAL=0.2
RUN_MTR=0

usage() {
  cat <<USAGE
Usage: $(basename "$0") [-c count] [-i interval] [-m] <target>
  -c count      ping packet count (default: 20)
  -i interval   ping interval seconds (default: 0.2)
  -m            run mtr report if available
USAGE
}

have() {
  command -v "$1" >/dev/null 2>&1
}

while getopts ":c:i:mh" opt; do
  case "$opt" in
    c) PING_COUNT="$OPTARG" ;;
    i) PING_INTERVAL="$OPTARG" ;;
    m) RUN_MTR=1 ;;
    h) usage; exit 0 ;;
    *) usage; exit 1 ;;
  esac
done
shift $((OPTIND - 1))

if [ $# -lt 1 ]; then
  usage
  exit 1
fi
TARGET="$1"

section() {
  echo
  echo "==== $1 ===="
}

run_cmd() {
  local name="$1"
  shift

  echo "-- $name"
  if "$@"; then
    :
  else
    echo "[warn] command failed: $*"
  fi
}

show_or_warn() {
  local tool="$1"
  shift
  if have "$tool"; then
    run_cmd "$tool" "$@"
  else
    echo "[warn] $tool not available"
  fi
}

echo "Network triage started: $(date -u +"%Y-%m-%dT%H:%M:%SZ")"
echo "Target: $TARGET"

section "System and interface baseline"
show_or_warn uname uname -a
show_or_warn ip ip -br addr
show_or_warn ip ip route
show_or_warn ss ss -s

section "Name resolution"
if have getent; then
  run_cmd "getent ahosts $TARGET" getent ahosts "$TARGET"
else
  echo "[warn] getent not available"
fi

section "Active probe: ping"
if have ping; then
  run_cmd "ping" ping -n -c "$PING_COUNT" -i "$PING_INTERVAL" "$TARGET"
else
  echo "[warn] ping not available"
fi

section "TCP/IP counters"
show_or_warn nstat nstat -az
show_or_warn netstat netstat -s

section "Socket detail snapshot"
show_or_warn ss ss -tin

if [ "$RUN_MTR" -eq 1 ]; then
  section "Path probe: mtr"
  if have mtr; then
    run_cmd "mtr" mtr -rw -c 20 "$TARGET"
  else
    echo "[warn] mtr not available"
  fi
fi

section "Operator checklist"
cat <<'CHECKLIST'
- Packet loss in ping with clean local counters usually points upstream.
- Rising TCP retransmits plus stable RTT often indicates congestion or shaping.
- Errors/drops on local interfaces suggest NIC, driver, or local queue pressure.
- Route flaps or changing next-hops can cause intermittent failures.
- Collect two or more runs during good vs bad periods for comparison.
CHECKLIST
