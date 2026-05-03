# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

molehill is a secure, stable, and high-performance reverse proxy for NAT traversal, written in Rust. It allows services behind a NAT/firewall to be exposed to the internet via a publicly accessible server. Think of it as a Rust alternative to frp or ngrok.

- **Language**: Rust (Edition 2024)
- **Toolchain**: Pinned to `1.95.0` in `rust-toolchain.toml`
- **License**: Apache-2.0

## Common Commands

### Build

```bash
# Standard release build
cargo build --release

# Minimal binary size build (~500KiB)
cargo build --profile minimal --no-default-features --features client

# Build with rustls instead of native-tls
cargo build --release --no-default-features --features server,client,rustls,noise,websocket-rustls,hot-reload

# Docker build
docker build --build-arg FEATURES="default" -t molehill .
```

### Test

```bash
# Run tests with default features (native-tls)
cargo test --verbose

# Run tests with rustls
cargo test --verbose --no-default-features --features server,client,rustls,noise,websocket-rustls,hot-reload

# Run a specific integration test by function name
cargo test --test integration_test tcp
cargo test --test integration_test udp

# Run a specific unit test
cargo test <test_name>
```

Integration tests spawn their own echo/pingpong servers internally (see `tests/common/mod.rs`). Test fixture configs live in `tests/for_tcp/`, `tests/for_udp/`, `tests/config_test/`. They do not require external services to be running.

### Lint

```bash
# Run all checks at once (requires just, cargo-audit, cargo-machete)
just check

# Individual checks
cargo clippy -- -D warnings
cargo fmt --check
cargo audit
cargo machete

# Check all feature combinations
cargo install cargo-hack
cargo hack check --feature-powerset --no-dev-deps --mutually-exclusive-features default,native-tls,websocket-native-tls,rustls,websocket-rustls
```

### Run

```bash
# Run from source (development)
cargo run -- server.toml
cargo run -- client.toml

# Generate a Noise keypair (requires `noise` feature; default x25519; optionally pass x448)
cargo run -- --genkey
cargo run --features noise -- --genkey x448

# Run compiled binary as server
./molehill server.toml

# Run compiled binary as client
./molehill client.toml

# Run with explicit mode (when config has both client and server)
./molehill --server unified.toml
./molehill --client unified.toml

# Control logging
RUST_LOG=debug ./molehill config.toml
```

### Benchmark

```bash
# Benchmarks live in benches/scripts (iperf3-based stress testing)
# See examples/iperf3/ for benchmark config templates
```

### Configuration Examples

Example configs for various scenarios live in `examples/`:

| Directory | Description |
|-----------|-------------|
| `tls/` | TLS transport with self-signed certificates |
| `noise_nk/` | Noise NK pattern |
| `udp/` | UDP service forwarding |
| `use_proxy/` | Outbound connections via SOCKS5/HTTP CONNECT proxy |
| `minimal/` | Minimal config for embedded devices |
| `iperf3/` | Performance benchmark setup |
| `unified/` | Combined server+client config in one file |
| `systemd/` | Systemd service unit files |

## Architecture

### Core Concepts

- **Service**: The entity whose traffic needs forwarding (e.g., an SSH server)
- **Server**: Publicly accessible host running molehill in server mode
- **Client**: Host behind NAT running molehill in client mode
- **Control Channel**: A TCP connection carrying control commands for one service
- **Data Channel**: A TCP connection carrying forwarded data for one service

### Source Structure

Uses the Rust 2018 module layout (`src/foo.rs` + `src/foo/`) rather than the older `mod.rs` convention.

- `build.rs`: Build metadata injection via `vergen` (git SHA, build timestamp, cargo features, target triple)
- `src/main.rs`: Binary entry point — CLI parsing (clap), signal handling, logging setup
- `src/lib.rs`: Library root — re-exports public API, run mode detection (`determine_run_mode`), main event loop. `lib.rs` also owns the config watcher lifecycle: on `ConfigChange::General` it restarts the instance, on service-level changes it forwards via mpsc.
- `src/cli.rs`: CLI argument definitions (clap derive)
- `src/protocol.rs`: Wire protocol definitions (Hello, Auth, Ack, ControlChannelCmd, DataChannelCmd, UdpTraffic). Serialized with postcard (compact binary). Protocol versioning via `CURRENT_PROTO_VERSION`.
- `src/common.rs` + `src/common/`: Shared utilities
  - `constants.rs`: Timeouts, buffer sizes (UDP_BUFFER_SIZE), backoff strategies
  - `helper.rs`: DNS resolution, keepalive, UDP connect, retry helpers
  - `multi_map.rs`: Multi-value hash map (used in server for connection pools)
- `src/config.rs` + `src/config/`: Configuration
  - `parsing.rs`: TOML parsing/validation with serde. Key types: `Config`, `ClientConfig`, `ServerConfig`, `ClientServiceConfig`, `ServerServiceConfig`. Uses `MaskedString` to avoid leaking tokens in debug logs.
  - `watcher.rs`: Hot-reload file watcher (behind `hot-reload` feature). Sends `ConfigChange` variants over an mpsc channel.
- `src/core.rs` + `src/core/`: Client and server implementations
  - `client.rs`: Client mode (feature-gated on `client`). `Client<T: Transport>` generic struct. Handles control channel setup, authentication, data channel requests from server.
  - `server.rs`: Server mode (feature-gated on `server`). `Server<T: Transport>` generic struct. Listens for visitors, creates data channels, manages connection pools. Key constants: `TCP_POOL_SIZE=8`, `UDP_POOL_SIZE=2`, `CHAN_SIZE=2048`.
- `src/transport.rs` + `src/transport/`: Transport layer
  - `tcp.rs`: Plain TCP transport
  - `native_tls.rs` / `rustls.rs`: TLS transports (mutually exclusive features via compile_error! macro)
  - `noise.rs`: Noise Protocol transport (KK pattern by default, supports x25519 and x448)
  - `websocket.rs`: WebSocket transport (feature-gated)

### Key Design Patterns

1. **Transport Trait**: All transports implement the `Transport` trait (`bind`, `accept`, `handshake`, `connect`). The client and server are generic over `Transport`. Transport selection happens at the config level — `Client<TcpTransport>`, `Client<TlsTransport>`, etc.

2. **Generic Client/Server**: `Client<T: Transport>` and `Server<T: Transport>` are generic structs. The concrete transport is instantiated via match on `TransportType` in `run_client`/`run_server`.

3. **Feature-Gated Compilation**: Major functionality is behind Cargo features:
   - `server` / `client`: Enable respective modes
   - `native-tls` / `rustls`: TLS backends (mutually exclusive, enforced by `compile_error!`)
   - `noise`: Noise Protocol encryption
   - `websocket-native-tls` / `websocket-rustls`: WebSocket support
   - `hot-reload`: Config file watching
   - `embedded`: Minimal feature set for embedded devices

4. **Protocol Flow**:
   - Client establishes a control channel to the server for each service
   - Server challenges client with a nonce; client authenticates with a service token
   - When a visitor connects to the server's `bind_addr`, the server sends `CreateDataChannel` via the control channel
   - Client connects back to create a data channel
   - Server also pre-creates data channels to reduce latency
   - For UDP services, traffic is framed with a `UdpHeader` (source address + length) and sent over the data channel

5. **Config Watcher**: `lib.rs` spawns a `ConfigWatcherHandle` that monitors the config file. General config changes trigger a full restart; service-level changes are sent via an mpsc channel to the running instance for hot updates.

6. **MaskedString**: Sensitive values (tokens, private keys) use `MaskedString` which implements `Debug` as `"MASKED"` to prevent accidental leakage in logs.

7. **Proxy Support**: Outbound connections can go through SOCKS5 or HTTP CONNECT proxies, configured via the `proxy` field on `TcpConfig`. Handled transparently during transport connection.

8. **Connection Pooling**: The server maintains pools of pre-established data channels (`TCP_POOL_SIZE = 8` for TCP, `UDP_POOL_SIZE = 2` for UDP) to reduce connection latency for new visitors.

## Feature Flags

| Feature | Description |
|---------|-------------|
| `server` | Enable server mode |
| `client` | Enable client mode |
| `native-tls` | TLS via native-tls (OpenSSL/Secure Transport/Schannel) |
| `rustls` | TLS via rustls (pure Rust) |
| `noise` | Noise Protocol encryption via snowstorm |
| `websocket-native-tls` | WebSocket transport with native-tls |
| `websocket-rustls` | WebSocket transport with rustls |
| `hot-reload` | Configuration file hot-reloading |
| `embedded` | Minimal feature set for embedded devices |
| `console` | Tokio console debugging support |

**Note**: `native-tls` and `rustls` are mutually exclusive. Same for their websocket variants.

## Proxy Support

molehill supports outbound connections through proxies for environments where direct TCP connections to the server are blocked:

- **SOCKS5** via `async-socks5`
- **HTTP CONNECT** via `async-http-proxy` (supports basic auth)

Configured per-service in the `[client.services.<name>.transport.tcp]` section with a `proxy` URL field (e.g., `proxy = "socks5://127.0.0.1:1080"` or `proxy = "http://user:pass@proxy:8080"`).

## Test Infrastructure

- **Integration tests** (`tests/integration_test.rs`): Spawn a real molehill server + client pair and hit them with echo/pingpong hitters. Test both TCP and UDP forwarding across all transport types (tcp, tls, noise, websocket).
- **Shared test helpers** (`tests/common/mod.rs`): Echo servers, ping-pong servers, and molehill runner functions. Tests are self-contained — no external services needed.
- **Test fixtures**: Transport-specific configs in `tests/for_tcp/` and `tests/for_udp/`.
- **Config validation tests** (`tests/config_test/`): Valid and invalid config files for parsing tests.

## Build Profiles

- `dev`: Default cargo dev profile (empty `[profile.dev]` in Cargo.toml)
- `release`: `lto = true`, `codegen-units = 1`, `strip = true`, `panic = "abort"`
- `minimal`: Inherits release, `opt-level = "z"` for smallest binary size (~500KiB)
- `bench`: `debug = 1`

## CI/CD

GitHub Actions in `.github/workflows/`:
- `ci.yml`: Lints (clippy, fmt, cargo-hack, cargo-machete, cargo-audit), builds for Linux/Windows/macOS (x86_64 + aarch64), runs tests
- `release.yml`: Cross-compiles for 14+ targets, publishes Docker images and crates.io

## Additional Documentation

- `docs/transport.md`: Detailed TLS and Noise Protocol setup, including certificate generation and keypair configuration.
- `docs/build-guide.md`: Build customization, rustls support, and binary size minimization.
- `docs/internals.md`: Conceptual overview of control/data channels and forwarding process.

## Known TODOs

No deferred items.
