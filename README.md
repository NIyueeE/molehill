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

<p align="center">A secure, stable, high-performance reverse proxy for NAT traversal — now with transparent (L3) clients, where the device behind NAT owns its own public IP.</p>

[English](README.md) | [简体中文](README.zh.md)

molehill exposes a service on a device behind NAT to the Internet through a server with a public IP, like [frp](https://github.com/fatedier/frp) and [ngrok](https://github.com/inconshreveable/ngrok).

**Two ways to expose a service**, and the choice is the client's:

- **Forwarding (L4)** — the server binds a public port and relays connections
  into the tunnel. The usual shape, and what every tool in this space does.
- **Transparent (L3)** — the client **owns the public `ip:port` itself**. The
  server routes whole IP packets into the tunnel and the client's own kernel
  answers the visitor, so the backend sees the visitor's real source address
  and the server holds no connection state for the flow at all. This is the
  mode you want when the real source address matters — a mail server, a game
  server, anything that logs or rate-limits by peer — and it is the reason a
  reverse proxy in front of it is not a compromise. Linux only.

<!-- TOC -->

- [molehill](#molehill)
  - [Features](#features)
  - [Benchmarks](#benchmarks)
    - [The v0.11.0 run](#the-v0110-run)
  - [Quickstart](#quickstart)
  - [Configuration](#configuration)
  - [Deployment](#deployment)
  - [Documentation](#documentation)
  - [Development](#development)

<!-- /TOC -->

## Features

- **Transparent (L3) clients: the device behind NAT owns its public IP (Linux).**
  A `[transparent]` block is a run mode of its own: the server binds nothing,
  routes whole IP packets into the tunnel, and the **client's kernel answers the
  visitor** — so the backend sees the visitor's real source address, TCP keeps
  its end-to-end semantics, and the server holds no socket and no per-flow state
  for the connection. Nothing is translated. It lowers into the same engine the
  forwarding path uses, so the wire and the data path are shared. The daemon
  configures no network itself: it attaches to a TUN device you created and
  refuses a missing prerequisite with the exact command to run, instead of
  failing as silent packet loss. Serving L3 is the *server operator's* decision
  (`[server.transparent]`), and without it a claim is refused by policy before
  any device is looked at. See [Configuration](./docs/configuration.md#transparent-l3-services)
  and [Deployment](./docs/deployment.md#transparent-services) for the two
  recipes and the privileges each mode needs.
- **High throughput** — measured against frp, rathole and nps on one host with the same method; the published numbers are [below](#benchmarks).
- **Small binary** — the `minimal` profile builds a **1.84 MiB** client binary and a **2.55 MiB** embedded one (measured 2026-10-17; see the [build guide](docs/build-guide.md)).
- **Client-authoritative services** — the server needs no per-service configuration: clients declare what to expose (including the public port) and the server enforces an `allow_ports` whitelist. One shared token authenticates everything.
- **Multiplexing** — every data channel of a forwarding service rides as a yamux stream over one of a fixed pool of tunnel connections (`[client.data].tunnels`, default 4) — no per-connection handshakes, dramatically fewer file descriptors, throughput beyond a single TCP flow, and head-of-line isolation (a lost segment stalls only its own tunnel). The pool is established at service start and never resized for load (a dead tunnel is repaired); the data plane itself runs over KCP (feature `kcp`) and falls back to TCP when a KCP session cannot be established, which is what `MOLEHILL_DATA_CARRIER` forces apart for a measurement. A transparent (L3) claim is the one shape that does not multiplex — its channels *are* its carrier connections, allotted by the lane budget and lent between claims as their flows come and go (`[transparent.data].lend`) — and the `[client.data]` knobs are covered in [Configuration](./docs/configuration.md).
- **Security** — a shared token is mandatory and the `allow_ports` whitelist bounds what any client can expose. The control channel **always** speaks Noise: it carries the credential every data channel authenticates with, so a plain control leg would publish it to the path, and both ends refuse to start without a key pair (`molehill --genkey`). One pre-shared X25519 keypair, no PKI, no CA; with `resume = true` a reconnect proves itself with a MAC instead of repeating the handshake's key exchanges (connection setup 442.7 → 38.5 us per pair). A *service* or an L3 lane may still run plain where the payload carries its own crypto.
- **An optional status API** — a token-gated `GET /status` on its own listener, answering what the client is carrying as JSON, so a panel or a script can read it. See [Configuration](./docs/configuration.md).
- **Hot reload** — services can be added or removed dynamically by hot-reloading the configuration file.

## Benchmarks

Single-machine comparison (`visitor -> server -> client -> backend`, all four
hops on one host), measured **through the tunnel**: the probes dial each tool's
exposed port, never the backend it forwards to. Method, chart reading and
reproduction: [Benchmarks](./docs/benchmarks.md); the decision tree behind the
settings, and the two numbers worth measuring on your own path:
[Configuration](./docs/configuration.md#choosing-your-configuration-decision-tree).

### The v0.11.0 run

Every tool is driven through the identical workload while the path follows the
stage schedule, changed in place so a session is never rebuilt. This is the
v0.11.0 sweep on one host, with the released binary's defaults — a multiplexed
pool of four tunnels, and **the control channel's Noise keys applied to the data
plane too**, because that is what this release's mandatory-Noise rule means for
a client that does not override it per service. The chart legend and the
schedule are in [Benchmarks](./docs/benchmarks.md#how-to-read-the-charts).

![molehill and the peers over the stage schedule](assets/bench-v0.11.0-timeline.png)

The same run as small multiples — one panel per stage, a lollipop per tool:

![Interactive RTT per stage, per tool](assets/bench-v0.11.0-stages.png)

**Interactive stream RTT p99, per stage** (ms). Every stage after `clean` is
shaped — the harness installed the queue or the delay that dominates it — so no
winner is marked in those columns. Sample counts and the full reading rules:
[Benchmarks](./docs/benchmarks.md#how-to-read-a-cell).

| tool | clean | rtt100 | loss1 | loss5 | rate100 | rate20 | jitter | clean (repeat) |
|---|---|---|---|---|---|---|---|---|
| molehill (L4) | 4.3 | 402.2 | 1061.4 | 2087.2 | 196.9 | 432.8 | 268.9 | 4.6 |
| molehill (L3) | 0.9 | 401.2 | 1055.2 | 2003.2 | 202.1 | 426.9 | 261.0 | 0.9 |
| frp | 3.0 | 401.4 | 1069.1 | 2440.8 | 197.0 | 432.8 | 272.1 | 1.5 |
| rathole | 1.6 | 401.0 | 1058.3 | 1914.2 | 195.9 | 434.8 | 281.0 | 1.4 |
| nps | 96.7 | 492.8 | 1090.3 | 2045.6 | 195.0 | 434.0 | 259.2 | 69.1 |

**Bulk throughput per stage** (Gbit/s, over the stage's whole measured window,
not its best second).

| tool | clean | rtt100 | loss1 | loss5 | rate100 | rate20 | jitter | clean (repeat) |
|---|---|---|---|---|---|---|---|---|
| molehill (L4) | 8.901 | 3.404 | 8.669 | 1.083 | 0.094 | 0.019 | 0.019 | 8.883 |
| molehill (L3) | 3.881 | 3.177 | 4.971 | 0.928 | 0.093 | 0.019 | 0.019 | 3.906 |
| frp | 6.026 | 3.376 | 6.135 | 1.124 | 0.094 | 0.019 | 0.019 | 6.019 |
| rathole | 28.180 | 3.412 | 14.852 | 1.123 | 0.094 | 0.019 | 0.019 | 28.281 |
| nps | 0.135 | 0.135 | 0.134 | 0.135 | 0.094 | 0.019 | 0.019 | 0.134 |

**The run's own replicate.** `clean` is measured at both ends of every
timeline, so each tool's two readings are two samples of one condition about an
hour apart — the scale every other cell is read against. The percentages use
the unrounded readings (the tables round to three decimals):

| tool | clean bulk reading | clean interactive p99 |
|---|---|---|
| molehill (L4) | 8.901 – 8.883 Gbit/s (**0.2 %** apart) | 4.3 – 4.6 ms |
| molehill (L3) | 3.881 – 3.906 Gbit/s (**0.7 %** apart) | 0.9 – 0.9 ms |
| frp | 6.026 – 6.019 Gbit/s (**0.1 %** apart) | 3.0 – 1.5 ms |
| rathole | 28.180 – 28.281 Gbit/s (**0.4 %** apart) | 1.6 – 1.4 ms |
| nps | 0.135 – 0.134 Gbit/s (**0.2 %** apart) | 96.7 – 69.1 ms |

**How much it carries.** The same artifact carries the load ramp — the first
bulk load level at which a fresh interactive connection breaks the SLO — a
different instrument from the staged schedule
([Benchmarks](./docs/benchmarks.md#the-scenarios)); every arm carried its full 8
streams, so 8 reads as a **floor** ("at least 8"), not a maximum.

**Two rows are one tool.** `molehill (L3)` is this project's second
architecture, not a fifth tool: frp, rathole and nps are forwarding proxies with
no TUN path, so the L3 row's comparison is `molehill (L4)` and the tool-free
control arm — same workload, same topology, one architecture apart. The two
architectures split the workloads: on the clean stage L3 reads 3.88 against
L4's 8.90 Gbit/s bulk, with a lower interactive p99 (0.9 against 4.3 ms) and a
steadier one under load — it holds every level of the capacity ramp. A claim's
lanes are carrier connections of its own rather than streams of a multiplexed
pool, and it holds **two by default**
([Configuration](./docs/configuration.md#transparent-l3-services) prices the
trade in connections).

**Every arm reads zero wedges on this run.** The shaped `rtt100` column above
is the shaper's number more than either architecture's: this run reads 401.2 ms
for L3 against 402.2 for L4, and the shaper's queue counters are zero for both
([Benchmarks](./docs/benchmarks.md#how-to-read-a-cell)).

**rathole reads above this host's loopback calibration** (28.2 against a
~21.1 Gbit/s loopback probe), so its row is a reading of the host's state, not
a standing ranking; `frp` and `nps`, an order of magnitude below the ceiling,
repeat their bulk reading within 1 %.

These are v0.11.0 numbers from one host; only same-schema, same-method,
same-host runs compare directly. v0.11.0 is the first release measured by the
bench model, so it has no comparable baseline
([Benchmarks](./docs/benchmarks.md#comparability)).

A deployment that wants the data plane unencrypted sets
`transport = { type = "plain" }` per service and keeps the encrypted control
channel; the cost of that choice is measured at
[Benchmarks](./docs/benchmarks.md#what-each-configuration-choice-costs-per-decision-measurements).

The rest of the run's chart set: `bench-v0.11.0-capacity.png` (the load ramp),
`bench-v0.11.0-drift.png` (RSS, fds and threads over the run) and
`bench-v0.11.0-cost.png` (CPU-seconds per carried Gbit).

## Quickstart

Download a binary from the [release](https://github.com/NIyueeE/molehill/releases) page, or [build from source](docs/build-guide.md) for another platform or a smaller binary.

As with frp, all service definitions live on the client side; the server only sets policy (a shared token and the `allow_ports` whitelist).

You need a server with a public IP and a device behind NAT running the services you want to expose.

Assuming you have a NAS at home behind the NAT, and want to expose its ssh service to the Internet:

1. On the server which has a public IP

Create `server.toml` and adapt it to your needs. The control channel is always
encrypted — it carries the credential every data channel authenticates with — so
both files carry a Noise key pair. Generate one with `molehill --genkey`, keep
the **private** key here and give the **public** one to the client.

```toml
# server.toml
[server]
default_token = "use_a_secret_that_only_you_know" # Shared secret with your clients # security-scan:allow documentation placeholder

# Master switch for dynamic registrations: only ports covered here can be claimed by clients
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333" # Port that molehill listens for clients

# Required: the control channel is always Noise
[server.transport.noise]
local_private_key = "2wOFku4PdLhcWQyqUXBWRrKhQV72S/9rkN9jktqiUS8=" # security-scan:allow documentation example key
```

Then run:

```bash
./molehill server.toml
```

2. On the host which is behind the NAT (your NAS)

Create `client.toml` and adapt it to your needs.

```toml
# client.toml
[client]
default_token = "use_a_secret_that_only_you_know" # Must match the server's `default_token` # security-scan:allow documentation placeholder

[client.control]
default_remote_addr = "myserver.com:2333" # The address of the server. The port must match `server.control.bind_addr`

[client.transport]
type = "noise"

# The server's public key, from `molehill --genkey`
[client.transport.noise]
remote_public_key = "9PE8H+fhLSgk9xFyqEwoKuQtq3n2sdGYzgY8jBtctlw=" # security-scan:allow documentation example key

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

3. The client connects to `myserver.com:2333`; traffic to `myserver.com:5202` is forwarded to port `22` on the NAS.

Then `ssh -p 5202 myserver.com` reaches the NAS.

To run `molehill` as a background service, see the
[systemd units](./docs/deployment.md#systemd) or
[container deployments](./docs/deployment.md#container).

## Configuration

`molehill` determines the running mode — server, client or transparent — from
the config file, or you can force it with `--server` / `--client` /
`--transparent`. The full configuration specification, logging and tuning
options are in [Configuration](./docs/configuration.md); ready-to-run examples
are in [Deployment](./docs/deployment.md#worked-examples).

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
