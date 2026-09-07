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

<p align="center">A community-maintained fork of <a href="https://github.com/rapiz1/rathole">rathole</a>.</p>

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
- **Multiplexing** Every data channel rides as a yamux stream over one tunnel connection by default — no per-connection handshakes and dramatically fewer file descriptors. The `mux` knobs and the `mux = false` fallback are covered in [Configuration](./docs/configuration.md).
- **Security** A shared token is mandatory and the `allow_ports` whitelist bounds what any client can expose. With the optional Noise Protocol, encryption can be configured at ease. No need to create a self-signed certificate! TLS is also supported.
- **Hot Reload** Services can be added or removed dynamically by hot-reloading the configuration file.

## Benchmarks

Single-machine comparison (all on loopback, `visitor -> server -> client ->
backend`); everything is measured **through the tunnel** — iperf3 and the
probes dial each tool's exposed port, never the backend. Peers are the
latest GitHub release builds (frp 0.71.0, rathole 0.5.0 upstream, bore
0.6.0). Network cells (netem on `lo`, every leg affected) and the metric
set are described in [Methodology](#methodology).

### Choosing a configuration

The measurements below justify the defaults and tell you when to deviate:

| Config | Use it when | Cost measured |
|---|---|---|
| **mux on (default)** | one client exposes **multiple services**, or connections churn (HTTP/WebSocket/game sessions); connection resources matter (FDs, ports, **NAT mappings** — every physical tunnel behind a NAT costs one mapping); a single long-lived tunnel survives NAT/firewall better than frequent new connects | single-stream ceiling ~10 Gbit/s (20.1 with mux off); under loss all streams share one retransmit domain (8-stream 4.7 vs 18.5 Gbit/s at 1% loss) |
| **mux off** | one service or a few long-lived streams (SSH); **raw throughput first** (bulk transfers): 20.1/28.2 Gbit/s on loopback; lossy network + many concurrent streams, where per-stream retransmit isolation wins | one physical tunnel per stream: FDs/ports/NAT mappings scale with stream count, plus one extra connect RTT per visitor connection (hidden by the pool in these tests) |
| **noise** | encrypted transport wanted with **memory and simplicity first**: 22.3 MiB vs TLS's 33.8, pre-shared public key, no certificates/PKI | throughput ~5-8% below TLS |
| **tls** | encrypted transport with the **best throughput** (hardware-accelerated AES) or an existing PKI; standard CA ecosystem compatibility | +50% memory vs noise |

Both encryptions halve throughput (3.8–4.1 vs 10.2 Gbit/s plain) with
negligible RTT cost; the transport choice does not change loss behavior.

### molehill vs plain-TCP peers

Plain-TCP axis only (mux on, no encryption): encrypted competitors such as
chisel's SSH tunnel are not comparable here — molehill's own encrypted rows
are isolated below.

![Benchmark: molehill 0.7.2 vs plain-TCP peers](assets/benchmark-v0.7.2.png)

| Tool | 1-stream | 8-stream | echo RTT p50 | Memory |
|---|---|---|---|---|
| **molehill (mux)** | 10.2 | 9.5 | 0.262 ms | 22.6 MiB |
| rathole 0.5.0 | 12.4 | 26.8 | 0.234 ms | 20.0 MiB |
| bore 0.6.0 | 14.2 | 27.0 | 0.495 ms | **8.4 MiB** |
| frp 0.71.0 | 4.8 | 6.3 | 0.375 ms | 72.1 MiB |

The multiplexed single tunnel tops out at ~10 Gbit/s per stream; the
per-connection architectures reach 12–14. bore is the lightest and a strong
plain TCP relay (no UDP forwarding); frp pays the highest memory for the
lowest throughput. Under delay every tool tracks ~101/1001 ms echo RTT
(bore pays extra round trips per connect); under 1% loss molehill, rathole
and bore hold 4.3–4.6 Gbit/s while frp collapses to 0.8.

### molehill: multiplexing cost (mux vs mux-off)

One variable (multiplexing on/off), loopback + all weak cells:

![Multiplexing cost](assets/benchmark-mux-v0.7.2.png)

| Cell | mux 1-str | mux-off 1-str | mux 8-str | mux-off 8-str |
|---|---|---|---|---|
| loopback | 10.2 | 20.1 | 9.5 | 28.2 |
| rtt10 | 6.3 | 8.3 | 5.9 | 10.1 |
| rtt100 | 0.75 | 0.74 | 1.0 | 1.3 |
| loss 1% | 4.4 | 4.3 | 4.7 | 18.5 |
| loss 5% | 0.19 | 0.32 | 0.31 | 1.49 |
| loss 2% burst | 4.0 | 4.3 | 4.3 | 18.7 |

Two takeaways: pure delay narrows the gap to a tie (per-connection cost is
amortized once RTT dominates), while **under loss the gap inverts for
concurrent streams** — the single tunnel shares one loss/retransmit domain
and all streams stall together, where mux-off retransmits per stream.

### molehill: transport cost (mux vs noise vs tls)

One variable (the encrypted transport), mux on for all three:

![Transport cost](assets/benchmark-transport-v0.7.2.png)

| Configuration | 1-stream | 8-stream | echo RTT p50 | Memory |
|---|---|---|---|---|
| **mux (plain)** | 10.2 | 9.5 | 0.262 ms | 22.6 MiB |
| noise | 3.8 | 4.3 | 0.318 ms | 22.3 MiB |
| tls | 4.1 | 4.5 | 0.327 ms | 33.8 MiB |

Encryption halves throughput (hardware AES gives TLS a ~5-8% edge over
Noise's software ChaCha20); RTT cost stays sub-millisecond; TLS carries
+50% memory. In weak cells the encrypted rows track the plain row.

### Methodology

- **Setup**: everything on one machine's loopback; the four hops
  (visitor, server, client, backend) are processes on the same host, so
  absolute numbers are host-dependent — comparisons are same-host and
  same-methodology only.
- **Cells**: loopback, rtt10, rtt100, loss1%, loss5%, loss2%-burst — netem
  shapes the whole `lo`, so every hop is delayed/lossy; a "10 ms" cell
  shows ~100 ms echo RTT because the path is multi-leg. Without
  `CAP_NET_ADMIN`, rtt cells fall back to a userspace delay proxy and loss
  cells are skipped.
- **Metrics**: TCP throughput (1 and 8 streams; median of reps with the
  median rep's retransmits), connection-path RTT (300 fresh connects per
  arm), steady data-path RTT (200 pings on one connection), UDP session
  quality (RTT/loss/jitter/max inter-packet gap over one session), a
  head-of-line probe (saturating bulk + game-like pinger through the same
  service), and RSS (0.5 s sampling of server+client).
- **Discipline**: every comparison varies **one variable** (plain-TCP axis,
  multiplexing on/off, transport) with a shared control; arms run
  **serially** in isolated port bands with fresh processes (parallel runs
  would compete for CPU and invalidate the numbers); molehill arms run 3
  reps, peers 1.
- **Known limits**: loopback is not a real network (idealized loss/delay,
  no real congestion); the shared qdisc dilutes drops — the UDP loss column
  is the residual share a light session sees, not the configured rate; the
  `pool_size=8` connection pool hides per-connect handshake costs (mux's
  connect advantage is not visible here); connection-resource metrics
  (FDs, NAT mappings, TIME_WAIT) — mux's main benefit — are not measured.
- **Reproduce**: `just bench-peers` → `just bench` → `just bench-plot` →
  `just bench-check` (raw data in `benches/scripts/bench/results-v0.7.2.json`;
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
bind_addr = "0.0.0.0:2333" # Port that molehill listens for clients
default_token = "use_a_secret_that_only_you_know" # Shared secret with your clients

# Master switch for dynamic registrations: only ports covered here can be claimed by clients
allow_ports = ["5202"]
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
remote_addr = "myserver.com:2333" # The address of the server. The port must match `server.bind_addr`
default_token = "use_a_secret_that_only_you_know" # Must match the server's `default_token`

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

So you can `ssh myserver.com:5202` to ssh to your NAS.

To run `molehill` as a background service on Linux, checkout the
[systemd examples](./examples/systemd) or the
[container examples](./examples/container).

## Configuration

`molehill` determines the running mode (server/client) from the config file
automatically, or you can force it with `--server` / `--client`. The full
configuration specification, logging and tuning options are documented in
[Configuration](./docs/configuration.md). [Example configs](./examples) for
various scenarios are also available.

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

The [systemd examples](./examples/systemd) show how to run molehill as a
systemd service, both as root and rootless, including multiple instances.

### Container

Official multi-arch images (linux/amd64, linux/arm64) are published to
`ghcr.io/niyueee/molehill`. The image is a single static musl binary on
`scratch` (~8 MiB), runs as non-root UID 1000, bundles CA certificates for
TLS verification, and includes the same default feature set as the regular
release builds (multiplexing enabled).

```bash
docker run -v /etc/molehill/server.toml:/app/server.toml:ro \
  ghcr.io/niyueee/molehill:latest server.toml
```

The image contains no configuration — mount your config file and pass its
name as the argument. See the [container examples](./examples/container)
for Docker Compose (`compose.yaml` / `compose.bridge.yaml`) and Podman
Quadlet (`molehill-server.container` / `molehill-client.container`)
deployments.

## Documentation

Using molehill:

- [Configuration](./docs/configuration.md) — full configuration specification, logging, tuning
- [Transport](./docs/transport.md) — TLS and Noise Protocol setup
- [Build guide](./docs/build-guide.md) — build customization, rustls support, minimal binary
- [Internals](./docs/internals.md) — how control/data channels work
- [Examples](./examples) — configs for common scenarios

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
version. Layered git hooks guard every commit and push, and CI runs the
identical chain:

```bash
just setup   # activate git hooks (core.hooksPath githooks) + install check tools
just check   # fmt / secrets / machete / docs / clippy + audit / deny / outdated / test
```

molehill is a community fork of [rathole](https://github.com/rapiz1/rathole);
version numbers continue the upstream line (upstream's last release was
v0.5.0). See [docs/release.md](./docs/release.md) for release mechanics and
[AGENTS.md](./AGENTS.md) for the repository rules.
