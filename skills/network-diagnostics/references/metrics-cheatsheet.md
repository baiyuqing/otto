# Network metrics cheatsheet

## Core signals

- **Packet loss (ping/mtr)**
  - Sustained loss >1% is often user-visible for real-time traffic.
  - Loss to gateway implies local/LAN issue before WAN investigation.
- **Latency and jitter**
  - Higher baseline RTT with low jitter may indicate longer route/path change.
  - High jitter and spikes suggest transient queueing or congestion.
- **TCP retransmissions**
  - Rising retransmits during incidents indicate delivery impairment.
  - Retransmits can rise even with low ICMP loss (QoS, middleboxes, asymmetry).
- **Interface errors/drops**
  - RX/TX drops, overruns, or errors on host NICs indicate local contention first.

## Fault-domain hints

- **Likely local host/LAN issue**
  - Gateway ping unstable, or interface drops/errors increase.
  - Multiple remote targets fail similarly.
- **Likely WAN/transit issue**
  - Gateway stable, but remote targets show loss/jitter.
  - mtr degradation begins at/after ISP edge hops.
- **Likely remote service issue**
  - Only one destination/service subnet is degraded.
  - Most path hops are stable until final segment.

## Escalation bundle

Provide:

1. Timestamped outputs from at least two runs (good vs bad window).
2. Source host/region, destination(s), and test duration.
3. Loss %, RTT min/avg/max, gateway-vs-target comparison.
4. Retransmit/error counter trend and any route/interface changes.
