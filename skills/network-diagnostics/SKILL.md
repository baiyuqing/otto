---
name: network-diagnostics
description: Diagnose packet loss, latency spikes, TCP retransmissions, and unstable network paths; use for intermittent connectivity and escalation-ready network triage.
---

# Network Diagnostics

Use this skill for evidence-driven host and path triage.

## Workflow

1. Run `scripts/network_triage.sh` against an impacted destination.
2. Compare **target ping** and **default-gateway ping**.
3. Review TCP/IP counters and interface errors for local saturation or drops.
4. If needed, run with `-m` to collect hop-level path evidence.
5. Summarize probable fault domain and attach escalation evidence.

## Quick start

```bash
./scripts/network_triage.sh <target-host-or-ip>
```

Optional flags:

- `-c <count>`: ping count (default `20`)
- `-i <seconds>`: ping interval (default `0.2`)
- `-m`: include `mtr` report (if installed)
- `-t <seconds>`: timeout per command (default `5`)

## Interpretation guidance

- Use `references/metrics-cheatsheet.md` for concise heuristics.
- Prioritize trends across repeated runs over single-point spikes.
- Explicitly call out unavailable tools/permissions as confidence limits.
