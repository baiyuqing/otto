# Network metrics cheatsheet

## Core signals

- **Packet loss (ping/mtr)**
  - Sustained loss >1% is usually user-visible for real-time traffic.
  - Burst loss with long quiet periods often maps to queue drops.
- **Latency (RTT)**
  - Higher average RTT with low jitter may indicate longer path.
  - High jitter with occasional spikes suggests congestion or unstable queueing.
- **TCP retransmissions**
  - Increasing retransmit counters during incidents confirms delivery problems.
  - Retransmits with no packet loss in ICMP can still happen due to middleboxes/QoS.
- **Interface errors/drops**
  - RX/TX errors or drops on host interfaces indicate local bottlenecks first.

## Fault-domain hints

- **Likely local host/LAN issue**
  - Interface drops/errors increase.
  - Gateway ping unreliable.
- **Likely WAN/transit issue**
  - Local gateway stable but remote target shows loss/jitter.
  - mtr loss appears at or after ISP edge hops.
- **Likely remote service issue**
  - Path mostly stable until final hop/service subnet.
  - Only one destination impacted while peers are healthy.

## Escalation bundle

Provide:

1. Timestamped triage output from at least two runs.
2. Source host, destination, and test duration.
3. Percent loss, min/avg/max RTT, and retransmit trend.
4. Any route/interface changes observed.
