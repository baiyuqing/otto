---
name: network-diagnostics
description: Diagnose host and path networking issues such as packet loss, TCP retransmissions, and unstable routes. Use when users ask to investigate dropped packets, high latency, retransmits, or intermittent connectivity.
---

# Network Diagnostics

Use this skill when a user needs quick, evidence-driven network triage.

## Workflow

1. Capture baseline host/network state.
2. Run active probes (ping and optional mtr) toward impacted destination(s).
3. Inspect TCP/IP kernel counters for retransmission, drops, and errors.
4. Summarize probable fault domain (local host, gateway/LAN, ISP/transit, remote service).
5. Provide escalation-ready evidence.

## Quick start

From this skill directory, run:

```bash
./scripts/network_triage.sh <target-host-or-ip>
```

Optional arguments:

- `-c <count>`: ping packet count (default: 20)
- `-i <seconds>`: ping interval (default: 0.2)
- `-m`: run `mtr` report if available

## Interpretation guidance

- Start with `references/metrics-cheatsheet.md` when writing conclusions.
- Prioritize patterns over single spikes.
- Highlight missing tools or permissions in the output so users understand confidence limits.
