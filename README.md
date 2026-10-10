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
    - [The v0.10.0 run](#the-v0100-run)
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
- **Transparent (L3) Clients (Linux)** a `[transparent]` block makes the client own a public `ip:port`: the server routes whole IP packets into the tunnel and the client's kernel answers the visitor, so the backend sees the visitor's real source address and the server holds no connection state for the flow. It is a run mode of its own (`--transparent`), so a host that also forwards runs two processes; the client needs a TUN device and `CAP_NET_ADMIN`, and a server needs them only when its own `[server.transparent]` table asks for it. See [Configuration](./docs/configuration.md#transparent-l3-services).
- **Multiplexing** Every data channel rides as a yamux stream over one of an elastic pool of tunnel connections (up to `[client.data.tcp|kcp].max_tunnels`, default 4) — no per-connection handshakes, dramatically fewer file descriptors, throughput beyond a single TCP flow, and head-of-line isolation (a lost segment stalls only its own tunnel). The pool starts cold and grows on demand, so a client that is idle holds nothing; the optional `default_carrier = "kcp"` (feature `kcp`) moves the data plane onto KCP-over-UDP sessions. The `[client.data]` knobs and the `mode = "direct"` fallback are covered in [Configuration](./docs/configuration.md).
- **Security** A shared token is mandatory and the `allow_ports` whitelist bounds what any client can expose. The optional Noise Protocol encrypts the wire with a single pre-shared X25519 keypair — no PKI, no CA — and, with `resume = true`, proves a reconnect with a MAC instead of repeating the handshake's key exchanges (connection setup 442.7 -> 38.5 us per pair). `plain` forwards unencrypted.
- **Hot Reload** Services can be added or removed dynamically by hot-reloading the configuration file.

## Benchmarks

Single-machine comparison (`visitor -> server -> client -> backend`, all four
hops on one host), measured **through the tunnel**: the probes dial each tool's
exposed port, never the backend it forwards to. Method, chart reading and
reproduction: [Benchmarks](./docs/benchmarks.md); the decision tree behind the
settings, and the two numbers worth measuring on your own path:
[Configuration](./docs/configuration.md#choosing-your-configuration-decision-tree).

### The v0.10.0 run

Every tool is driven through the identical workload while the path follows the
stage schedule, changed in place so a session is never rebuilt — this is the
v0.10.0 run on one host, with the released binary's defaults (`multiplex`, plain
transport); the chart legend and the schedule are in
[Benchmarks](./docs/benchmarks.md#how-to-read-the-charts).

![Soak: molehill and the peers over the stage schedule](assets/soak-v0.10.0.png)

The same run as small multiples — one panel per stage, a lollipop per tool:

![Interactive RTT per stage, per tool](assets/soak-v0.10.0-stages.png)

**Interactive stream RTT p99, per stage** (ms). `~` marks a **shaped** class —
the harness installed the queue that dominates it, so no winner is marked in
those columns; `‡` marks a stage that also recorded a wedge. Sample counts and
the full reading rules: [Benchmarks](./docs/benchmarks.md#how-to-read-a-cell).

| tool | clean | rtt100 | loss1 | loss5 | rate100 | rate20 | jitter | clean (repeat) |
|---|---|---|---|---|---|---|---|---|
| molehill (mux) | 8.4 | ~5749‡ | ~1136 | ~3378 | ~1544 | ~7633‡ | ~6120‡ | 9.4 |
| frp | 2.9 | ~4904 | ~1084 | ~3558 | ~2917 | ~7670‡ | ~6297‡ | 3.0 |
| rathole | 102.3 | ~7186‡ | ~1147 | ~3716‡ | ~1595 | ~7360‡ | ~4522‡ | 104.5 |
| nps | 64.4 | ~466 | ~1096 | ~1690 | ~1543 | ~7158‡ | ~8086‡ | 66.9 |

**Bulk throughput per stage** (Gbit/s, over the stage's whole measured window,
not its best second). A `*` marks a cell read from the **receiver's** own
window; `— †` marks a stage with no reading at all, with the reason why. Which
side a cell uses: [Benchmarks](./docs/benchmarks.md#how-to-read-a-cell).

| tool | clean | rtt100 | loss1 | loss5 | rate100 | rate20 | jitter | clean (repeat) |
|---|---|---|---|---|---|---|---|---|
| molehill (mux) | 16.830 | 5.261 | 9.727 | 5.272 | 0.100 | 0.019 | — † | 15.921 |
| frp | 6.191 | 5.540 | 5.862 | 5.306 | 0.099 | 0.019 | — † | 6.181 |
| rathole | 12.822 | 5.191 | 9.679 | 5.222 | 0.100 | 0.020 | — † | 12.867 |
| nps | 0.134 | 0.150 | 0.142 | 0.167 | 0.100 | 0.020 | — † | 0.135 |

**The run's own replicate.** `clean` is measured at both ends of every
timeline, so each tool's two readings are two samples of one condition about
an hour apart — the scale every other cell is read against:

| tool | clean bulk reading | clean interactive p99 |
|---|---|---|
| molehill (mux) | 15.921 – 16.830 Gbit/s (**5.4 %** apart) | 8.4 – 9.4 ms |
| frp | 6.181 – 6.191 Gbit/s (**0.2 %** apart) | 2.9 – 3.0 ms |
| rathole | 12.822 – 12.867 Gbit/s (**0.3 %** apart) | 102.3 – 104.5 ms |
| nps | 0.134 – 0.135 Gbit/s (**0.5 %** apart) | 64.4 – 66.9 ms |

**How much it carries.** The same artifact carries the load ramp — the first
bulk load level at which a fresh interactive connection breaks the SLO — a
different instrument from the staged schedule
([Benchmarks](./docs/benchmarks.md#the-scenarios)); three arms carried its full 8
streams, so 8 reads as a **floor** ("at least 8"), not a maximum:

| tool | sustainable streams | ceiling | headroom | reason at the break |
|---|---|---|---|---|
| molehill (mux) | 8 | 8 | 0.0 | never broke |
| frp | 8 | 8 | 0.0 | never broke |
| rathole | 8 | 8 | 0.0 | never broke |
| nps | 0 | 8 | 1.0 | interactive p99 204.84 > 50.0 |

These are v0.10.0 numbers from one host, and only same-schema, same-method,
same-host runs compare directly: every results file records the host, the
method and two tool-free calibrations, and each run is gated on its own
completeness, endpoint and SLO checks
([Benchmarks](./docs/benchmarks.md#comparability)). Note that **molehill and
rathole both read at this host's loopback ceiling and move with its state
between runs**: across three sweeps of identical code their clean readings span
15.9-22.2 and 12.8-20.4 Gbit/s — swings of 39 % and 59 % that reverse their
order — while `frp` (6.04-6.19) and `nps` (0.133-0.136), an order of magnitude
below the ceiling, moved by under 3 %.

The rest of the run's chart set is published beside these two:
`soak-v0.10.0-drift.png` (open fds, RSS and CPU slopes over the run),
`soak-v0.10.0-udp.png` (the UDP session's RTT and sliding loss *rate*) and
`soak-v0.10.0-capacity.png` (the load ramp).

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
[systemd units](./docs/deployment.md#systemd) or the
[container deployments](./docs/deployment.md#container).

## Configuration

`molehill` determines the running mode (server/client) from the config file
automatically, or you can force it with `--server` / `--client`. The full
configuration specification, logging and tuning options are documented in
[Configuration](./docs/configuration.md), which also includes
[worked examples](./docs/deployment.md#worked-examples) for various scenarios.

## Deployment

The same binary runs on both ends; the mode comes from the config file:

```bash
./molehill server.toml   # on the public server
./molehill client.toml   # on the device behind NAT
```

Ready-to-run configuration examples, systemd units and container recipes are
in [Deployment](./docs/deployment.md), which also covers the network
requirements and the deployment security notes.

## Documentation

For people running molehill:

- [Configuration](./docs/configuration.md) — full configuration specification, logging, tuning
- [Deployment](./docs/deployment.md) — ready-to-run configs, systemd units and container recipes
- [Transport](./docs/transport.md) — Noise Protocol setup
- [Benchmarks](./docs/benchmarks.md) — how the published numbers are produced, how to read them, how to reproduce them

For people changing it (contributor and governance docs are English-only by
decision — see [AGENTS.md](./AGENTS.md) §3):

- [Checks](./docs/checks.md) — what every gate runs, how to handle a block
- [Lint policy](./docs/lint-policy.md) — lint levels and waiver rules
- [Release](./docs/release.md) — release mechanics, versioning, test builds
- [Structure](./docs/structure.md) — what every file in this repo is for
- [Build guide](./docs/build-guide.md) — build customization, minimal binary
- [Internals](./docs/internals.md) — how control/data channels work
- [Contributing](./CONTRIBUTING.md) — setup and workflow
- [Security](./SECURITY.md) — reporting vulnerabilities
- [`HANDOFF.md`](./HANDOFF.md) — current working state; planned work and future design documents

## Development

molehill is written in Rust (2024 edition); `rust-toolchain.toml` declares
`channel = "stable"` with clippy and rustfmt components — never hardcode a
version. What each gate runs, and how to handle a block, is in
[Checks](./docs/checks.md).

```bash
just setup   # activate git hooks (core.hooksPath githooks) + install check tools
just check   # fmt / secrets / machete / docs / ruff (check + format) / clippy + audit / deny / outdated / test
just tag     # release review (githooks/pre-tag) + create the local v* tag
```

molehill is an independent project that began as a fork of
[rathole](https://github.com/rapiz1/rathole); see
[CHANGELOG.md](./CHANGELOG.md) for what each release changed and
[AGENTS.md](./AGENTS.md) for the repository rules.
