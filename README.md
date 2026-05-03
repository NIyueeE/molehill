<p align="center">
  <img src="https://raw.githubusercontent.com/NIyueeE/molehill/main/assets/molehill.svg" width="81" height="81">
</p>

<h1 align="center">molehill</h1>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/NIyueeE/molehill.svg"></a>
  <img src="https://img.shields.io/github/v/release/NIyueeE/molehill.svg">
  <img src="https://img.shields.io/badge/rust-1.95.0+-93450a.svg">
  <img src="https://github.com/NIyueeE/molehill/actions/workflows/ci.yml/badge.svg">
</p>
<p align="center">
  <img src="https://img.shields.io/github/stars/NIyueeE/molehill.svg">
  <img src="https://img.shields.io/github/forks/NIyueeE/molehill.svg">
  <img src="https://img.shields.io/github/last-commit/NIyueeE/molehill.svg">
</p>

<p align="center">A community-maintained fork of <a href="https://github.com/rapiz1/rathole">rathole</a>.</p>

[English](README.md) | [简体中文](README-zh.md)

molehill, like [frp](https://github.com/fatedier/frp) and [ngrok](https://github.com/inconshreveable/ngrok), can help to expose the service on the device behind the NAT to the Internet, via a server with a public IP.

<!-- TOC -->

- [molehill](#molehill)
  - [Features](#features)
  - [Quickstart](#quickstart)
  - [Configuration](#configuration)
    - [Logging](#logging)
    - [Tuning](#tuning)
  - [Planning](#planning)

<!-- /TOC -->

## Features

- **High Performance** Much higher throughput can be achieved than frp, and more stable when handling a large volume of connections.
- **Low Resource Consumption** Consumes much fewer memory than similar tools. [The binary can be](docs/build-guide.md) **as small as ~500KiB** to fit the constraints of devices, like embedded devices as routers.
- **Security** Tokens of services are mandatory and service-wise. The server and clients are responsible for their own configs. With the optional Noise Protocol, encryption can be configured at ease. No need to create a self-signed certificate! TLS is also supported.
- **Hot Reload** Services can be added or removed dynamically by hot-reloading the configuration file. HTTP API is WIP.

## Quickstart

A full-powered `molehill` can be obtained from the [release](https://github.com/NIyueeE/molehill/releases) page. Or [build from source](docs/build-guide.md) **for other platforms and minimizing the binary**.

The usage of `molehill` is very similar to frp. If you have experience with the latter, then the configuration is very easy for you. The only difference is that configuration of a service is split into the client side and the server side, and a token is mandatory.

To use `molehill`, you need a server with a public IP, and a device behind the NAT, where some services that need to be exposed to the Internet.

Assuming you have a NAS at home behind the NAT, and want to expose its ssh service to the Internet:

1. On the server which has a public IP

Create `server.toml` with the following content and accommodate it to your needs.

```toml
# server.toml
[server]
bind_addr = "0.0.0.0:2333" # `2333` specifies the port that molehill listens for clients

[server.services.my_nas_ssh]
token = "use_a_secret_that_only_you_know" # Token that is used to authenticate the client for the service. Change to an arbitrary value.
bind_addr = "0.0.0.0:5202" # `5202` specifies the port that exposes `my_nas_ssh` to the Internet
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
remote_addr = "myserver.com:2333" # The address of the server. The port must be the same with the port in `server.bind_addr`

[client.services.my_nas_ssh]
token = "use_a_secret_that_only_you_know" # Must be the same with the server to pass the validation
local_addr = "127.0.0.1:22" # The address of the service that needs to be forwarded
```

Then run:

```bash
./molehill client.toml
```

3. Now the client will try to connect to the server `myserver.com` on port `2333`, and any traffic to `myserver.com:5202` will be forwarded to the client's port `22`.

So you can `ssh myserver.com:5202` to ssh to your NAS.

To run `molehill` run as a background service on Linux, checkout the [systemd examples](./examples/systemd).

## Configuration

`molehill` can automatically determine to run in the server mode or the client mode, according to the content of the configuration file, if only one of `[server]` and `[client]` block is present, like the example in [Quickstart](#quickstart).

But the `[client]` and `[server]` block can also be put in one file. Then on the server side, run `molehill --server config.toml` and on the client side, run `molehill --client config.toml` to explicitly tell `molehill` the running mode.

Before heading to the full configuration specification, it's recommend to skim [the configuration examples](./examples) to get a feeling of the configuration format.

See [Transport](./docs/transport.md) for more details about encryption and the `transport` block.

Here is the full configuration specification:

```toml
[client]
remote_addr = "example.com:2333" # Necessary. The address of the server
default_token = "default_token_if_not_specify" # Optional. The default token of services, if they don't define their own ones
heartbeat_timeout = 40 # Optional. Set to 0 to disable the application-layer heartbeat test. The value must be greater than `server.heartbeat_interval`. Default: 40 seconds
retry_interval = 1 # Optional. The interval between retry to connect to the server. Default: 1 second
prefer_ipv6 = false # Optional. Prefer IPv6 when resolving remote addresses. Default: false

[client.transport] # The whole block is optional. Specify which transport to use
type = "tcp" # Optional. Possible values: ["tcp", "tls", "noise", "websocket"]. Default: "tcp"

[client.transport.tcp] # Optional. TCP socket options (also apply to `tls` and `noise` transports)
proxy = "socks5://user:passwd@127.0.0.1:1080" # Optional. The proxy used to connect to the server. `http` and `socks5` is supported.
nodelay = true # Optional. Determine whether to enable TCP_NODELAY, if applicable, to improve the latency but decrease the bandwidth. Default: true
keepalive_secs = 20 # Optional. Specify `tcp_keepalive_time` in `tcp(7)`, if applicable. Default: 20 seconds
keepalive_interval = 8 # Optional. Specify `tcp_keepalive_intvl` in `tcp(7)`, if applicable. Default: 8 seconds

[client.transport.tls] # Necessary if `type` is "tls"
trusted_root = "ca.pem" # Necessary. The certificate of CA that signed the server's certificate
hostname = "example.com" # Optional. The hostname that the client uses to validate the certificate. If not set, fallback to `client.remote_addr`

[client.transport.noise] # Noise protocol. See `docs/transport.md` for further explanation
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s" # Optional. Default value as shown
local_private_key = "key_encoded_in_base64" # Optional
remote_public_key = "key_encoded_in_base64" # Optional
psk = "key_encoded_in_base64" # Optional. Pre-shared key (32 bytes, base64-encoded). The pattern must include a PSK modifier (e.g. Noise_KKpsk0_...)
psk_location = 0 # Optional. The PSK slot index used in the pattern. Default: 0

[client.transport.websocket] # Necessary if `type` is "websocket"
tls = true # Necessary. Set to `true` to enable TLS on the WebSocket connection (uses settings from `client.transport.tls`). Set to `false` for plain WebSocket.

[client.services.service1] # A service that needs forwarding. The name `service1` can change arbitrarily, as long as identical to the name in the server's configuration
type = "tcp" # Optional. The protocol that needs forwarding. Possible values: ["tcp", "udp"]. Default: "tcp"
token = "whatever" # Necessary if `client.default_token` not set
local_addr = "127.0.0.1:1081" # Necessary. The address of the service that needs to be forwarded
nodelay = true # Optional. Override the `client.transport.nodelay` per service
retry_interval = 1 # Optional. The interval between retry to connect to the server. Default: inherits the global config
prefer_ipv6 = false # Optional. Override the `client.prefer_ipv6` per service

[client.services.service2] # Multiple services can be defined
local_addr = "127.0.0.1:1082"

[server]
bind_addr = "0.0.0.0:2333" # Necessary. The address that the server listens for clients. Generally only the port needs to be change.
default_token = "default_token_if_not_specify" # Optional
heartbeat_interval = 30 # Optional. The interval between two application-layer heartbeat. Set to 0 to disable sending heartbeat. Default: 30 seconds

[server.transport] # Same as `[client.transport]`
type = "tcp"

[server.transport.tcp] # Same as the client
nodelay = true
keepalive_secs = 20
keepalive_interval = 8

[server.transport.tls] # Necessary if `type` is "tls"
pkcs12 = "identity.pfx" # Necessary. pkcs12 file of server's certificate and private key
pkcs12_password = "password" # Necessary. Password of the pkcs12 file

[server.transport.noise] # Same as `[client.transport.noise]`
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s"
local_private_key = "key_encoded_in_base64"
remote_public_key = "key_encoded_in_base64"
psk = "key_encoded_in_base64" # Optional. Pre-shared key (32 bytes, base64-encoded). The pattern must include a PSK modifier (e.g. Noise_KKpsk0_...)
psk_location = 0 # Optional. The PSK slot index used in the pattern. Default: 0

[server.transport.websocket] # Necessary if `type` is "websocket"
tls = true # Necessary. Set to `true` to enable TLS on the WebSocket connection (uses settings from `server.transport.tls`). Set to `false` for plain WebSocket.

[server.services.service1] # The service name must be identical to the client side
type = "tcp" # Optional. Same as the client `[client.services.X.type]`
token = "whatever" # Necessary if `server.default_token` not set
bind_addr = "0.0.0.0:8081" # Necessary. The address of the service is exposed at. Generally only the port needs to be change.
nodelay = true # Optional. Same as the client

[server.services.service2]
bind_addr = "0.0.0.1:8082"
```

### Logging

`molehill`, like many other Rust programs, use environment variables to control the logging level. `info`, `warn`, `error`, `debug`, `trace` are available.

```shell
RUST_LOG=error ./molehill config.toml
```

will run `molehill` with only error level logging.

If `RUST_LOG` is not present, the default logging level is `info`.

### Tuning

From v0.4.7, molehill enables TCP_NODELAY by default, which should benefit the latency and interactive applications like rdp, Minecraft servers. However, it slightly decreases the bandwidth.

If the bandwidth is more important, TCP_NODELAY can be opted out with `nodelay = false`.

## Planning

- [ ] HTTP APIs for configuration
