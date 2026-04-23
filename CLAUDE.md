# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

molehill is a secure, stable, and high-performance reverse proxy for NAT traversal, written in Rust. It allows services behind a NAT/firewall to be exposed to the internet via a publicly accessible server. Think of it as a Rust alternative to frp or ngrok.

- **Language**: Rust (Edition 2021)
- **Toolchain**: Pinned to `1.71.0` in `rust-toolchain`
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

# Run a specific test
cargo test --test integration_test <test_name>
```

### Lint

```bash
cargo clippy -- -D warnings

# Check all feature combinations
cargo install cargo-hack
cargo hack check --feature-powerset --no-dev-deps --mutually-exclusive-features default,native-tls,websocket-native-tls,rustls,websocket-rustls
```

### Run

```bash
# Generate a Noise keypair
./molehill --genkey

# Run as server
./molehill server.toml

# Run as client
./molehill client.toml

# Run with explicit mode (when config has both client and server)
./molehill --server unified.toml
./molehill --client unified.toml

# Control logging
RUST_LOG=debug ./molehill config.toml
```

## Architecture

### Core Concepts

- **Service**: The entity whose traffic needs forwarding (e.g., an SSH server)
- **Server**: Publicly accessible host running molehill in server mode
- **Client**: Host behind NAT running molehill in client mode
- **Control Channel**: A TCP connection carrying control commands for one service
- **Data Channel**: A TCP connection carrying forwarded data for one service

### Source Structure

- `src/main.rs`: Entry point — CLI parsing (clap), signal handling, logging setup
- `src/lib.rs`: Library root — run mode detection, main event loop, config watcher coordination
- `src/cli.rs`: CLI argument definitions
- `src/config.rs`: Configuration parsing and validation (TOML)
- `src/config_watcher.rs`: Hot-reload config file watcher (behind `hot-reload` feature)
- `src/protocol.rs`: Wire protocol definitions (Hello, Auth, Ack, ControlChannelCmd, DataChannelCmd)
- `src/client.rs`: Client mode implementation
- `src/server.rs`: Server mode implementation
- `src/transport/`: Transport layer implementations
  - `mod.rs`: `Transport` trait definition
  - `tcp.rs`: Plain TCP transport
  - `native_tls.rs` / `rustls.rs`: TLS transports (mutually exclusive features)
  - `noise.rs`: Noise Protocol transport
  - `websocket.rs`: WebSocket transport

### Key Design Patterns

1. **Transport Trait**: All transports implement the `Transport` trait (`bind`, `accept`, `handshake`, `connect`). The client and server are generic over `Transport`.

2. **Feature-Gated Compilation**: Major functionality is behind Cargo features:
   - `server` / `client`: Enable respective modes
   - `native-tls` / `rustls`: TLS backends (mutually exclusive)
   - `noise`: Noise Protocol encryption
   - `websocket-native-tls` / `websocket-rustls`: WebSocket support
   - `hot-reload`: Config file watching
   - `embedded`: Minimal feature set for embedded devices

3. **Protocol Flow**:
   - Client establishes a control channel to the server for each service
   - Server challenges client with a nonce; client authenticates with a service token
   - When a visitor connects to the server's `bind_addr`, the server sends `CreateDataChannel` via the control channel
   - Client connects back to create a data channel
   - Server also pre-creates data channels to reduce latency

4. **Config Watcher**: `lib.rs` spawns a `ConfigWatcherHandle` that monitors the config file. General config changes trigger a full restart; service-level changes are sent via an mpsc channel to the running instance for hot updates.

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

## Build Profiles

- `release`: `lto = true`, `codegen-units = 1`, `strip = true`
- `minimal`: Inherits release, `opt-level = "z"` for smallest binary size
- `bench`: `debug = 1`

## CI/CD

GitHub Actions in `.github/workflows/`:
- `rust.yml`: Lints (clippy, cargo-hack), builds for Linux/Windows/macOS (x86_64 + aarch64), runs tests
- `release.yml`: Cross-compiles for 14+ targets, publishes Docker images and crates.io
