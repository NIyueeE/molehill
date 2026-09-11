<p align="center">
  <img src="assets/molehill.svg" width="257" height="257">
</p>

<h1 align="center">molehill</h1>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/NIyueeE/molehill.svg"></a>
  <img src="https://img.shields.io/github/v/release/NIyueeE/molehill.svg">
  <img src="https://img.shields.io/badge/rust-stable-93450a.svg">
  <img src="https://github.com/NIyueeE/molehill/actions/workflows/ci.yml/badge.svg">
</p>
<p align="center">
  <img src="https://img.shields.io/github/stars/NIyueeE/molehill.svg">
  <img src="https://img.shields.io/github/forks/NIyueeE/molehill.svg">
  <img src="https://img.shields.io/github/last-commit/NIyueeE/molehill.svg">
</p>

<p align="center">A secure, stable, high-performance reverse proxy for NAT traversal.</p>

[English](README.md) | [简体中文](README.zh.md)

molehill, like [frp](https://github.com/fatedier/frp) and [ngrok](https://github.com/inconshreveable/ngrok), can help to expose the service on the device behind the NAT to the Internet, via a server with a public IP.

<!-- TOC -->

- [molehill](#molehill)
  - [Features](#features)
  - [Quickstart](#quickstart)
  - [Deployment](#deployment)
    - [Binary](#binary)
    - [systemd](#systemd)
    - [Container](#container)
  - [Configuration](#configuration)
  - [Documentation](#documentation)
  - [Development](#development)

<!-- /TOC -->

## Features

- **High Performance** Much higher throughput can be achieved than frp, and more stable when handling a large volume of connections.
- **Low Resource Consumption** Consumes much fewer memory than similar tools. [The binary can be](docs/build-guide.md) **as small as ~500KiB** to fit the constraints of devices, like embedded devices as routers.
- **Client-Authoritative Services** Since v0.7 the server needs no per-service configuration: clients declare what to expose (including the public port) and the server enforces an `allow_ports` whitelist. One shared token authenticates everything.
- **Multiplexing** Every data channel rides as a yamux stream over one of N parallel tunnel connections by default (`[client.data].default_count = 4`) — no per-connection handshakes, dramatically fewer file descriptors, throughput beyond a single TCP flow, and head-of-line isolation (a lost segment stalls only its own tunnel). The optional `default_carrier = "kcp"` (feature `kcp`) moves the data plane onto KCP-over-UDP sessions. The `[client.data]` default knobs and the `mode = "direct"` fallback are covered in [Configuration](./docs/configuration.md).
- **Security** A shared token is mandatory and the `allow_ports` whitelist bounds what any client can expose. The optional Noise Protocol encrypts the wire with a single pre-shared X25519 keypair — no PKI, no CA. `plain` forwards unencrypted.
- **Hot Reload** Services can be added or removed dynamically by hot-reloading the configuration file.

## Benchmarks

Single-machine comparison (all on loopback, `visitor -> server -> client ->
backend`); everything is measured **through the tunnel** — iperf3 and the
probes dial each tool's exposed port, never the backend. Peers are the
latest GitHub release builds (frp 0.71.0, rathole 0.5.0 upstream, bore
0.6.0). Network cells (netem on `lo`, every leg affected) and the metric
set are described in [Methodology](#methodology). These are the v0.8.0
numbers (the v0.7.2 baseline ran a single-tunnel default and a different
container; cross-version values are indicative — same-matrix comparisons
are precise).

### Choosing a configuration

The measurements below justify the defaults and tell you when to deviate.

| Config | Use it when | Cost measured |
|---|---|---|
| **`mode = "multiplex"` (default)** | one client exposes **multiple services**, or connections churn (HTTP/game sessions); connection resources matter (FDs, ports, **NAT mappings** — every physical tunnel behind a NAT costs one mapping) | 10.0 Gbit/s single-stream on loopback (19.2 with `mode = "direct"` — one yamux stream is bounded by one tunnel flow), 19.5 at 8 streams; the yamux ceiling caps concurrent connections at `count × 32` (128 at the default `count = 4` — 64 works) |
| **`mode = "direct"`** | one service or a few long-lived streams (SSH); **raw throughput first** (bulk transfers): 19.2/23.3 Gbit/s on loopback | one physical tunnel per stream: FDs/ports/NAT mappings scale with stream count; per-connection setup is real (churn p99 ~3.5 ms at 16-way concurrency) but invisible at `pool_size = 8`; smallest footprint (~15.5 MiB) and lower CPU (~515% vs ~494% at four tunnels, but 216% at one) |
| **`count = 4` (default)** | many concurrent streams, or a lossy path: independent tunnels isolate head-of-line blocking and **aggregate beyond a single flow** | 4 physical connections per service (FDs/ports/NAT mappings) and ~494% CPU against 216% for one tunnel; loopback 8-stream 19.5 vs 9.2 Gbit/s at `count = 1`, 1% loss 12.3 vs 4.5, burst loss 13.3 vs 4.5; the 10 ms HoL max is lower (80.7 vs 100.1 ms) |
| **`count = 1`** | one long-lived stream, a tight connection budget, or the smallest footprint (~16 MiB with half the CPU) | one TCP-flow ceiling; no aggregation (loopback 8-stream 9.2 Gbit/s); every stream shares one retransmit domain |
| **`carrier = "kcp"`** (experimental) | when TCP data tunnels are blocked or throttled, or for **latency-first UDP at high delay** | far behind the TCP carrier wherever the path is not the bottleneck (loopback 8-stream 1.1 vs 14.9 Gbit/s, rtt10 0.79 vs 5.45, loss1 0.71 vs 7.74) at ~2.5-3x RSS (83 vs 26 MiB) and lower CPU; its clearest win is rtt100 session quality (max gap 20 ms vs the TCP arms' 800+) |
| **noise** | encrypted transport wanted with **memory and simplicity first**: a pre-shared public key and no PKI | ~58% of single-stream and ~76% of 8-stream plain throughput (5.8/14.9 vs 10.0/19.5 Gbit/s), sub-millisecond RTT, ~4 MiB more RSS; CPU a wash (470% vs 494% of one core) |

How to apply each choice: the `[client.data]` block holds the per-client
defaults, and every service can override `mode`/`count`/`carrier` on its
own `[client.services.<name>]` block — one client can mix a multiplexed
interactive service with a `direct` bulk service, and can even point
individual services at different molehill servers via `remote_addr` (the
server adapts per connection, no server-side change). The `[transport]`
block is in [Configuration](docs/configuration.md); Noise keypairs in
[Transport](docs/transport.md); ready-to-run configs in
[Quickstart](#quickstart) and [Complete examples](./docs/configuration.md#complete-examples).

**How to choose, step by step.** Start from the defaults
(`multiplex`, `count = 4`, `carrier = "tcp"`, plain transport) and answer
three questions about your workload; change one thing at a time and
re-test:

1. **Do you need encryption?** Yes → set `[client.transport] type =
   "noise"` and place the keys (cost: ~42% of single-stream throughput,
   5.8 vs 10.0 Gbit/s, and ~24% at 8 streams; irrelevant below ~2 Gbit/s
   needs; sub-millisecond RTT, ~4 MiB RSS). No → keep `"plain"`.
2. **One user or many, and how many concurrent connections?** A single
   long-lived session (SSH, one Minecraft player) → `direct` or the
   default mux both work; mux saves NAT mappings at low concurrency too.
   Many users / churn / multiple services → keep or raise `count`
   (each tunnel carries ~32 concurrent connections before the yamux
   ceiling — `count = 8` ≈ 256).
3. **What does the path look like, and do you forward UDP?** If TCP data
   tunnels are blocked or throttled, or you need latency-first UDP at high
   delay, A/B `carrier = "kcp"` (its rtt100 session max gap is 20 ms against
   the TCP arms' 800+). Otherwise keep the TCP carrier: the UDP ladder and
   head-of-line probes show no reproducible UDP-under-load penalty for the
   default in our cells (a 100% paced-pinger loss seen in two runs came back
   as 2% in a third). For lossy/wifi paths keep `count >= 4` — it aggregates
   (1% loss 8-stream 12.3 vs 4.5 Gbit/s) and keeps the 10 ms HoL max
   lower — and pick `count` for the per-tunnel connection ceiling
   (`count = 1 -> 32` connections, `count = 4 -> 128`).

Validate with the exposure you care about: `ping`/in-game feel for
latency, `iperf3` on the exposed port for raw throughput, and the
exposed-service behavior under your real traffic. For development,
`just bench-fast` runs a ~2-minute molehill-only matrix for A/B-ing
configurations on this machine.

### molehill vs plain-TCP peers

Plain-TCP axis only (mux on, no encryption): encrypted competitors such as
chisel's SSH tunnel are not comparable here — molehill's own encrypted rows
are isolated below.

![Benchmark: molehill 0.8.0 vs plain-TCP peers](assets/benchmark-v0.8.0.png)

| Tool | 1-stream | 8-stream | echo RTT p50 | Memory |
|---|---|---|---|---|
| **molehill (mux)** | 10.0 | 19.5 | 0.266 ms | 21.8 MiB |
| rathole 0.5.0 | 12.2 | **21.4** | 0.240 ms | 21.1 MiB |
| bore 0.6.0 | **13.8** | 20.5 | 0.482 ms | **10.6 MiB** |
| frp 0.71.0 | 4.6 | 6.2 | 0.391 ms | 68.9 MiB |

The multiplexed client sits mid-pack single-stream (10.0 Gbit/s against
rathole's 12.2 and bore's 13.8) and within ~10% of rathole at 8 streams
(19.5 vs 21.4) while beating frp (6.2); memory is second-lightest (bore
10.6 MiB, frp 68.9). At the 10 ms cell every tool tracks ~101 ms echo RTT
(bore pays 142 ms — extra round trips per connect) and the molehill-only
100 ms cell holds ~1001 ms. In the 1%-loss cell the group lands at
3.8-4.2 Gbit/s (frp 0.8). The shaped cells converge on the configured link
rate (see Methodology).

### molehill: multiplexing cost (mux vs mux-off)

One variable (multiplexing on/off), loopback:

![Multiplexing cost](assets/benchmark-mux-v0.8.0.png)

| Cell | mux 1-str | mux-off 1-str | mux 8-str | mux-off 8-str |
|---|---|---|---|---|
| loopback | 10.0 | 19.2 | 19.5 | 23.3 |

Single-stream shows the mux cost (10.0 vs 19.2 Gbit/s): one yamux stream is
bounded by one tunnel flow. At 8 streams the per-connection architecture
stays ahead here (23.3 vs 19.5) — the default tunnels' value is connection
resources and head-of-line isolation under loss, not raw aggregation
against direct mode (count axis below); mux-off only runs loopback by
design.

### molehill: transport cost (mux vs noise)

One variable (encryption), mux on for both:

![Transport cost](assets/benchmark-transport-v0.8.0.png)

| Configuration | 1-stream | 8-stream | echo RTT p50 | Memory |
|---|---|---|---|---|
| **mux (plain)** | 10.0 | 19.5 | 0.266 ms | 21.8 MiB |
| noise | 5.8 | 14.9 | 0.281 ms | 25.9 MiB |

Noise retains ~58% of single-stream and ~76% of 8-stream throughput with a
sub-millisecond RTT cost and ~4 MiB of extra RSS; in the weak cells the
encrypted row tracks the plain row. The in-repo cipher work (ring-accelerated
ChaChaPoly, batched datagram IO for the KCP carrier) narrowed but did not
remove this cost.

### molehill: tunnel count (`count = 4` vs `count = 1`)

One variable (the number of parallel tunnel connections), plain transport,
everything else at the default:

![Tunnel count](assets/benchmark-count-v0.8.0.png)

| Cell | c4 1-str | c1 1-str | c4 8-str | c1 8-str | c4 HoL max | c1 HoL max |
|---|---|---|---|---|---|---|
| loopback | 10.0 | 9.6 | 19.5 | 9.2 | 33.4 | 33.4 |
| rtt10 | 6.4 | 6.2 | 7.2 | 5.7 | 80.7 | 100.1 |
| rtt100 | 0.569 | 0.611 | 1.3 | 1.4 | 801.0 | 807.9 |
| loss1_rtt10 | 4.2 | 4.3 | 12.3 | 4.5 | 287.7 | 289.6 |
| loss5_rtt100 | 0.217 | 0.232 | 0.826 | 0.330 | 1627.3 | 2456.2 |
| loss2b25_rtt10 | 3.9 | 4.0 | 13.3 | 4.5 | 320.9 | 317.6 |
| rate100_rtt20 | 0.0356 | 0.0395 | 0.0381 | 0.0355 | 201.7 | 746.4 |
| rate20_rtt40 | 0.0089 | 0.0078 | - | - | 2673.8 | 3015.3 |
| jitter20_10 | 2.4 | 2.5 | 4.1 | 4.0 | 162.2 | 159.0 |

Independent tunnels aggregate concurrent streams where one flow cannot
(loopback 8-stream 19.5 vs 9.2 Gbit/s; 1% loss 12.3 vs 4.5; burst loss 13.3
vs 4.5) and keep the 10 ms HoL max lower (80.7 vs 100.1 ms). Under
sustained loss the shared retransmit domain shows (loss5 HoL 1627 vs 2456 ms
in `count = 4`'s favour here, but rate100 202 vs 746 ms against it) — the
HoL maxima are noisy, the aggregation is the stable effect.
`rate20_rtt40` reports 1-stream only: its 8-stream slot times out with the
reason recorded in `partial_metrics` (see Methodology).

### molehill: data-plane carrier (`carrier = "tcp"` vs `"kcp"`)

One variable (what carries the data channels), noise control channel,
`count = 4` for both:

![Data-plane carrier](assets/benchmark-carrier-v0.8.0.png)

| Cell | tcp 1-str | kcp 1-str | tcp 8-str | kcp 8-str | tcp HoL max | kcp HoL max | tcp RSS | kcp RSS |
|---|---|---|---|---|---|---|---|---|
| loopback | 5.8 | 3.7 | 14.9 | 1.1 | 33.4 | 33.4 | 25.9 | 83.0 |
| rtt10 | 4.14 | 0.407 | 5.45 | 0.785 | 80.7 | 81.1 | 18.8 | 64.3 |
| rtt100 | 0.656 | 0.080 | 1.17 | - | 801.4 | 801.1 | 18.5 | 43.4 |
| loss1_rtt10 | 3.76 | 0.456 | 7.74 | 0.710 | 285.1 | 289.0 | 23.6 | 68.4 |
| loss5_rtt100 | 0.247 | 0.059 | 1.06 | 0.078 | 1210.5 | 1074.8 | 19.5 | 45.4 |
| loss2b25_rtt10 | 3.49 | 0.408 | 8.71 | 0.794 | 288.5 | 509.4 | 29.8 | 60.5 |
| rate100_rtt20 | 0.0358 | 0.0366 | 0.0178 | - | 265.1 | 234.5 | 20.0 | 42.0 |
| rate20_rtt40 | 0.0097 | 0.0068 | - | - | 3015.2 | 1297.3 | 19.1 | 24.8 |
| jitter20_10 | 2.13 | 0.155 | 3.15 | 0.273 | 145.3 | 151.8 | 22.9 | 60.2 |

KCP-over-UDP stays far behind the TCP carrier wherever the path is not the
bottleneck (loopback 1.1 vs 14.9 Gbit/s at 8 streams; rtt10 0.79 vs 5.45;
loss1 0.71 vs 7.74) at ~2.5-3x the RSS (83 vs 26 MiB on loopback) — the
2048/4096-segment ARQ windows — while at the shaped rate cells both
carriers sit on the ceiling. Its defensible uses are a **UDP-only path**
(TCP blocked or throttled) and latency-first UDP at high delay (rtt100
session max gap 20 ms against the TCP arms' 800+).

### Configuration tradeoffs (loopback)

![Configuration tradeoffs](assets/benchmark-cost-v0.8.0.png)

| Tool | CPU% | churn/s | churn p99 ms | RSS MiB | thr64 | mixed bulk |
|---|---|---|---|---|---|---|
| **mux** | 494.1 | 4987.0 | 3.56 | 21.8 | 14.91 | 11.52 |
| mux-off | 514.8 | 5022.7 | 3.50 | 15.5 | 18.04 | 21.08 |
| noise | 470.7 | 5005.0 | 3.53 | 25.9 | 12.97 | 5.80 |
| mux1 | 216.0 | 5030.0 | 3.52 | 16.0 | - | 8.81 |
| kcp4 | 299.2 | 4823.0 | 3.73 | 83.0 | 6.99 | 1.30 |

Every mode sustains ~4.8-5.0k connections/s under churn (setup-to-first-byte
p99 ~3.5-3.7 ms — the pool absorbs per-connection setup). CPU tracks the
tunnel count (one tunnel 216% of one core, four ~471-515%) and memory
separates mux-off/mux1 (~16 MiB) from mux (22) and KCP (83). The 64-stream
point is a working-point reference (14.9 Gbit/s at the default, 18.0
direct); `mux1` has no point by design (64 > its `count × 32` ceiling). The
mixed workload keeps 11.5 Gbit/s while sharing the client with an
interactive service (21.1 direct, 1.3 on KCP).

### Methodology

- **Setup**: everything on one machine's loopback; the four hops
  (visitor, server, client, backend) are processes on the same host, so
  absolute numbers are host-dependent — comparisons are same-host and
  same-methodology only.
- **Cells**: loopback, rtt10, rtt100, loss1%, loss5%, loss2%-burst, and
  rate-limited (r100/20, r20/40 — a bottleneck uplink) and jittery
  (j20/10) cells — netem shapes the whole `lo`, so every hop is
  delayed/lossy; a "10 ms" cell shows ~100 ms echo RTT because the path is
  multi-leg. Without `CAP_NET_ADMIN`, rtt cells fall back to a userspace
  delay proxy and loss cells are skipped. A rate cell shapes `lo` at the
  configured rate with a `limit 2000`-packet queue (`netem_rate_limit` in
  the meta), so the cell is a floor test rather than a tool ranking: the
  100 Mbit/s cell measures both stream counts (0.036 / 0.040 Gbit/s) and the
  20 Mbit/s cell measures 1-stream (0.008); its 8-stream slot is `null`
  because eight parallel streams cannot finish through that bottleneck
  inside the harness bound — the timeout reason is in `partial_metrics`.
  The plain-TCP peers run a lean subset (loopback, rtt10, 1% loss and the
  two rate cells); the molehill-vs-peers chart therefore plots only those
  cells, while the pure-delay, 5%-loss, burst-loss and jitter cells are
  molehill-only stories told by the count and carrier charts.
- **Metrics**: TCP throughput (1/8/64 streams — 64 is the working point
  below the yamux ceiling of `count × 32` concurrent connections; the
  headline is the sender's bytes over the **measured window** (falling back
  to the receiver's count when a fast sender's writes were all absorbed by
  the `-O` warm-up and backpressure blocked the measured window — recorded
  with the raw numbers), with the receiver's own drain-inclusive window
  next to it, plus the median rep's retransmits, the min/max spread and
  per-stream bytes),
  connection-path RTT (fresh connects, up to 300 samples bounded at 20 s
  wall), steady data-path RTT (pings on one connection, likewise bounded
  at 20 s), **connection churn** (short-connection storm at 16 concurrent
  connectors: connects/sec and setup-to-first-byte p50/p99 — the
  mux-vs-direct and pool guidance data), UDP session quality
  (RTT/loss/jitter/max gap over one session) and a two-point UDP
  pacing probe whose raw `offered`/`delivered`/loss numbers are recorded
  but deliberately not charted or claimed (the probe is being redesigned —
  see Known limits), a head-of-line probe (saturating bulk + game-like pinger through one service), a
  **mixed workload** (iperf bulk + interactive latency through two
  services of the same client simultaneously — per-service override
  guidance), **CPU%** of server+client (the noise/KCP tradeoff cost) and
  RSS (0.5 s sampling).
- **Discipline**: every comparison varies **one variable** (plain-TCP axis,
  multiplexing on/off, transport) with a shared control; arms run
  **serially** in isolated port bands with fresh processes (parallel runs
  would compete for CPU and invalidate the numbers); molehill arms run 3
  reps, peers 1; the matrix self-throttles (nice 10, and every arm waits
  for the load average to fall below 70% of the core count before
  starting) so each arm begins on a quiet machine and a long run cannot
  freeze the host.
- **Known limits**: loopback is not a real network (idealized loss/delay,
  no real congestion); the shared qdisc dilutes drops — the UDP loss column
  is the residual share a light session sees, not the configured rate; the
  `pool_size=8` connection pool hides per-connect handshake costs
  (the churn metric now shows the residual cost); connection-resource
  metrics (FDs, NAT mappings, TIME_WAIT) — mux's main benefit — are not
  measured; rate cells need netem with `rate` support (modern iproute2).
  A rate cell shapes `lo` with a `limit 2000`-packet queue (recorded in the
  meta as `netem_rate_limit`): the depth is a measurement parameter, and a
  shallow queue tail-drops whole GSO segments, which costs ~80% of the
  shaped rate for reasons that belong to the shaper rather than the tool.
  Each throughput sample bounds its iperf3 client at least as loosely as the
  historical `secs + 20` and replaces the single-test iperf3 server after a
  stalled repetition, so one wedge cannot starve the rest of the repetition
  budget. Every `null` in the results file carries the typed reason that
  produced it in `partial_metrics`, and `audit_results.py` refuses a `null`
  without one. **Method and host bound comparability:** these rows are one
  same-host run of the revised method (schema v3), so older rows and the
  v0.7.2 gate are informational only (docs/release.md).
  **A throughput number is only valid for the endpoint it dialed.** The
  harness samples the tool's exposed port; an earlier revision of this
  revision dialed the iperf3 backend by mistake and reported the loopback
  ceiling (~46 Gbit/s) for every tool with the tunnel bypassed. The entries
  now record `_throughput_exposed_port`/`_bench_backend_port`, the sampler
  raises when they are equal, and the audit fails such a run (AGENTS.md
  §10).
  **UDP characterisation.** The UDP probe walks a ladder of paced short
  bursts (500 pps to the configured burst rate, 2000-datagram bursts, a
  1.5 s drain) and reports the highest step delivered within
  `max(2%, cell loss + 2pp)`. On unshaped loopback every step arrives, so
  the figure is a lower bound (27.2 Mbit/s, the ladder's top); the 10 ms
  cell bends at 12 000 pps (10.9 Mbit/s delivered) and the 100 ms cell at
  1 000-2 000 pps (0.7-1.4 Mbit/s); in the loss cells no step is within
  tolerance, so the knee plus its delivered rate is the informative pair.
  An earlier two-point version reported its own pace back and was not
  charted; this ladder is what the results file records now. The head-of-line
  probe's pinger loss was 100% for the default arms in two runs and 2% in a
  third, so **a UDP-under-load weakness is NOT claimed**: it does not
  reproduce (variance, not a path property).
  An arm whose yamux ceiling (`count × 32`) is below the
  64-stream scale point skips that probe by design (reason in
  `partial_metrics`). Charts plot only the
  rows and cells that take part in a comparison (a peer-less cell or a
  structurally skipped probe is not drawn at all); a value missing inside a
  compared panel is a grey `x`, while a measured zero is labelled `0` so
  zero and absent stay distinct. A HoL pinger that gets zero or one reply
  records a stall (the elapsed wait) rather than `null`. Raw per-rep iperf3
  JSON for every throughput sample is kept under the run's work directory
  (`iperf-raw/`), so a surprising number can be re-diagnosed.
- **Reproduce**: `just bench-peers` → `just bench` → `just bench-plot` →
  `just bench-check` (raw data in `benches/scripts/bench/results-v0.8.0.json`;
  ritual and regression gate in docs/release.md).

## Quickstart

A full-powered `molehill` can be obtained from the [release](https://github.com/NIyueeE/molehill/releases) page. Or [build from source](docs/build-guide.md) **for other platforms and minimizing the binary**.

Like frp, all service definitions live on the client side; the server only sets policy (a shared token and the `allow_ports` whitelist).

To use `molehill`, you need a server with a public IP, and a device behind the NAT, where some services that need to be exposed to the Internet.

Assuming you have a NAS at home behind the NAT, and want to expose its ssh service to the Internet:

1. On the server which has a public IP

Create `server.toml` with the following content and accommodate it to your needs.

```toml
# server.toml
[server]
default_token = "use_a_secret_that_only_you_know" # Shared secret with your clients # security-scan:allow documentation placeholder

# Master switch for dynamic registrations: only ports covered here can be claimed by clients
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333" # Port that molehill listens for clients
```

Then run:

```bash
./molehill server.toml
```

2. On the host which is behind the NAT (your NAS)

Create `client.toml` with the following content and accommodate it to your needs.

```toml
# client.toml
[client]
default_token = "use_a_secret_that_only_you_know" # Must match the server's `default_token` # security-scan:allow documentation placeholder

[client.control]
default_remote_addr = "myserver.com:2333" # The address of the server. The port must match `server.control.bind_addr`

[client.services.my_nas_ssh]
local_addr = "127.0.0.1:22" # The local service to forward
remote_bind_addr = "0.0.0.0:5202" # The public port to expose it at on the server
```

At startup the client registers `my_nas_ssh` on the server, which validates
the port against `allow_ports` and starts forwarding. Adding or removing
services in `client.toml` applies live via hot reload — no server-side edit
needed.

Then run:

```bash
./molehill client.toml
```

3. Now the client will try to connect to the server `myserver.com` on port `2333`, and any traffic to `myserver.com:5202` will be forwarded to the client's port `22`.

So you can `ssh -p 5202 myserver.com` to ssh to your NAS.

To run `molehill` as a background service on Linux, checkout the
[systemd units](./docs/configuration.md#systemd) or the
[container deployments](./docs/configuration.md#container).

## Configuration

`molehill` determines the running mode (server/client) from the config file
automatically, or you can force it with `--server` / `--client`. The full
configuration specification, logging and tuning options are documented in
[Configuration](./docs/configuration.md), which also includes
[complete examples](./docs/configuration.md#complete-examples) for various
scenarios.

## Deployment

### Binary

Download a pre-built binary for your platform from the
[release page](https://github.com/NIyueeE/molehill/releases), or
[build from source](./docs/build-guide.md) for other platforms and
minimal-sized binaries.

```bash
./molehill server.toml   # on the public server
./molehill client.toml   # on the device behind NAT
```

### systemd

The [systemd units](./docs/configuration.md#systemd) show how to run molehill as a
systemd service, both as root and rootless, including multiple instances.

### Container

Official multi-arch images (linux/amd64, linux/arm64) are published to
`ghcr.io/niyueee/molehill`. The image is a single static musl binary on
`scratch` (~1.2 MiB), runs as non-root UID 1000, and includes the same default
feature set as the regular release builds (multiplexing and the `kcp` carrier
included).

```bash
docker run -v /etc/molehill/server.toml:/app/server.toml:ro \
  ghcr.io/niyueee/molehill:latest server.toml
```

The image contains no configuration — mount your config file and pass its
name as the argument. Two container-specific notes: the process runs as UID
1000 (so mount the config world-readable, and prefer ports ≥ 1024), and under
bridge networking a `carrier = "kcp"` service needs its data-plane port
published over **UDP** as well. See the [container deployments](./docs/configuration.md#container)
for Docker Compose (`compose.yaml` / `compose.bridge.yaml`) and Podman
Quadlet (`molehill-server.container` / `molehill-client.container`)
deployments.

## Documentation

Using molehill:

- [Configuration](./docs/configuration.md) — full configuration specification, logging, tuning
- [Transport](./docs/transport.md) — Noise Protocol setup
- [Build guide](./docs/build-guide.md) — build customization, minimal binary
- [Internals](./docs/internals.md) — how control/data channels work
- [Configuration examples](./docs/configuration.md#complete-examples) — configs for common scenarios (systemd & container deployments included)

Contributing & engineering:

- [Checks](./docs/checks.md) — what every gate runs, how to handle a block
- [Lint policy](./docs/lint-policy.md) — lint levels and waiver rules
- [Release](./docs/release.md) — release mechanics, versioning, test builds
- [Structure](./docs/structure.md) — what every file in this repo is for
- [Contributing](./CONTRIBUTING.md) — setup and workflow
- [Security](./SECURITY.md) — reporting vulnerabilities
- [`HANDOFF.md`](./HANDOFF.md) — current working state; planned work and future design documents

## Development

molehill is written in Rust (2024 edition); `rust-toolchain.toml` declares
`channel = "stable"` with clippy and rustfmt components — never hardcode a
version. Layered git hooks guard every commit, push, and release tag, and CI
runs the identical chain:

```bash
just setup   # activate git hooks (core.hooksPath githooks) + install check tools
just check   # fmt / secrets / machete / docs / clippy + audit / deny / outdated / test
just tag     # release review (githooks/pre-tag) + create the local v* tag
```

molehill began as a fork of [rathole](https://github.com/rapiz1/rathole)
(Apache-2.0) and has been developed independently since; the upstream
history is preserved below the fork point and the version line continues
from there (upstream's last release was v0.5.0). See
[docs/release.md](./docs/release.md) for release mechanics and
[AGENTS.md](./AGENTS.md) for the repository rules.
