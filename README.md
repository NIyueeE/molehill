<p align="center">
  <img src="https://raw.githubusercontent.com/NIyueeE/molehill/main/assets/molehill.svg" width="81" height="81">
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
| **`mode = "multiplex"` (default)** | one client exposes **multiple services**, or connections churn (HTTP/game sessions); connection resources matter (FDs, ports, **NAT mappings** — every physical tunnel behind a NAT costs one mapping) | single-stream ceiling ~10.9 Gbit/s (19.3 with `mode = "direct"`); the yamux ceiling caps concurrent connections at `count × 32` (128 at the default `count = 4` — 64 works, 128 starts failing); a UDP flood on one service can starve TCP on another (one shared sendq) |
| **`mode = "direct"`** | one service or a few long-lived streams (SSH); **raw throughput first** (bulk transfers): 19.3/28.0 Gbit/s on loopback | one physical tunnel per stream: FDs/ports/NAT mappings scale with stream count; per-connection setup is real (churn p99 ~3.5 ms at 16-way concurrency) but invisible at `pool_size = 8` |
| **`count = 4` (default)** | many concurrent streams, or a lossy path: independent tunnels isolate head-of-line blocking and aggregate beyond a single flow | 4 physical connections per service (FDs/ports/NAT mappings); at 1% loss 8-stream 15.4 vs 4.6 Gbit/s at `count = 1`; under heavy loss the shared retransmit domain shows (loss5 HoL max 1673 vs 1157 ms at `count = 1`) |
| **`count = 1`** | one long-lived stream, or a tight connection budget | one TCP-flow ceiling; every stream shares one retransmit domain |
| **`carrier = "kcp"`** (experimental) | only when TCP data tunnels are blocked or throttled; KCP trades CPU and memory for aggressive loss recovery over UDP | much lower throughput than `carrier = "tcp"` in every cell (loopback 1-str 3.2 vs 4.8 Gbit/s against the noise control; jitter cell 0.151 vs 2.1) and ~4x the RSS (91 vs 24 MiB); its one win: at rtt100 its UDP session quality holds (loss 0%, max gap 20 ms vs the TCP arms' 100+ ms) |
| **noise** | encrypted transport wanted with **memory and simplicity first**: a pre-shared public key and no PKI, at 23.8 MiB | throughput 4.8/14.8 vs 10.9/27.7 Gbit/s plain (1/8 streams) on loopback; RTT cost sub-millisecond; CPU is a wash with plain (both ~550% of one core under full load — the ring-accelerated cipher's cost is hidden by the forwarding path) |

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
   "noise"` and place the keys (cost: ~55% of single-stream throughput,
   irrelevant below ~2 Gbit/s needs; sub-millisecond RTT). No → keep
   `"plain"`.
2. **One user or many, and how many concurrent connections?** A single
   long-lived session (SSH, one Minecraft player) → `direct` or the
   default mux both work; mux saves NAT mappings at low concurrency too.
   Many users / churn / multiple services → keep or raise `count`
   (each tunnel carries ~32 concurrent connections before the yamux
   ceiling — `count = 8` ≈ 256).
3. **What does the path look like?** Pure high latency (100 ms+ RTT,
   e.g. cross-continent) → keep everything default; only for a
   **UDP** interactive service (game) on such a path, A/B-test
   `carrier = "kcp"` — the one regime where the data shows it wins
   (session max-gap 20 ms vs 100+ ms). Lossy/wifi paths → KCP buys
   nothing measurable; `count >= 4` gives you the aggregation and loss
   isolation that matter.

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
| **molehill (mux)** | 10.9 | 27.7 | 0.254 ms | 24.0 MiB |
| rathole 0.5.0 | 10.2 | 27.3 | 0.249 ms | 21.0 MiB |
| bore 0.6.0 | 14.3 | 26.5 | 0.500 ms | **9.8 MiB** |
| frp 0.71.0 | 4.6 | 6.4 | 0.388 ms | 75.3 MiB |

With the `count = 4` default the multiplexed client matches the
per-connection architectures at 8 streams (27.7 vs 26.5-27.3 Gbit/s);
bore stays the lightest and a strong plain TCP relay (no UDP forwarding);
frp pays the highest memory for the lowest throughput. At the 10 ms cell
every tool tracks ~101 ms echo RTT (bore pays extra round trips per
connect — 142 ms), and the molehill-only 100 ms cell holds ~1001 ms (count
chart); at 1% loss molehill, rathole and bore hold 4.2-4.4 Gbit/s while frp
collapses to 0.8. On the shaped-bottleneck cells (rate100/rate20) every
tunnel converges to the same ~1/3-of-cap throughput (0.033 / 0.007 Gbit/s
— the netem shaper delivers about a third of the nominal rate on
loopback), so those cells tell the relative-overhead story, not a tool
ranking.

### molehill: multiplexing cost (mux vs mux-off)

One variable (multiplexing on/off), loopback:

![Multiplexing cost](assets/benchmark-mux-v0.8.0.png)

| Cell | mux 1-str | mux-off 1-str | mux 8-str | mux-off 8-str |
|---|---|---|---|---|
| loopback | 10.9 | 19.3 | 27.7 | 28.0 |

Single-stream shows the mux overhead (10.9 vs 19.3 Gbit/s); at 8 streams
the four default tunnels aggregate to the same ceiling as per-connection
architecture. The real multiplexing cost shows up under loss and concurrency
— see the count axis below (mux-off only runs the loopback cell by design).

### molehill: transport cost (mux vs noise)

One variable (encryption), mux on for both:

![Transport cost](assets/benchmark-transport-v0.8.0.png)

| Configuration | 1-stream | 8-stream | echo RTT p50 | Memory |
|---|---|---|---|---|
| **mux (plain)** | 10.9 | 27.7 | 0.254 ms | 24.0 MiB |
| noise | 4.8 | 14.8 | 0.282 ms | 23.8 MiB |

Noise retains ~44% of the plain 1-stream throughput (4.8 vs 10.9 Gbit/s)
and ~53% at 8 streams (14.8 vs 27.7), with sub-millisecond RTT cost and no
memory change. In weak cells the encrypted row tracks the plain row.

### molehill: tunnel count (`count = 4` vs `count = 1`)

One variable (the number of parallel tunnel connections), plain transport,
everything else at the default:

![Tunnel count](assets/benchmark-count-v0.8.0.png)

| Cell | c4 1-str | c1 1-str | c4 8-str | c1 8-str | c4 HoL max | c1 HoL max |
|---|---|---|---|---|---|---|
| loopback | 10.9 | 9.4 | 27.7 | 9.0 | 33.4 | 33.4 |
| rtt10 | 6.3 | 6.3 | 7.8 | 5.7 | 80.6 | 101.4 |
| rtt100 | 0.668 | 0.684 | 1.1 | 0.93 | 801.4 | 1001.7 |
| loss1_rtt10 | 4.3 | 4.1 | 15.4 | 4.6 | 289.6 | 314.4 |
| loss5_rtt100 | 0.273 | 0.256 | 0.734 | 0.295 | 1673.0 | 1157.4 |
| loss2b25_rtt10 | 3.9 | 4.0 | 14.5 | 4.5 | 349.4 | 288.5 |
| rate100_rtt20 | 0.033 | 0.032 | 0.032 | 0.031 | 210.6 | 689.9 |
| rate20_rtt40 | 0.006 | 0.007 | - | - | 1167.3 | 1311.7 |
| jitter20_10 | 2.5 | 2.6 | 4.6 | 4.2 | 159.1 | 160.2 |

Independent tunnels aggregate concurrent streams beyond one flow (loss1
8-stream 15.4 vs 4.6 Gbit/s) and at the default `count = 4` the yamux
ceiling allows ~128 concurrent connections (`count × 32` per tunnel). The
tradeoff shows at heavy loss: every stream on the 4 tunnels shares the
retransmit domain (loss5 HoL max 1673 ms vs 1157 ms at `count = 1`). The
rate20 cell reports 1-stream only — eight parallel iperf streams wedge the
single-test iperf3 server on the shaped path (see Methodology); rate100
measures both.

### molehill: data-plane carrier (`carrier = "tcp"` vs `"kcp"`)

One variable (what carries the data channels), noise control channel,
`count = 4` for both:

![Data-plane carrier](assets/benchmark-carrier-v0.8.0.png)

| Cell | tcp 1-str | kcp 1-str | tcp 8-str | kcp 8-str | tcp HoL max | kcp HoL max | tcp RSS | kcp RSS |
|---|---|---|---|---|---|---|---|---|
| loopback | 4.8 | 3.2 | 14.8 | 1.5 | 33.4 | 33.4 | 23.8 | 91.0 |
| rtt10 | 4.2 | 0.415 | 5.5 | 0.653 | 81.1 | 80.9 | 20.6 | 57.4 |
| rtt100 | 0.652 | 0.038 | 0.992 | 0.064 | 801.6 | 801.0 | 19.3 | 33.8 |
| loss1_rtt10 | 3.7 | 0.418 | 8.6 | 0.686 | 360.0 | 288.7 | 28.5 | 67.9 |
| loss5_rtt100 | 0.242 | 0.029 | 0.62 | 0.038 | 2963.2 | 801.0 | 21.6 | 41.4 |
| loss2b25_rtt10 | 3.3 | 0.393 | 9.5 | 0.691 | 1179.7 | 315.4 | 34.3 | 64.1 |
| rate100_rtt20 | 0.03 | 0.026 | 0.03 | - | 514.2 | 246.4 | 18.1 | 42.9 |
| rate20_rtt40 | 0.007 | 0.005 | - | - | 2039.8 | 958.9 | 19.9 | 25.6 |
| jitter20_10 | 2.1 | 0.151 | 3.1 | 0.176 | 159.3 | 349.9 | 21.6 | 68.5 |

KCP-over-UDP stays far slower than TCP tunnels in every cell and costs
~4x the memory (the 2048/4096 ARQ windows); its one measured win is UDP
session quality at high delay: at rtt100 its probe shows 0% loss and a
20 ms max inter-packet gap where the TCP arms' games stall 100+ ms. It is
worth considering only when TCP data tunnels are blocked or throttled.

The kcp4 numbers above are the 2026-09-10 refresh (batched datagram IO
and tightened SACK thresholds — see HANDOFF.md). Note the loopback
8-stream cell: the re-measured 1.5 Gbit/s reflects today's host state
(the previous 5.9 was not reproducible on this host for either code
version, A/B-checked), while the loss cells improved +11-81% across the
board.

### Configuration tradeoffs (loopback)

![Configuration tradeoffs](assets/benchmark-cost-v0.8.0.png)

| Tool | CPU% | churn/s | churn p99 ms | UDP pps | thr64 | mixed bulk |
|---|---|---|---|---|---|---|
| **mux** | 568.3 | 4998.0 | 3.53 | 19997.8 | 17.87 | 11.22 |
| mux-off | 596.6 | 4969.3 | 3.55 | 19997.8 | 24.65 | 17.56 |
| noise | 535.3 | 4960.0 | 3.56 | 19997.8 | 15.93 | 4.84 |
| mux1 | 262.6 | 5005.7 | 3.53 | 19999.6 | - | 9.00 |
| kcp4 | 366.0 | 4778.3 | 3.71 | 19999.0 | 6.59 | 0.38 |

Every mode sustains ~20k UDP datagrams/s with 0% loss and ~5k
connections/s under churn (setup-to-first-byte p99 ~3.5-4.7 ms — the pool
absorbs per-connection setup); noise's CPU is a wash with plain under
full load, and KCP's lower CPU% simply reflects its lower throughput.
`mux1` is omitted from the 64-stream panel: 64 streams on a single tunnel
exceed the yamux ceiling of `count × 32` (32 here), so the runner skips that
scale point by design instead of wedging its iperf3 server; the mixed-bulk
probe that follows now measures 9.0 Gbit/s (previously it inherited the
wedge and recorded a null). The mixed workload shows the per-service story:
a bulk service starves an interactive one on the same client (mux mixed
bulk 11.2 vs 17.6 in direct mode).

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
  delay proxy and loss cells are skipped. The rate cells shape at a
  fraction of the nominal rate on loopback (measured ~30% of the cap) —
  read them as relative overhead comparisons, not literal link speeds.
  The plain-TCP peers run a lean subset (loopback, rtt10, 1% loss and the
  two rate cells); the molehill-vs-peers chart therefore plots only those
  cells, while the pure-delay, 5%-loss, burst-loss and jitter cells are
  molehill-only stories told by the count and carrier charts.
- **Metrics**: TCP throughput (1/8/64 streams — 64 is the working point
  below the yamux ceiling of `count × 32` concurrent connections; median
  rep with its retransmits, plus the min/max spread across reps),
  connection-path RTT (fresh connects, up to 300 samples bounded at 20 s
  wall), steady data-path RTT (pings on one connection, likewise bounded
  at 20 s), **connection churn** (short-connection storm at 16 concurrent
  connectors: connects/sec and setup-to-first-byte p50/p99 — the
  mux-vs-direct and pool guidance data), UDP session quality
  (RTT/loss/jitter/max gap over one session) and **sustained UDP
  capacity** (paced 20k datagrams/s, delivered pps + loss), a head-of-line
  probe (saturating bulk + game-like pinger through one service), a
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
  On the low-rate rate20 cell, eight parallel iperf3 streams wedge the
  single-test iperf3 server (netem's packet limit counts GSO-sized
  segments, so the buffer holds seconds of data on the shaped path) — its
  8-stream slot is `null` with the reason recorded in
  `partial_metrics`; the rate100 cell measures both 1-stream and 8-stream.
  An arm whose yamux ceiling (`count × 32`) is below the
  64-stream scale point skips that probe the same way. Charts plot only the
  rows and cells that take part in a comparison (a peer-less cell or a
  structurally skipped probe is not drawn at all); a value missing inside a
  compared panel is a grey `x`, while a measured zero is labelled `0` so
  zero and absent stay distinct. A HoL pinger that gets zero or one reply
  records a stall (the elapsed wait) rather than `null`.
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
`scratch` (~8 MiB), runs as non-root UID 1000, and includes the same default
feature set as the regular release builds (multiplexing enabled).

```bash
docker run -v /etc/molehill/server.toml:/app/server.toml:ro \
  ghcr.io/niyueee/molehill:latest server.toml
```

The image contains no configuration — mount your config file and pass its
name as the argument. See the [container deployments](./docs/configuration.md#container)
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
