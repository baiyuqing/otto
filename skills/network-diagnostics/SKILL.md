---
name: network-diagnostics
description: Collect and interpret host-level network evidence for packet loss, TCP retransmissions, and path instability. Use when investigating connectivity degradation, intermittent latency, dropped packets, retransmit spikes, or when preparing escalation-ready diagnostics.
---

# Network Diagnostics

Use this skill to gather reproducible evidence of network issues from a Linux host.

## Run the triage script

1. Execute `scripts/network_triage.sh <target>` from this skill directory.
2. Optionally tune probe volume with `PING_COUNT`, for example `PING_COUNT=60`.
3. Save outputs from repeated runs to compare counter growth over time.

Example:

```bash
cd skills/network-diagnostics
PING_COUNT=60 ./scripts/network_triage.sh 1.1.1.1 | tee triage-$(date +%s).log
```

## Interpret outputs

1. Inspect ping and mtr for persistent packet loss patterns.
2. Inspect `netstat -s` or `nstat -az` for retransmission-related counters.
3. Inspect `ip -s link` for increasing drops/errors on active interfaces.
4. Use `references/metrics-cheatsheet.md` for quick diagnosis and escalation guidance.

## Escalation format

Include:
- Target(s), timeframe, and user impact.
- At least two triage runs showing trend direction.
- Which counters increased (for example retransmits, timeouts, RX drops).
- Any recent infrastructure or host changes.
