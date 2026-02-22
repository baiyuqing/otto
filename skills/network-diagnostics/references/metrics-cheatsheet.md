# Network Metrics Cheatsheet

## Signals for packet loss
- `ping` packet loss percentage above 0%.
- `mtr` reports where loss starts at hop N and remains elevated through later hops.
- Interface drop counters (`ip -s link`) that increase between samples.

## Signals for TCP retransmission issues
- `netstat -s` fields like `segments retransmitted` increasing quickly.
- `nstat -az` counters `TcpRetransSegs`, `TcpTimeouts`, or `TCPSynRetrans` growing rapidly.
- Application latency spikes that correlate with retransmission growth.

## Quick differential diagnosis
- **Loss + retransmits + interface drops:** Local host/interface bottleneck likely.
- **Loss + retransmits + clean interface stats:** Upstream path congestion or ISP/transit issue.
- **Retransmits without ICMP loss:** Middleboxes filtering ICMP or microbursts affecting TCP only.
- **Only one destination affected:** Remote service, route, or peering issue.

## Minimal evidence bundle for escalation
1. Two consecutive runs of `network_triage.sh` 5-10 minutes apart.
2. Timestamp, affected source host, and affected destination(s).
3. Any recent network changes (routing, firewall, kernel, NIC driver).
