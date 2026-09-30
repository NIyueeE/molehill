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
  - [Benchmarks](#benchmarks)
    - [Choosing a configuration](#choosing-a-configuration)
    - [molehill vs the plain-TCP peers](#molehill-vs-the-plain-tcp-peers)
  - [Quickstart](#quickstart)
  - [Configuration](#configuration)
  - [Deployment](#deployment)
  - [Documentation](#documentation)
  - [Development](#development)

<!-- /TOC -->

## Features

- **High Performance** Much higher throughput can be achieved than frp, and more stable when handling a large volume of connections.
- **Low Resource Consumption** Consumes much fewer memory than similar tools. [The binary can be](docs/build-guide.md) **as small as ~500KiB** to fit the constraints of devices, like embedded devices as routers.
- **Client-Authoritative Services** Since v0.7 the server needs no per-service configuration: clients declare what to expose (including the public port) and the server enforces an `allow_ports` whitelist. One shared token authenticates everything.
- **Multiplexing** Every data channel rides as a yamux stream over one of an elastic pool of tunnel connections (up to `[client.data.tcp|kcp].max_tunnels`, default 4) — no per-connection handshakes, dramatically fewer file descriptors, throughput beyond a single TCP flow, and head-of-line isolation (a lost segment stalls only its own tunnel). The pool starts cold and grows on demand, so a client that is idle holds nothing; the optional `default_carrier = "kcp"` (feature `kcp`) moves the data plane onto KCP-over-UDP sessions. The `[client.data]` knobs and the `mode = "direct"` fallback are covered in [Configuration](./docs/configuration.md).
- **Security** A shared token is mandatory and the `allow_ports` whitelist bounds what any client can expose. The optional Noise Protocol encrypts the wire with a single pre-shared X25519 keypair — no PKI, no CA — and, with `resume = true`, proves a reconnect with a MAC instead of repeating the handshake's key exchanges (connection setup 442.7 -> 38.5 us per pair). `plain` forwards unencrypted.
- **Hot Reload** Services can be added or removed dynamically by hot-reloading the configuration file.

## Benchmarks

Single-machine comparison (`visitor -> server -> client -> backend`, all four
hops on one machine). Everything is measured **through the tunnel**: the probes
dial each tool's exposed port, never the backend it forwards to. The peers are
the latest GitHub release builds (frp, rathole upstream, nps, versions recorded
with each run). Every tool is driven through the identical workload while the
network condition follows a scripted stage schedule, changed in place, so a
tool's session is never rebuilt — how it adapts to a degrading and then
recovering path is part of the measurement.

### Choosing a configuration

The defaults — `mode = "multiplex"`, `max_tunnels = 4`, `carrier = "tcp"`,
plain transport — are the right starting point for almost everyone; deviate
only when the tree says so. Three questions decide the rest, and each answer is
one line in `[client.data]` or `[client.services.<name>]`: **encryption** (set
`[client.transport] type = "noise"` and place the keys, see
[Transport](docs/transport.md)); **concurrency** (raise `max_tunnels` — each
tunnel carries ~64 concurrent connections, so `8` ≈ 512 — or spread one
connection over `[server.data] stripe_count` parallel channels); and **the
path** (A/B `carrier = "kcp"` when TCP data tunnels are throttled or you need
latency-first UDP; keep `max_tunnels >= 4` on lossy paths so the pool aggregates
and isolates head-of-line blocking).

Two numbers decide between those options, and they are best measured on your
own path rather than read off a table: the **sustainable load** (how many bulk
streams the tool carries while a fresh interactive connection still meets the
50 ms and 0.5 % errors) and the **cost at the operating point** (CPU-seconds
per carried Gbit/s). How to run that comparison is in
[Benchmarks](docs/benchmarks.md); the settings themselves — including the
step-by-step decision tree — are in
[Configuration](docs/configuration.md#choosing-your-configuration-decision-tree).

### molehill vs the plain-TCP peers

Every tool is driven through the identical workload — one interactive stream
(the SLO instrument), N = 20 bulk TCP streams, 16 short connections per
second and one UDP session — while the path follows the stage schedule
(netem on `lo`, the control plane left unshaped). The chart below is the
v0.10.0 run on one host (the released binary's defaults: `multiplex`, an
elastic pool of up to four tunnels per service, plain transport): the orange
line is the bulk throughput, the blue points the interactive stream's RTT, the
shaded bands the path classes, the dashed line the SLO (p99 <= 50 ms).

![Soak: molehill and the peers over the stage schedule](assets/soak-v0.10.0.png)

The same run as small multiples — one panel per stage, a lollipop per tool
(dot = p50, bar = p99, tick = worst second), so "who wins which condition"
reads without a table:

![Interactive RTT per stage, per tool](assets/soak-v0.10.0-stages.png)

**Interactive stream RTT p99, per stage** (ms). A shaped stage of a saturated
run carries tens of samples, and a stage under a hundred reports its *worst
observation* rather than a p99 — the sample counts are in the results file
beside these numbers. `~` marks a **shaped** class: the value is the run's
reading, but the harness installed the queue that dominates it and one run does
not repeat it — three runs of one unchanged method moved these cells by 5-24 %
on this host — so the `~` columns are context and no winner is marked in them.
`‡` marks a stage that also recorded a wedge (a silent stretch, drawn as a flat
segment in the chart); a stage that recovered carries both the marker and its
number.

| tool | clean | rtt100 | loss1 | loss5 | rate100 | rate20 | jitter | clean (repeat) |
|---|---|---|---|---|---|---|---|---|
| molehill (mux) | 8.4 | ~6242‡ | ~1126 | ~3201‡ | ~1532 | ~7659‡ | ~5045‡ | 7.6 |
| frp | 2.8 | ~5626‡ | ~1058 | ~3494 | ~1569 | ~7494‡ | ~8040‡ | 2.9 |
| rathole | 77.4 | ~6107‡ | ~1133 | ~3687‡ | ~1573 | ~7893‡ | ~4477‡ | 77.7 |
| nps | 66.4 | ~467 | ~1068 | ~2073 | ~1519 | ~7804‡ | ~5406‡ | 68.1 |

**Bulk throughput per stage** (Gbit/s, over the stage's whole measured window,
not its best second: netem releases a shaped burst into whichever interval it
likes, so the peak is the shaper's schedule, not the path). The **measurement**
decides which side speaks: the sender, unless its `end` event or a stage with at
least half its intervals at zero bytes says its writes did not track the path —
then the reading is the **receiver's** own window, marked `*`. `— †` is a stage
with no reading at all, with the reason why (all four arms' `jitter`, whose
zeros are congestion collapse rather than a buffered sender). The rate cells
read the shaper's own numbers — 0.100 and 0.019 Gbit/s on every arm — because
the client's window is bounded on a rate class: without that bound the same cell
read 0.033 Gbit/s on a 20 Mbit path, *above* nominal, because the transfer
outlived the stage it was measured in. A rate cell has no contrast by
construction, and this table says so instead of ranking arms on it.

| tool | clean | rtt100 | loss1 | loss5 | rate100 | rate20 | jitter | clean (repeat) |
|---|---|---|---|---|---|---|---|---|
| molehill (mux) | 18.830 | 5.224 | 9.717 | 5.265 | 0.100 | 0.019 | — † | 20.482 |
| frp | 6.036 | 5.596 | 5.709 | 5.252 | 0.100 | 0.019 | — † | 6.058 |
| rathole | 17.986 | 5.197 | 9.692 | 5.237 | 0.100 | 0.019 | — † | 17.585 |
| nps | 0.133 | 0.152 | 0.139 | 0.160 | 0.100 | 0.019 | — † | 0.135 |

**The noise these numbers have to clear.** The schedule measures `clean` at
both ends of every timeline, so each tool's two clean readings are two samples
of one condition about an hour apart — the run's own replicate, and the scale
every other cell has to be read against. `just soak-check` reports it:

| tool | clean bulk reading | clean interactive p99 |
|---|---|---|
| molehill (mux) | 18.830 – 20.482 Gbit/s (**8.1 %** apart) | 7.6 – 8.4 ms |
| frp | 6.036 – 6.058 Gbit/s (**0.4 %** apart) | 2.8 – 2.9 ms |
| rathole | 17.585 – 17.986 Gbit/s (**2.2 %** apart) | 77.4 – 77.7 ms |
| nps | 0.133 – 0.135 Gbit/s (**1.8 %** apart) | 66.4 – 68.1 ms |

**How much it carries.** The same artifact carries the load ramp: the first
bulk load level at which a fresh interactive connection breaks the SLO (p99
50 ms, 0.5 % errors). It is a *different instrument* from the staged schedule —
the schedule asks what happens as the path changes, the ramp asks where the
ceiling is — and neither cross-checks the other.

| tool | sustainable streams | ceiling | headroom | reason at the break |
|---|---|---|---|---|
| molehill (mux) | 8 | 8 | 0.0 | never broke |
| frp | 8 | 8 | 0.0 | never broke |
| rathole | 3 | 8 | 0.625 | interactive error rate 0.006 > 0.005 |
| nps | 0 | 8 | 1.0 | interactive p99 205.035 > 50.0 |

Two arms carried the ramp's full 8 streams — molehill and frp — which is the
ramp's own ceiling, so that reads as a **floor** ("at least 8"), not as a
measured maximum; rathole broke at the fourth load level (its interactive error
rate crossed 0.5 % at ~20 Gbit/s of offered load) and nps breaks the SLO at the
first stream it is offered.

**What these shapes say.** Every tool degrades under a bad path and every tool
recovers on the return to clean — that recovery is what the last column
measures, and a tool that stayed wedged would be a finding. On the clean path
molehill reads 18.8-20.5 Gbit/s against rathole's 17.6-18.0: the ranges do not
overlap, but the gap is smaller than molehill's own replicate (8.1 %), so this
run does not separate them — then frp at 6.0 and nps at 0.13. On latency the
order inverts at the top — frp answers in 2.8-2.9 ms, molehill 7.6-8.4, nps
66-68, rathole 77.4-77.7 — so molehill and frp are the two arms inside the SLO
on both axes, and they are also the two that carry the ramp's full eight
streams. `loss1` (10 ms delay, 1 % loss) separates the throughput pair from frp:
9.72 and 9.69 Gbit/s against 5.71, with nps at 0.14. The shaped interactives are
*context*: they are dominated by the queue the harness installed, they swing by
more than any between-tool gap in them between runs of unchanged code, and every
arm wedges on `rate20` and `jitter` — that is the path, not one tool. The honest
losses are carried rather than smoothed over: frp's clean-stage interactive cost
is 2.8 ms against molehill's 7.6, and nps reads zero bytes on a share of its
intervals in *every* stage including the clean ones, which no other arm does.

The peers are driven by the same workload and charted in the same panels; the
drift axis (open fds, RSS and CPU slopes over the run) is in
`soak-v0.10.0-drift.png`, the UDP session's RTT/loss in
`soak-v0.10.0-udp.png` (a sliding loss *rate*, not a count of loss events), and
the load ramp in `soak-v0.10.0-capacity.png`.

These are v0.10.0 numbers from one host, measured with the method this page
describes. **The host's *instance* is not the method**: the two arms that reach
the loopback ceiling lost a quarter to a third of their clean throughput between
the container instances this work ran on (molehill 21.8 -> 16.7, rathole
21.2 -> 12.8 Gbit/s) while frp and nps were flat, so the top pair's ordering is
a fact about that run and does not travel as a standing claim. Every results
file records the host, the method and two tool-free calibrations (CPU state and
the loopback path), and `just soak-check` refuses to compare runs that disagree on
them; only same-schema, same-method,
same-host runs compare directly, and each run is gated on its own completeness,
endpoint and SLO checks.
The per-stage numbers carry their sample count in the results file
(`rtt_n`): a stage that carried fewer than a hundred interactive samples
reports its *worst observation* as the p99, which is what a shaped stage of a
saturated run (tens of samples) is.
Reading a chart, reproducing a run and the gate's verdict:
[Benchmarks](docs/benchmarks.md). How to read a chart in detail (the log axis, the
step lines, the wedge bars, what each band means), the stage schedule, the test
types and how to reproduce a run on your own hardware:
[Benchmarks](docs/benchmarks.md).

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

Download a pre-built binary for your platform from the
[release page](https://github.com/NIyueeE/molehill/releases), or
[build from source](./docs/build-guide.md) for other platforms and
minimal-sized binaries.

```bash
./molehill server.toml   # on the public server
./molehill client.toml   # on the device behind NAT
```

How to run it as a service is in [Configuration](./docs/configuration.md),
which owns the [systemd units](./docs/configuration.md#systemd) (root and
rootless, multiple instances) and the
[container deployments](./docs/configuration.md#container) — the published
`ghcr.io/niyueee/molehill` images (linux/amd64, linux/arm64; a static musl
binary on `scratch`), the non-root UID they run as, and the one
container-specific note that a `carrier = "kcp"` service needs its data-plane
port published over **UDP**.

## Documentation

For people running molehill:

- [Configuration](./docs/configuration.md) — full configuration specification, logging, tuning
- [Transport](./docs/transport.md) — Noise Protocol setup
- [Benchmarks](./docs/benchmarks.md) — how the published numbers are produced, how to read them, how to reproduce them
- [Build guide](./docs/build-guide.md) — build customization, minimal binary
- [Internals](./docs/internals.md) — how control/data channels work
- [Configuration examples](./docs/configuration.md#complete-examples) — configs for common scenarios (systemd & container deployments included)

For people changing it (contributor and governance docs are English-only by
decision — see [AGENTS.md](./AGENTS.md) §3):

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
runs the identical chain for anything that touches code — a docs-only change
runs just the docs-alignment check (`docs.yml`) instead:

```bash
just setup   # activate git hooks (core.hooksPath githooks) + install check tools
just check   # fmt / secrets / machete / docs / ruff (check + format) / clippy + audit / deny / outdated / test
just tag     # release review (githooks/pre-tag) + create the local v* tag
```

molehill began as a fork of [rathole](https://github.com/rapiz1/rathole)
(Apache-2.0) and has been developed independently since; the upstream
history is preserved below the fork point and the version line continues
from there (upstream's last release was v0.5.0). See
[docs/release.md](./docs/release.md) for release mechanics and
[AGENTS.md](./AGENTS.md) for the repository rules.
