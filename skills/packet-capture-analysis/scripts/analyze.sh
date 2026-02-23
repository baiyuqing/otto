#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# analyze.sh — Analyze a pcap file and produce a structured diagnostic report
#
# Usage:  ./scripts/analyze.sh <pcap-file>
#
# Requires: tcpdump (mandatory), tshark (optional, enables deeper analysis)
# ---------------------------------------------------------------------------

PCAP="${1:-}"
REPORT_TIME="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

have_cmd() {
  command -v "$1" >/dev/null 2>&1
}

section() {
  printf '\n===== %s =====\n' "$1"
}

# --- Pre-flight checks -----------------------------------------------------

if [[ -z "${PCAP}" ]]; then
  echo "Usage: $0 <pcap-file>" >&2
  exit 1
fi

if [[ ! -f "${PCAP}" ]]; then
  echo "ERROR: File not found: ${PCAP}" >&2
  exit 1
fi

if ! have_cmd tcpdump; then
  echo "ERROR: tcpdump is required but not found." >&2
  exit 1
fi

HAS_TSHARK=false
if have_cmd tshark; then
  HAS_TSHARK=true
fi

echo "Packet capture analysis report"
echo "==============================="
echo "Generated: ${REPORT_TIME}"
echo "File:      ${PCAP}"
FILE_SIZE="$(du -h "${PCAP}" | cut -f1)"
echo "Size:      ${FILE_SIZE}"
echo "tshark:    ${HAS_TSHARK}"

# ===== Section 1: Capture overview ==========================================

section "Capture overview"
TOTAL_PACKETS="$(tcpdump -nn -r "${PCAP}" 2>/dev/null | wc -l)"
echo "Total packets: ${TOTAL_PACKETS}"

if [[ "${TOTAL_PACKETS}" -eq 0 ]]; then
  echo "The capture file contains no packets. Nothing to analyze."
  exit 0
fi

FIRST_TS="$(tcpdump -nn -r "${PCAP}" -c 1 2>/dev/null | awk '{print $1}')"
LAST_TS="$(tcpdump -nn -r "${PCAP}" 2>/dev/null | tail -1 | awk '{print $1}')"
echo "First packet timestamp: ${FIRST_TS}"
echo "Last packet timestamp:  ${LAST_TS}"

# ===== Section 2: Protocol breakdown ========================================

section "Protocol breakdown"
if $HAS_TSHARK; then
  tshark -r "${PCAP}" -q -z io,phs 2>/dev/null || echo "tshark protocol hierarchy failed"
else
  echo "Protocol counts (heuristic, from tcpdump flags):"
  TCP_COUNT="$(tcpdump -nn -r "${PCAP}" 'tcp' 2>/dev/null | wc -l)"
  UDP_COUNT="$(tcpdump -nn -r "${PCAP}" 'udp' 2>/dev/null | wc -l)"
  ICMP_COUNT="$(tcpdump -nn -r "${PCAP}" 'icmp' 2>/dev/null | wc -l)"
  ARP_COUNT="$(tcpdump -nn -r "${PCAP}" 'arp' 2>/dev/null | wc -l)"
  echo "  TCP:  ${TCP_COUNT}"
  echo "  UDP:  ${UDP_COUNT}"
  echo "  ICMP: ${ICMP_COUNT}"
  echo "  ARP:  ${ARP_COUNT}"
fi

# ===== Section 3: Top talkers (by IP) ======================================

section "Top talkers (source IP)"
tcpdump -nn -r "${PCAP}" 2>/dev/null \
  | awk '{print $3}' \
  | sed 's/\.[0-9]*:$//' \
  | sort | uniq -c | sort -rn | head -10

section "Top talkers (destination IP)"
tcpdump -nn -r "${PCAP}" 2>/dev/null \
  | awk '{print $5}' \
  | sed 's/:$//' | sed 's/\.[0-9]*$//' \
  | sort | uniq -c | sort -rn | head -10

# ===== Section 4: TCP conversation summary ==================================

section "TCP conversations"
if $HAS_TSHARK; then
  tshark -r "${PCAP}" -q -z conv,tcp 2>/dev/null | head -30 || echo "tshark conversations failed"
else
  echo "Unique TCP source:port -> destination:port pairs (top 15):"
  tcpdump -nn -r "${PCAP}" 'tcp' 2>/dev/null \
    | awk '{print $3, "->", $5}' \
    | sed 's/:$//' \
    | sort | uniq -c | sort -rn | head -15
fi

# ===== Section 5: TCP handshake analysis (SYN / SYN-ACK) ===================

section "TCP handshake analysis"
SYN_COUNT="$(tcpdump -nn -r "${PCAP}" 'tcp[tcpflags] & (tcp-syn) != 0 and tcp[tcpflags] & (tcp-ack) == 0' 2>/dev/null | wc -l)"
SYN_ACK_COUNT="$(tcpdump -nn -r "${PCAP}" 'tcp[tcpflags] & (tcp-syn) != 0 and tcp[tcpflags] & (tcp-ack) != 0' 2>/dev/null | wc -l)"
echo "SYN packets (connection attempts):  ${SYN_COUNT}"
echo "SYN-ACK packets (accepted):         ${SYN_ACK_COUNT}"
if [[ "${SYN_COUNT}" -gt 0 ]]; then
  UNANSWERED=$((SYN_COUNT - SYN_ACK_COUNT))
  echo "Unanswered SYNs:                    ${UNANSWERED}"
  if [[ "${UNANSWERED}" -gt 0 ]]; then
    echo "  WARNING: ${UNANSWERED} SYN(s) received no SYN-ACK. Possible causes:"
    echo "    - Target not listening on that port"
    echo "    - Firewall dropping SYNs"
    echo "    - Server SYN backlog full"
  fi
fi

if $HAS_TSHARK; then
  echo ""
  echo "Handshake round-trip times (first 10 connections):"
  tshark -r "${PCAP}" -Y "tcp.flags.syn==1 && tcp.flags.ack==1" \
    -T fields -e ip.src -e ip.dst -e tcp.srcport -e tcp.dstport -e tcp.analysis.initial_rtt \
    2>/dev/null | head -10 || echo "  (handshake RTT extraction not available)"
fi

# ===== Section 6: Retransmission analysis ===================================

section "TCP retransmissions"
if $HAS_TSHARK; then
  RETRANS="$(tshark -r "${PCAP}" -Y "tcp.analysis.retransmission" 2>/dev/null | wc -l)"
  FAST_RETRANS="$(tshark -r "${PCAP}" -Y "tcp.analysis.fast_retransmission" 2>/dev/null | wc -l)"
  DUP_ACKS="$(tshark -r "${PCAP}" -Y "tcp.analysis.duplicate_ack" 2>/dev/null | wc -l)"
  SPURIOUS="$(tshark -r "${PCAP}" -Y "tcp.analysis.spurious_retransmission" 2>/dev/null | wc -l)"
  echo "Retransmissions:         ${RETRANS}"
  echo "Fast retransmissions:    ${FAST_RETRANS}"
  echo "Duplicate ACKs:          ${DUP_ACKS}"
  echo "Spurious retransmissions: ${SPURIOUS}"
  if [[ "${RETRANS}" -gt 0 ]]; then
    echo ""
    echo "Top retransmitting flows:"
    tshark -r "${PCAP}" -Y "tcp.analysis.retransmission" \
      -T fields -e ip.src -e ip.dst -e tcp.srcport -e tcp.dstport \
      2>/dev/null | sort | uniq -c | sort -rn | head -10
  fi
else
  echo "(Detailed retransmission analysis requires tshark)"
  echo "Heuristic: looking for likely retransmissions via duplicate seq numbers ..."
  tcpdump -nn -r "${PCAP}" 'tcp' 2>/dev/null \
    | awk '{print $3, $5, $0}' \
    | sort | uniq -d -c | sort -rn | head -10 \
    || echo "  No obvious duplicates detected"
fi

# ===== Section 7: RST and FIN analysis =====================================

section "TCP RST and FIN packets"
RST_COUNT="$(tcpdump -nn -r "${PCAP}" 'tcp[tcpflags] & (tcp-rst) != 0' 2>/dev/null | wc -l)"
FIN_COUNT="$(tcpdump -nn -r "${PCAP}" 'tcp[tcpflags] & (tcp-fin) != 0' 2>/dev/null | wc -l)"
echo "RST packets: ${RST_COUNT}"
echo "FIN packets: ${FIN_COUNT}"

if [[ "${RST_COUNT}" -gt 0 ]]; then
  echo ""
  echo "Top RST sources:"
  tcpdump -nn -r "${PCAP}" 'tcp[tcpflags] & (tcp-rst) != 0' 2>/dev/null \
    | awk '{print $3}' \
    | sed 's/\.[0-9]*:$//' \
    | sort | uniq -c | sort -rn | head -5
fi

# ===== Section 8: DNS analysis ==============================================

section "DNS summary"
DNS_COUNT="$(tcpdump -nn -r "${PCAP}" 'udp port 53 or tcp port 53' 2>/dev/null | wc -l)"
echo "DNS packets: ${DNS_COUNT}"

if [[ "${DNS_COUNT}" -gt 0 ]]; then
  if $HAS_TSHARK; then
    echo ""
    echo "DNS queries (top 15):"
    tshark -r "${PCAP}" -Y "dns.flags.response == 0" \
      -T fields -e dns.qry.name -e dns.qry.type \
      2>/dev/null | sort | uniq -c | sort -rn | head -15

    echo ""
    echo "DNS response codes:"
    tshark -r "${PCAP}" -Y "dns.flags.response == 1" \
      -T fields -e dns.qry.name -e dns.flags.rcode \
      2>/dev/null | sort | uniq -c | sort -rn | head -15

    DNS_ERRORS="$(tshark -r "${PCAP}" -Y "dns.flags.rcode != 0 && dns.flags.response == 1" 2>/dev/null | wc -l)"
    if [[ "${DNS_ERRORS}" -gt 0 ]]; then
      echo ""
      echo "WARNING: ${DNS_ERRORS} DNS response(s) with non-zero rcode (errors/NXDOMAIN)"
    fi
  else
    echo ""
    echo "DNS-related packet samples (first 10):"
    tcpdump -nn -r "${PCAP}" 'udp port 53 or tcp port 53' 2>/dev/null | head -10
  fi
fi

# ===== Section 9: Potential anomalies =======================================

section "Potential anomalies"

# Zero-window
if $HAS_TSHARK; then
  ZERO_WIN="$(tshark -r "${PCAP}" -Y "tcp.analysis.zero_window" 2>/dev/null | wc -l)"
  echo "TCP zero-window events: ${ZERO_WIN}"
  if [[ "${ZERO_WIN}" -gt 0 ]]; then
    echo "  WARNING: Zero-window indicates receiver buffer exhaustion."
  fi

  WINDOW_FULL="$(tshark -r "${PCAP}" -Y "tcp.analysis.window_full" 2>/dev/null | wc -l)"
  echo "TCP window-full events: ${WINDOW_FULL}"

  OUT_OF_ORDER="$(tshark -r "${PCAP}" -Y "tcp.analysis.out_of_order" 2>/dev/null | wc -l)"
  echo "Out-of-order segments:  ${OUT_OF_ORDER}"
fi

# Large ICMP unreachable counts
ICMP_UNREACH="$(tcpdump -nn -r "${PCAP}" 'icmp[icmptype] == 3' 2>/dev/null | wc -l)"
echo "ICMP Destination Unreachable: ${ICMP_UNREACH}"
if [[ "${ICMP_UNREACH}" -gt 0 ]]; then
  echo "  Breakdown:"
  tcpdump -nn -r "${PCAP}" 'icmp[icmptype] == 3' 2>/dev/null \
    | awk '{for(i=1;i<=NF;i++) if($i ~ /unreachable/) print $i}' \
    | sort | uniq -c | sort -rn | head -5
fi

# ===== Interpretation checklist =============================================

cat <<'CHECKLIST'

===== Interpretation checklist =====
1. Unanswered SYNs: target may be down, port blocked, or SYN backlog exhausted.
2. High retransmission count: packet loss, congestion, or receiver not ACKing.
3. RST after SYN-ACK: application rejecting connections or load balancer reset.
4. RST from one side mid-stream: abrupt application shutdown or idle timeout.
5. DNS errors (NXDOMAIN, SERVFAIL): check resolver config and upstream DNS health.
6. Zero-window events: application on receiver side not reading fast enough.
7. Out-of-order segments: path instability or load-balancer ECMP hashing issue.
8. ICMP Unreachable: routing blackhole, ACL, or host/network truly unreachable.
CHECKLIST
