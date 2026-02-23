# Packet Capture & Analysis Cheatsheet

## Useful tcpdump BPF filters

```bash
# MySQL traffic
tcp port 3306

# DNS traffic
udp port 53 or tcp port 53

# HTTP/HTTPS traffic
tcp port 80 or tcp port 443

# Traffic to/from a specific host
host 10.0.0.1

# Traffic on a subnet
net 10.0.0.0/24

# Only TCP SYN packets (new connections)
'tcp[tcpflags] & (tcp-syn) != 0 and tcp[tcpflags] & (tcp-ack) == 0'

# Only TCP RST packets
'tcp[tcpflags] & (tcp-rst) != 0'

# ICMP unreachable
'icmp[icmptype] == 3'

# Combine: MySQL traffic with packet size > 500 bytes
'tcp port 3306 and greater 500'
```

## Useful tshark display filters

```bash
# TCP retransmissions
tshark -r file.pcap -Y "tcp.analysis.retransmission"

# Slow TCP handshakes (initial RTT > 100ms)
tshark -r file.pcap -Y "tcp.analysis.initial_rtt > 0.1"

# Zero window events
tshark -r file.pcap -Y "tcp.analysis.zero_window"

# DNS query failures
tshark -r file.pcap -Y "dns.flags.rcode != 0"

# MySQL protocol errors (requires MySQL dissector)
tshark -r file.pcap -Y "mysql.error_code != 0"

# All RST packets with conversation context
tshark -r file.pcap -Y "tcp.flags.reset == 1" -T fields -e ip.src -e ip.dst -e tcp.srcport -e tcp.dstport

# Connection duration (FIN timing)
tshark -r file.pcap -q -z conv,tcp
```

## Quick diagnosis patterns

### Connection refused
- **Symptom:** SYN followed immediately by RST-ACK from target.
- **Cause:** No process listening on target port, or explicit reject rule.
- **Verify:** Check target `ss -tlnp` for listening sockets.

### Connection timeout
- **Symptom:** Multiple SYN retransmissions with no SYN-ACK.
- **Cause:** Firewall silently dropping, host unreachable, or ARP failure.
- **Verify:** Check firewall rules, routing table, ARP cache.

### Slow queries / high latency
- **Symptom:** Large time gap between request and response in TCP stream.
- **Cause:** Server-side processing delay, lock contention, or disk I/O.
- **Verify:** Correlate with server-side slow query log and system metrics.

### Intermittent packet loss
- **Symptom:** Retransmissions and duplicate ACKs scattered through capture.
- **Cause:** Congested link, faulty NIC/cable, or overloaded switch buffer.
- **Verify:** Check `ip -s link` counters and switch interface stats.

### Connection reset mid-stream
- **Symptom:** RST from one side after established data transfer.
- **Cause:** Application crash, idle timeout (load balancer/firewall), or TCP keepalive failure.
- **Verify:** Check application logs, LB idle timeout settings.

### DNS resolution failure
- **Symptom:** DNS query with NXDOMAIN or SERVFAIL response, or no response.
- **Cause:** Missing DNS record, resolver misconfiguration, or upstream DNS issue.
- **Verify:** `dig` / `nslookup` directly, check `/etc/resolv.conf`.

### Receiver overwhelmed
- **Symptom:** TCP zero-window advertisements from receiver.
- **Cause:** Application not reading from socket fast enough (CPU, GC pauses, blocking I/O).
- **Verify:** Check application CPU/memory, thread dumps, GC logs.

## Minimal evidence bundle for escalation

1. The `.pcap` file covering the incident window.
2. Output of `analyze.sh` highlighting specific anomalies.
3. Timestamp range, affected source/destination IPs, and ports.
4. Application-level error logs correlated to the same time window.
5. Any recent infrastructure changes (network, firewall, kernel, application deploy).
