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

Peer comparison on one machine (plain TCP, `visitor -> server -> client ->
backend` on loopback; weak-network cells add round-trip time to the
server↔client leg in software). Peers: frp 0.71.0, rathole 0.5.0 (upstream),
bore 0.6.0, chisel 1.10.1.

![Benchmark: molehill 0.7.2 vs peers](assets/benchmark-v0.7.2.png)

### Loopback

Throughput saturates loopback for every tool — the differentiator is
per-connection path overhead and memory:

| Tool | 1-stream (Gbit/s) | 8-stream (Gbit/s) | echo RTT p50 | echo RTT p99 | Memory (avg RSS) |
|---|---|---|---|---|---|
| **molehill 0.7.2** (mux, default) | 44.8 | 62.1 | 0.261 ms | 0.303 ms | 17.1 MiB |
| molehill 0.7.2 (`mux = false`) | 45.8 | 62.7 | 0.220 ms | 0.281 ms | 17.1 MiB |
| rathole 0.5.0 (upstream) | 45.4 | 62.0 | 0.235 ms | 0.312 ms | 19.7 MiB |
| bore 0.6.0 | 45.7 | 62.7 | 0.517 ms | 0.633 ms | **8.1 MiB** |
| chisel 1.10.1 | 45.4 | 62.3 | 0.373 ms | 0.556 ms | 24.5 MiB |
| frp 0.71.0 | 46.2 | 61.9 | 0.379 ms | 0.690 ms | 47.2 MiB |

### Weak network: added RTT on the tunnel leg

The connection path is where pre-established pooled channels pay off. With
`rtt` milliseconds of added round-trip time on the tunnel leg, every visitor
connection costs the pool-free tools one full extra round trip:

| Tool | rtt10: RTT p50 | rtt100: RTT p50 | rtt100: 1-stream (Gbit/s) |
|---|---|---|---|
| **molehill 0.7.2** (mux, default) | **11.2 ms** | **101.7 ms** | 44.8 |
| frp 0.71.0 | 11.4 ms | 102.0 ms | 45.9 |
| rathole 0.5.0 (upstream) | 11.5 ms | 102.0 ms | 46.1 |
| chisel 1.10.1 | 22.1 ms | 202.6 ms | 47.2 |
| bore 0.6.0 | 22.5 ms | 203.2 ms | 44.0 |

- molehill/frp/rathole keep ~1.2 ms of per-connection overhead on top of the
  added RTT (pre-established data channels); bore/chisel pay ~2× the added
  RTT per connection — one full round trip dialed through the tunnel each
  time a visitor connects.
- Absolute loopback throughput is host-dependent (it saturates the machine);
  treat it as a ceiling sanity check, not a differentiator.
- Loss cells (netem) require `CAP_NET_ADMIN` and are skipped when unavailable
  (see docs/release.md).
- Reproduce: `just bench` → `just bench-plot` → `just bench-check`
  (raw data in `benches/scripts/bench/results-v0.7.2.json`; ritual and
  regression gate in docs/release.md).

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
