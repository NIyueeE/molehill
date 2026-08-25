# Configuration

`molehill` can automatically determine to run in the server mode or the client mode, according to the content of the configuration file, if only one of `[server]` and `[client]` block is present, like the example in the [Quickstart](../README.md#quickstart).

But the `[client]` and `[server]` block can also be put in one file. Then on the server side, run `molehill --server config.toml` and on the client side, run `molehill --client config.toml` to explicitly tell `molehill` the running mode.

Before heading to the full configuration specification, it's recommend to skim [the configuration examples](../examples) to get a feeling of the configuration format. A complete, ready-to-use example covering every option is in [examples/full](../examples/full/).

See [Transport](./transport.md) for more details about encryption and the `transport` block.

## How to configure

The client and server configs must agree on three things:

1. **The transport type**: `[client.transport].type` and `[server.transport].type` must be identical.
2. **The service names**: `[client.services.<name>]` and `[server.services.<name>]` must use the same `<name>` on both sides.
3. **The tokens**: each service's token on the client side must equal the one on the server side (or both sides use the same `default_token`).

A typical setup:

1. Pick a transport — `tcp` (plain), `tls`, `noise`, or `websocket` — and generate any keys or certificates it needs (see [Transport](./transport.md)).
2. Write `server.toml` with `[server]`, `[server.transport]`, and one `[server.services.<name>]` block per service. `server.bind_addr` is the address clients connect to; each service's `bind_addr` is the port visitors will use.
3. Write `client.toml` with `[client]`, `[client.transport]`, and matching `[client.services.<name>]` blocks. `client.remote_addr` must point at the server's `bind_addr` (host:port); `local_addr` is where the local service listens.
4. Start the server first, then the client. Both run indefinitely; the client retries automatically while the server is unreachable.

A complete, ready-to-use example covering every option is in [examples/full](../examples/full/).

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
nodelay = true # Optional. TCP_NODELAY for this service's data channels. Default: true even when unset; set `false` to disable
retry_interval = 1 # Optional. The interval between retry to connect to the server. Default: inherits the global config
prefer_ipv6 = false # Optional. Override the `client.prefer_ipv6` per service
health_check = { type = "tcp", interval = 10, timeout = 3, max_failed = 1 } # Optional. TCP services only. Probes the local service and removes it from the server while it is down (see "Health check" below)

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
nodelay = true # Optional. Same as the client. Default: true even when unset; set `false` to disable

[server.services.service2]
bind_addr = "0.0.0.1:8082"
```

## Logging

`molehill`, like many other Rust programs, use environment variables to control the logging level. `info`, `warn`, `error`, `debug`, `trace` are available.

```shell
RUST_LOG=error ./molehill config.toml
```

will run `molehill` with only error level logging.

If `RUST_LOG` is not present, the default logging level is `info`.

## Tuning

From v0.4.7, molehill enables TCP_NODELAY by default on the transport-level connections, and now by default on every leg of a forwarded service as well: both ends of each data channel, the visitor-facing sockets, and the client's connection towards the local service. This benefits latency and interactive applications like SSH, rdp, Minecraft servers. However, it slightly decreases the bandwidth.

TCP keepalive is also enabled by default on these sockets (20s idle time, 8s probe interval), so pooled data channels that were silently dropped by NATs or middleboxes are detected instead of being handed out to visitors.

If the bandwidth is more important, TCP_NODELAY can be opted out with `nodelay = false` per service.

## Usage notes

### Network requirements

- The **server** must be reachable from the Internet: `server.bind_addr` and every service `bind_addr` need inbound access (open the ports in the firewall or port-forward them on the public server).
- The **client** only needs outbound access to `server.bind_addr`; no inbound port is required behind the NAT.
- `client.remote_addr` must use the same port as `server.bind_addr`.

### Security

- Tokens are mandatory and per-service. Use long random values; both sides must agree on them.
- The config file contains tokens in plain text, so restrict its permissions (e.g. `chmod 600 config.toml`). Tokens are masked (`MASKED`) in logs.
- Use the `noise` or `tls` transport when traffic traverses untrusted networks; plain `tcp` forwards unencrypted.
- The PKCS#12 file and Noise private keys are secrets too.

### Heartbeat

- `client.heartbeat_timeout` must be greater than `server.heartbeat_interval`, otherwise the client treats a healthy server as dead and reconnects in a loop.
- Set `heartbeat_interval = 0` to disable heartbeats (then set `heartbeat_timeout = 0` as well).

### Health check

- `health_check` is optional and only supported on TCP services. It makes the client probe `local_addr` every `interval` seconds (default 10) with a `timeout` of `timeout` seconds (default 3). After `max_failed` consecutive failed probes (default 1) the service is declared unhealthy: its control channel is dropped, so the server stops serving it and visitors fail fast instead of being forwarded to a dead local service. Once a probe succeeds again, the client re-registers the service automatically.
- Two probe types: `type = "tcp"` (default) opens a TCP connection to the service; `type = "http"` sends an HTTP GET to `http_path` (default `/`) and accepts any 2xx/3xx response.
- Example: `health_check = { type = "http", interval = 5, timeout = 2, max_failed = 3, http_path = "/healthz" }`.

### UDP services

- UDP datagrams are limited by the internal buffer size (2048 bytes); larger datagrams are dropped (the packet is discarded and the channel stays usable). This fits typical DNS/NTP/game traffic but not jumbo payloads.
- UDP forwarding is connectionless: the client maps visitor addresses to local sockets and cleans up idle mappings after 60 seconds.
- `health_check` does not apply to UDP services.

### Transport specifics

- **TLS**: with rustls builds, the server's PKCS#12 archive must use legacy PBE algorithms (with openssl 3, add `-legacy` when creating it). The client either pins `trusted_root` or falls back to the system certificate store.
- **Noise**: generate keypairs with `molehill --genkey`. The server holds `local_private_key`; the client holds the server's `remote_public_key`. A `psk` must be 32 bytes, base64-encoded, identical on both sides, and the pattern must include a PSK modifier.
- **Proxy**: `proxy` only applies to the client's outbound connection to the server. Both `socks5` and `http` (CONNECT), with optional basic auth, are supported.

### Hot reload

- Editing the config file while running: general changes (transport, addresses, tokens) restart the instance; adding, removing, or modifying a service applies without a restart.
- Keep the file valid while editing — an invalid config is rejected at startup or reload, and the previous state keeps running.

### Multiple services and instances

- One client config can forward many services (multiple `[client.services.<name>]` blocks), and several clients can connect to the same server for different services.
- To run several independent molehill pairs on one host, use different ports for `bind_addr` and separate config files (the systemd examples show templated instances).

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `Authentication failed: <service>` on the client | Token mismatch between client and server for that service. |
| `No such a service` / handshake rejected | Service names differ between the two configs. |
| `Failed to connect to <addr>: Connection refused` | Server not running, wrong `remote_addr` port, or `server.bind_addr` not reachable. |
| Repeated `Heartbeat timed out` | `heartbeat_timeout <= heartbeat_interval`, or the network path drops the connection. |
| TLS `certificate verify failed` | `trusted_root`/`hostname` mismatch, expired certificate, or missing `trusted_root` for a self-signed setup. |
| Noise handshake fails | Keypairs, `psk`, or pattern mismatch between the two sides. |
| `Proxy URL is missing the port` at startup | The `proxy` URL lacks a port; fix the config. |
| UDP traffic not flowing | Check `type = "udp"` on both sides; remember the 2048-byte datagram limit; idle mappings time out after 60 seconds. |
| `Failed to read cmd: early eof` warnings | The peer closed the channel (restart or shutdown); the client reconnects automatically. |
