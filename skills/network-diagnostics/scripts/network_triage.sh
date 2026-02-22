#!/usr/bin/env bash
set -uo pipefail

TARGET=""
PING_COUNT=20
PING_INTERVAL=0.2
RUN_MTR=0
TIMEOUT_S=5

usage() {
  cat <<USAGE
Usage: $(basename "$0") [-c count] [-i interval] [-m] [-t timeout] <target>
  -c count      ping packet count (default: 20)
  -i interval   ping interval seconds (default: 0.2)
  -m            run mtr report if available
  -t timeout    per-command timeout in seconds when supported (default: 5)
USAGE
}

have() {
  command -v "$1" >/dev/null 2>&1
}

is_posint() {
  [[ "$1" =~ ^[1-9][0-9]*$ ]]
}

is_posnum() {
  [[ "$1" =~ ^([0-9]+([.][0-9]+)?|[.][0-9]+)$ ]]
}

while getopts ":c:i:mt:h" opt; do
  case "$opt" in
    c)
      if ! is_posint "$OPTARG"; then
        echo "[error] -c must be a positive integer"
        exit 1
      fi
      PING_COUNT="$OPTARG"
      ;;
    i)
      if ! is_posnum "$OPTARG"; then
        echo "[error] -i must be a positive number"
        exit 1
      fi
      PING_INTERVAL="$OPTARG"
      ;;
    m) RUN_MTR=1 ;;
    t)
      if ! is_posint "$OPTARG"; then
        echo "[error] -t must be a positive integer"
        exit 1
      fi
      TIMEOUT_S="$OPTARG"
      ;;
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
  local label="$1"
  shift

  echo "-- $label"
  if have timeout; then
    if timeout "$TIMEOUT_S" "$@"; then
      :
    else
      echo "[warn] command failed or timed out: $*"
    fi
  else
    if "$@"; then
      :
    else
      echo "[warn] command failed: $*"
    fi
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
echo "Config: ping_count=$PING_COUNT ping_interval=$PING_INTERVAL run_mtr=$RUN_MTR timeout_s=$TIMEOUT_S"

section "System and interface baseline"
show_or_warn uname uname -a
show_or_warn hostname hostname
show_or_warn ip ip -br addr
show_or_warn ip ip route
show_or_warn ss ss -s
show_or_warn cat cat /proc/net/dev

section "Name resolution"
if have getent; then
  run_cmd "getent ahosts $TARGET" getent ahosts "$TARGET"
elif have nslookup; then
  run_cmd "nslookup $TARGET" nslookup "$TARGET"
else
  echo "[warn] neither getent nor nslookup available"
fi

section "Active probe: target ping"
if have ping; then
  run_cmd "ping" ping -n -c "$PING_COUNT" -i "$PING_INTERVAL" "$TARGET"
else
  echo "[warn] ping not available"
fi

section "Active probe: default gateway ping"
if have ip && have ping; then
  GW="$(ip route 2>/dev/null | awk '/^default/ {print $3; exit}')"
  if [ -n "$GW" ]; then
    run_cmd "ping gateway $GW" ping -n -c 5 -i 0.2 "$GW"
  else
    echo "[warn] default gateway not found"
  fi
else
  echo "[warn] gateway probe skipped (missing ip or ping)"
fi

section "TCP/IP counters"
show_or_warn nstat nstat -az
show_or_warn netstat netstat -s
show_or_warn cat cat /proc/net/snmp
show_or_warn cat cat /proc/net/netstat

section "Socket detail snapshot"
show_or_warn ss ss -tin
show_or_warn ss ss -uan

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
- Compare target ping vs gateway ping to separate LAN from upstream problems.
- Packet loss in ping with clean local counters usually points upstream.
- Rising TCP retransmits plus stable RTT often indicates congestion or shaping.
- Errors/drops on local interfaces suggest NIC, driver, or local queue pressure.
- Route flaps or changing next-hops can cause intermittent failures.
- Collect two or more runs during good vs bad periods for comparison.
CHECKLIST
