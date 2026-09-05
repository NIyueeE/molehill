# Configuration

`molehill` can automatically determine to run in the server mode or the client mode, according to the content of the configuration file, if only one of `[server]` and `[client]` block is present, like the example in the [Quickstart](../README.md#quickstart).

But the `[client]` and `[server]` block can also be put in one file. Then on the server side, run `molehill --server config.toml` and on the client side, run `molehill --client config.toml` to explicitly tell `molehill` the running mode.

Before heading to the full configuration specification, it's recommend to skim [the configuration examples](../examples) to get a feeling of the configuration format. A complete, ready-to-use example covering every option is in [examples/full](../examples/full/).

See [Transport](./transport.md) for more details about encryption and the `transport` block.

## How to configure (v0.7+ model)

Since v0.7 the **client owns the service definitions** and the server owns
only the policy:

- The client declares each forwarded service in its own config — including
  the public address it should be exposed at (`remote_bind_addr`).
- The server has **no** per-service configuration. When a client connects, it
  registers its services at runtime; the server validates every registration
  against its `allow_ports` whitelist before exposing anything.
- Both sides authenticate with one shared secret (`default_token`).

A typical setup:

1. Pick a transport — `tcp` (plain), `tls`, `noise`, or `websocket` — and generate any keys or certificates it needs (see [Transport](./transport.md)).
2. Write `server.toml`: `[server]` with `bind_addr`, `default_token` and the `allow_ports` whitelist. That's all.
3. Write `client.toml`: `[client]` with `remote_addr` + the same `default_token`, and one `[client.services.<name>]` block per service: `local_addr` (where your service listens) and `remote_bind_addr` (the public endpoint).
4. Start the server first, then the client. Both run indefinitely; the client retries automatically while the server is unreachable.

> **Migrating from ≤0.6**: delete the whole `[server.services.*]` section; move each service's `bind_addr` into the client's `remote_bind_addr`; replace per-service tokens with `default_token`; add `allow_ports` on the server. Both ends must be upgraded together (the protocol version changed).

Here is the full configuration specification:

```toml
[client]
remote_addr = "example.com:2333" # Necessary. The address of the server
default_token = "change-me" # Necessary. Must match `[server].default_token`
heartbeat_timeout = 40 # Optional. Set to 0 to disable the application-layer heartbeat test. The value must be greater than `server.heartbeat_interval`. Default: 40 seconds
retry_interval = 1 # Optional. The interval between retry to connect to the server. Default: 1 second
prefer_ipv6 = false # Optional. Prefer IPv6 when resolving remote addresses. Default: false
mux = true # Optional. Multiplex every data channel over one physical connection (yamux). Enabled by default when the `multiplex` feature is compiled in (it is part of the default feature set). Set to false to restore one-connection-per-channel behavior
mux_receive_window = 67108864 # Optional. Upper bound of the total yamux receive window per tunnel connection, in bytes. Only with `multiplex`. Default: rust-yamux default (1 GiB)
mux_max_streams = 512 # Optional. Maximum concurrent streams per tunnel connection. Only with `multiplex`. Default: 512

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

[client.services.service1] # A service that needs forwarding. The name identifies the service (shown in logs)
type = "tcp" # Optional. The protocol that needs forwarding. Possible values: ["tcp", "udp"]. Default: "tcp"
local_addr = "127.0.0.1:1081" # Necessary. The address of the local service that needs to be forwarded
remote_bind_addr = "0.0.0.0:8081" # Necessary. The public address this service is exposed at on the server. Must be covered by the server's `allow_ports`
nodelay = true # Optional. TCP_NODELAY for this service's data channels. Default: true even when unset; set `false` to disable
retry_interval = 1 # Optional. The interval between retry to connect to the server. Default: inherits the global config
prefer_ipv6 = false # Optional. Override the `client.prefer_ipv6` per service
pool_size = 8 # Optional. Requested number of pre-established data channels. Defaults: 8 for TCP, 2 for UDP. Clamped by the server's `max_pool_size`. For UDP this shards distinct visitors across channels; each visitor is pinned to one channel (session affinity)
health_check = { type = "tcp", interval = 10, timeout = 3, max_failed = 1 } # Optional. TCP services only. Probes the local service and removes it from the server while it is down (see "Health check" below)

[client.services.service2] # Multiple services can be defined
type = "udp"
local_addr = "127.0.0.1:1082"
remote_bind_addr = "0.0.0.0:8082"
udp_buffer_size = 2048 # Optional. UDP receive buffer in bytes. Default: 2048, maximum 65535
udp_idle_timeout = 60 # Optional. Seconds after which an idle UDP peer mapping is dropped on the client (its local socket, i.e. the source port the local service sees, is recycled with it). Default: 60
udp_sendq_size = 1024 # Optional. Queue size for outbound datagrams per data channel. Default: 1024

[server]
bind_addr = "0.0.0.0:2333" # Necessary. The address that the server listens for clients. Generally only the port needs to be change.
default_token = "change-me" # Necessary. Must match `[client].default_token`
allow_ports = ["6000-6999", "8080"] # Necessary to enable dynamic registration. Empty or missing: ALL registrations are rejected. Privileged ports (<1024) must be listed explicitly
max_pool_size = 16 # Optional. Upper bound applied to every service's requested pool_size. Default: no limit
heartbeat_interval = 30 # Optional. The interval between two application-layer heartbeat. Set to 0 to disable sending heartbeat. Default: 30 seconds
mux_receive_window = 67108864 # Optional. Server-side total yamux receive window per tunnel connection. Only with `multiplex`. Default: rust-yamux default (1 GiB)
mux_max_streams = 512 # Optional. Server-side maximum concurrent streams per tunnel connection. Only with `multiplex`. Default: 512

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
```

## Dynamic service registration

There are no `[server.services.*]` blocks anymore. The lifecycle is:

1. The client authenticates with `default_token`.
2. For each configured service the client sends a `RegisterService` message:
   name, type, `remote_bind_addr`, `pool_size`.
3. The server validates:
   - **whitelist**: the requested port must be covered by `allow_ports`;
     an empty/missing `allow_ports` rejects *every* registration (this is
     how you disable the feature entirely);
   - **privileged ports**: ports below 1024 must be listed explicitly;
   - **conflicts**: if the port is already bound, the registration fails
     with `Port already in use`.
4. On success the server binds the port immediately and starts forwarding.

Rejections are permanent for that service run: the client logs the exact
reason from the server and gives up until you fix the configuration or
restart it. Service names must be unique per server; re-registration from a
restarting client takes over cleanly.

## Multiplexing (`multiplex` feature)

The `multiplex` feature is part of the default feature set. With `mux = true`
(the default), each service opens **one extra connection** after registering
— the *tunnel* — and every subsequent data channel becomes a yamux stream
inside it. This removes the per-connection TCP + TLS/Noise handshake latency
and cuts FD usage under many concurrent visitors.

- The decision belongs to the client alone (`mux = true/false`); the server
  adapts per connection automatically.
- `mux = false` restores the one-connection-per-channel path.
- `mux_receive_window` / `mux_max_streams` bound the buffering and stream
  count per tunnel; sensible defaults apply when unset.
- Building without the feature removes the option entirely and always uses
  the one-connection-per-channel path.

The wire-level design — tunnel upgrade, per-stream framing and windows, and
why pooled streams need a SYN kick — is in [Internals](./internals.md).

## Logging

`molehill`, like many other Rust programs, use environment variables to control the logging level. `info`, `warn`, `error`, `debug`, `trace` are available.

```shell
RUST_LOG=error ./molehill config.toml
```

will run `molehill` with only error level logging.

If `RUST_LOG` is not present, the default logging level is `info`.

Log lines carry colored levels (red ERROR, yellow WARN, green INFO, cyan DEBUG, purple TRACE) and the active span context, e.g. `handle{service=ssh}:`, so every line of a busy server tells you which service produced it. Colors are enabled only on terminals; redirected output stays plain (also honoring `NO_COLOR`). At `debug`/`trace` level the source module is appended to each line.

## Tuning

From v0.4.7, molehill enables TCP_NODELAY by default on the transport-level connections, and now by default on every leg of a forwarded service as well: both ends of each data channel, the visitor-facing sockets, and the client's connection towards the local service. This benefits latency and interactive applications like SSH, rdp, Minecraft servers. However, it slightly decreases the bandwidth.

TCP keepalive is also enabled by default on these sockets (20s idle time, 8s probe interval), so pooled data channels that were silently dropped by NATs or middleboxes are detected instead of being handed out to visitors.

If the bandwidth is more important, TCP_NODELAY can be opted out with `nodelay = false` per service.

## Usage notes

### Network requirements

- The **server** must be reachable from the Internet: `server.bind_addr` and every registered `remote_bind_addr` need inbound access (open the ports in the firewall or port-forward them on the public server).
- The **client** only needs outbound access to `server.bind_addr`; no inbound port is required behind the NAT.
- `client.remote_addr` must use the same port as `server.bind_addr`.

### Security

- The shared token is mandatory. Use long random values.
- `allow_ports` is your authorization boundary: only list what clients genuinely need. Without it, the server exposes nothing regardless of what clients request.
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

- The datagram limit follows the service's `udp_buffer_size` (default 2048 bytes, up to 65535); larger datagrams are dropped while the channel stays usable. Configure it identically on the service and remember that the server enforces its own copy received at registration time.
- **Session affinity**: all datagrams from one visitor address travel a single data channel and leave the client through one dedicated local socket for the visitor's whole session, so stateful UDP services (game servers like Minecraft Bedrock/RakNet, QUIC, WireGuard, ...) see a stable `(ip, port)` and their sessions stay intact. `pool_size` shards *distinct visitors* across channels for parallelism; it never splits one visitor across channels.
- A mapping (and its local socket) is cleaned up after `udp_idle_timeout` seconds (default 60) without traffic in either direction; the next datagram re-binds a fresh socket, which changes the source port the local service sees. Keep the default or raise it for long-lived stateful sessions.
- `health_check` does not apply to UDP services.

### Transports

Transport setup — TLS certificates (including the rustls legacy-PBE caveat),
Noise keypairs, patterns and PSKs, and the WebSocket transport — is
documented step by step in [Transport](./transport.md): pick the `type` in
the specification above and follow that guide.

- **Proxy**: `proxy` only applies to the client's outbound connections to the
  server (control channel, tunnel, and non-multiplexed data channels). Both
  `socks5` and `http` (CONNECT), with optional basic auth, are supported.

### Hot reload

- Editing the client config while running: general changes (transport, addresses, tokens, mux settings) restart the instance; adding, removing, or modifying a service applies without a restart (services are unregistered/re-registered over the existing control channels).
- Keep the file valid while editing — an invalid config is rejected at startup or reload, and the previous state keeps running.

### Multiple services and instances

- One client config can forward many services (multiple `[client.services.<name>]` blocks), and several clients can connect to the same server. Each registered service name must be unique across all clients of a server.
- To run several independent molehill pairs on one host, use different ports for `bind_addr` and separate config files (the systemd examples show templated instances).

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `Server rejected service <name>: Port N rejected ... allow_ports` | The requested `remote_bind_addr` port is not whitelisted on the server, or the server has dynamic registration disabled. Fix `allow_ports`. |
| `Port N is already in use` | Another service (or another program) holds that port on the server. Pick a different `remote_bind_addr` port. |
| `Protocol version mismatched ... Please update` | One side runs an older molehill. Upgrade both ends together (protocol v2 since 0.7.0). |
| `Authentication failed` on the client | `default_token` differs between client and server. |
| `Failed to connect to <addr>: Connection refused` | Server not running, wrong `remote_addr` port, or `server.bind_addr` not reachable. |
| Repeated `Heartbeat timed out` | `heartbeat_timeout <= heartbeat_interval`, or the network path drops the connection. |
| TLS `certificate verify failed` | `trusted_root`/`hostname` mismatch, expired certificate, or missing `trusted_root` for a self-signed setup. |
| Noise handshake fails | Keypairs, `psk`, or pattern mismatch between the two sides. |
| `Proxy URL is missing the port` at startup | The `proxy` URL lacks a port; fix the config. |
| UDP traffic not flowing | Check `type = "udp"`; datagrams larger than `udp_buffer_size` are dropped; idle mappings time out after `udp_idle_timeout` seconds. |
| Stateful UDP sessions (games, QUIC, WireGuard) break mid-session | Ensure both ends run a version with UDP session affinity (≥ this fix); a peer whose traffic idles longer than `udp_idle_timeout` is re-bound to a fresh local socket (new source port) on the next datagram — raise the timeout or send periodic traffic. |
| `Failed to read cmd: early eof` warnings | The peer closed the channel (restart or shutdown); the client reconnects automatically. |
