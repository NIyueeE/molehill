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
beside these numbers. `‡` marks a stage that also recorded a wedge (a silent
stretch, drawn as a flat segment in the chart); `†` marks a stage whose bulk
spine produced no intervals at all, so its interactive number was measured
*without* the bulk load.

| tool | clean | rtt100 | loss1 | loss5 | rate100 | rate20 | jitter | clean (return) |
|---|---|---|---|---|---|---|---|---|
| **molehill (mux)** | 9.6 | ‡6382 | 1301 | 3447 | 2485 | ‡†3690 | 1275 | 6.6 |
| frp 0.71.0 | **2.8** | ‡6562 | 5454 | ‡6357 | ‡8211 | 3279 | ‡7741 | **2.9** |
| rathole 0.5.0 | 73 | ‡6134 | **1323** | ‡6203 | ‡7455 | 2018 | 5326 | 68 |
| nps 0.26.10 | 70 | **858** | 1146 | 2859 | ‡7036 | 2669 | †220 | 67 |

**Bulk throughput per stage** (Gbit/s, the stage's peak interval): molehill
**21.7** on clean -> 2.89 at rtt100 -> 5.35 at loss1 -> 2.43 at loss5 -> 0.816
at rate100 -> **no sample at rate20** -> no sample at jitter -> **23.2 on the
return to clean**; frp 7.09 -> 2.77 -> 5.34 -> 3.33 -> 0.535 -> 0.000 -> 0.000
-> 6.78; rathole 22.9 -> 3.03 -> 5.33 -> 3.49 -> 0.712 -> 0.000 -> 0.000 ->
24.0; nps 0.642 -> 1.53 -> 1.25 -> 1.07 -> 0.356 -> 0.000 -> no sample -> 0.453.

The delay- and loss-shaped cells are quotable: **not one zero-byte interval**
for molehill, frp or rathole across `rtt100`, `loss1` and `loss5`. The rate
cells are the degenerate side, and in this run they are degenerate for every
arm at once — the shaper holds each interval's bytes past the interval's own
accounting window, so 59 % of molehill's `rate100` intervals read zero (73 %
frp, 78 % rathole, 67 % nps), and every `rate20` and `jitter` interval that
arrived at all read zero. Where the peak is 0 for every tool there is no
contrast to read, so those cells are reported as `0.000` rather than drawn as a
comparison. `†` marks a **live-tool, dead-probe** cell: molehill's `rate20`
spine never connected (`control socket has closed unexpectedly`) and nps's
`jitter` one failed the same way, so the harness waited the stage out. Their
interactive numbers are real, but they describe that path with no bulk load on
it, which is not the pair the other stages report.

**What these shapes say.** Every tool degrades under a bad path, and every tool
recovers on the return to clean — that recovery is what the last band measures,
and a tool that stayed wedged would be a finding. On the clean stage molehill
and rathole carry the same bulk (21.7 against 22.9 Gbit/s; frp 7.1, nps 0.6)
while a fresh interactive connection costs 9.6 ms for molehill against frp's
2.8, rathole's 73 and nps's 70. The shaped interactives are worst observations
from tens of samples, and they move between runs of unchanged code by more than
the code moves them: molehill's `rate100` read 7686 ms in the previous sweep of
this method and 2485 ms here, frp's `jitter` 3061 and 7741. They are context,
not a verdict. All four tools wedge on `rate100` and three of the four on
`rtt100` — the shaped path, not one tool. The honest losses are carried in the
table rather than smoothed over: frp's clean-stage interactive cost is 2.8 ms
against molehill's 9.6, and one bulk cell per run comes back empty (`†`).

The peers are driven by the same workload and charted in the same panels; the
drift axis (open fds, RSS and CPU slopes over the run) is in
`soak-v0.10.0-drift.png` and the UDP session's RTT/loss in
`soak-v0.10.0-udp.png` (a sliding loss *rate*, not a count of loss events).

These are v0.10.0 numbers from one host. Only runs of the same model, method
and host compare directly, and every results file records the host it was
measured on: `just soak-check` reads it, refuses to gate one host's run against
another's, and gates each run on its own completeness, endpoint and SLO checks.
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
