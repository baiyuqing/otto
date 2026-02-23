---
name: packet-capture-analysis
description: Capture live network packets with tcpdump and analyze pcap files to diagnose connection failures, retransmissions, handshake problems, DNS issues, and application-layer anomalies. Use when investigating intermittent connectivity, slow queries, unexpected resets, or when building an evidence bundle for escalation.
---

# Packet Capture & Analysis

Use this skill to capture network traffic with `tcpdump` and analyze the resulting pcap files for common issues.

## Capture packets

1. Execute `scripts/capture.sh` from this skill directory.
2. Customize capture parameters with environment variables (see below).
3. The script writes a timestamped `.pcap` file to the current directory.

Environment variables:

| Variable       | Default              | Description                                   |
|----------------|----------------------|-----------------------------------------------|
| `IFACE`        | any                  | Network interface to capture on               |
| `DURATION`     | 30                   | Capture duration in seconds                   |
| `SNAP_LEN`     | 0 (full packet)      | Bytes per packet to capture (0 = no limit)    |
| `BPF_FILTER`   | *(empty — all traffic)* | BPF filter expression                      |
| `OUTDIR`       | .                    | Directory to write the pcap file              |
| `RING_SIZE`     | 100                  | Max pcap file size in MB (rotate if exceeded) |

Example — capture MySQL traffic on port 3306 for 60 seconds:

```bash
cd skills/packet-capture-analysis
BPF_FILTER="tcp port 3306" DURATION=60 sudo ./scripts/capture.sh
```

Example — capture DNS traffic:

```bash
BPF_FILTER="udp port 53" DURATION=15 sudo ./scripts/capture.sh
```

## Analyze a capture file

1. Execute `scripts/analyze.sh <pcap-file>` from this skill directory.
2. The script produces a structured report covering connection statistics, retransmissions, DNS queries, TCP resets, handshake timing, and top talkers.

Example:

```bash
cd skills/packet-capture-analysis
./scripts/analyze.sh capture-1706000000.pcap | tee analysis-$(date +%s).log
```

## Interpret outputs

1. Check the **TCP handshake summary** for incomplete or slow handshakes.
2. Check the **retransmission summary** for segments retransmitted (sign of loss or congestion).
3. Check the **RST/FIN analysis** for unexpected connection teardowns.
4. Check the **DNS summary** for slow or failing resolutions.
5. Use `references/pcap-cheatsheet.md` for quick diagnosis and filter recipes.

## Escalation format

Include:
- Capture timeframe, interface, and BPF filter used.
- Pcap file and analysis report.
- Which anomalies were found (retransmissions, resets, slow handshakes).
- Correlation with application-level symptoms (latency spikes, errors).
