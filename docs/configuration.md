# Configuration

`molehill` can automatically determine to run in the server mode or the client mode, according to the content of the configuration file, if only one of `[server]` and `[client]` block is present, like the example in the [Quickstart](../README.md#quickstart).

But the `[client]` and `[server]` block can also be put in one file. Then on the server side, run `molehill --server config.toml` and on the client side, run `molehill --client config.toml` to explicitly tell `molehill` the running mode.

Before heading to the full configuration specification, it's recommended
to skim the [complete examples](#complete-examples) to get a feeling of the
configuration format.

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

1. Pick a transport — `plain` or `noise` — and, for `noise`, generate a keypair (see [Transport](./transport.md)).
2. Write `server.toml`: `[server]` with `default_token` and the `allow_ports` whitelist, plus `[server.control].bind_addr`. That's all.
3. Write `client.toml`: `[client]` with the same `default_token`, `[client.control].default_remote_addr`, and one `[client.services.<name>]` block per service: `local_addr` (where your service listens) and `remote_bind_addr` (the public endpoint).
4. Start the server first, then the client. Both run indefinitely; the client retries automatically while the server is unreachable.

> **Migrating from ≤0.6**: delete the whole `[server.services.*]` section; move each service's `bind_addr` into the client's `remote_bind_addr`; replace per-service tokens with `default_token`; add `allow_ports` on the server. Both ends must be upgraded together (the protocol version changed).

> **Migrating from 0.7.x**: the client keys moved into dedicated blocks and
> the data plane gained an explicit endpoint. Old → new:
> `[client].remote_addr` → `[client.control].default_remote_addr`;
> `[client].heartbeat_timeout` / `retry_interval` →
> `[client.control].default_*`;
> `[client].mux = false` → `[client.data].default_mode = "direct"`;
> `[client].mux = true` → `[client.data].default_mode = "multiplex"` (the
> `mux_receive_window` / `mux_max_streams` knobs are gone — fixed internal
> yamux defaults now);
> `[client.transport].type = "tcp"` → `"plain"`;
> `[client.transport.tcp].proxy` → `[client.transport].proxy`;
> `[client.transport.tcp].nodelay` / `keepalive_secs` / `keepalive_interval`
> were removed (fixed internal defaults; the per-service `nodelay` remains);
> the `tls` and `websocket` transport values were removed in 0.8: only
> `"plain"` and `"noise"` remain, and the `[client.transport.tls]` /
> `[client.transport.websocket]` / `[server.transport.tls]` /
> `[server.transport.websocket]` blocks are gone (migrate to `noise`);
> `[server].bind_addr` → `[server.control].bind_addr` (a separate
> data-plane listener is optional via `[server.data].bind_addr`, defaulting
> to the control listener);
> `[server].heartbeat_interval` → `[server.control].heartbeat_interval`.
> The 0.8 client-side blocks use `default_`-prefixed names for the
> client-wide defaults — `[client.control]` (`default_remote_addr`,
> `default_heartbeat_timeout`, `default_retry_interval`) and `[client.data]`
> (`default_data_addr`, `default_mode`, `default_count`, `default_carrier`) — so
> they read distinctly from the per-service overlay keys on
> `[client.services.<name>]` (`protocol`, `remote_addr`, `token`,
> `heartbeat_timeout`, `retry_interval`, `mode`, `count`, `carrier`,
> `transport`, `udp_forwarder_ipv6`, `udp_send_queue_size`, ...; new in 0.8).
> `[client.transport]` keeps `type`/`noise` unprefixed: its per-service
> overrides live in the nested `transport` table, so there is no same-name
> collision at the service level (the `default_` prefix exists to
> disambiguate exactly that).
> Old keys are rejected (`deny_unknown_fields`), never silently ignored.
>
> **0.8 protocol v3**: every connection starts with a one-byte transport
> selector (`0x00` plain / `0x01` noise) and the registration carries the
> data-plane carrier — both ends must upgrade together; a version mismatch
> is a hard error.

## Choosing your configuration (decision tree)

The defaults — `mode = "multiplex"`, `count = 4`, `carrier = "tcp"`, plain
transport — are the right starting point for almost everyone. Deviate only
when the tree says so. The numbers below are the measured basis of the
v0.8.0 benchmark (same-host loopback and weak-network cells; raw data in
`benches/scripts/bench/results-v0.8.0.json`, charts and tables in the
README's Benchmarks chapter):

```mermaid
flowchart TD
    A["Start: defaults<br/>multiplex, count=4, carrier=tcp, plain"] --> B{"Traffic crosses an<br/>untrusted network?"}
    B -- Yes --> C["transport type = noise<br/>+ keypair (Transport doc)"]
    B -- No --> D{"One service or a few<br/>long-lived connections?"}
    C --> D
    D -- "Yes, raw throughput first" --> E["mode = direct"]
    D -- "No: many services,<br/>many users, churn" --> F{"Many concurrent<br/>connections?"}
    E --> Z["Done - tune per service<br/>via [client.services.*] overrides"]
    F -- "> ~100 concurrent" --> G["count = 8 or higher"]
    F -- Typical --> H["keep count = 4"]
    G --> I{"Path quality?"}
    H --> I
    I -- "High pure latency +<br/>UDP game (100ms+ RTT)" --> J["A/B test carrier = kcp"]
    I -- Otherwise --> Z
    J --> Z
```

### What each choice costs (measured)

| Decision | Option | Measured basis |
|---|---|---|
| `mode` | `"multiplex"` (default) | 1-stream 10.9 Gbit/s on loopback vs 19.3 for `direct`; at 8 streams both reach ~27.7; multiplex absorbs per-connection setup (churn p99 ~3.5 ms) and saves FDs / ports / NAT mappings |
| `mode` | `"direct"` | raw single-stream throughput; one physical tunnel per stream (FD / port / NAT cost scales with stream count) |
| `count` | `1` | single-flow ceiling (loopback 8-str 9.0 Gbit/s); every stream shares one retransmit domain (loss5 HoL max 1157 ms) |
| `count` | `4` (default) | aggregates beyond one flow (loss1 8-str 15.4 vs 4.6 Gbit/s) and isolates head-of-line blocking (rtt10 HoL max 80.6 vs 101.4 ms at count=1); yamux ceiling `count × 32` concurrent connections |
| `count` | `8+` | ~256 concurrent connections; 8 physical tunnels per service (NAT mappings ×8) |
| `carrier` | `"tcp"` (default) | faster in every measured cell (loopback 1-str 4.8 vs 2.5 Gbit/s against the noise control; 8-str 14.8 vs 5.9); RSS 24 vs 102 MiB |
| `carrier` | `"kcp"` | only when TCP data tunnels are blocked or throttled, or A/B for a UDP game on a high-latency path: its one measured win is UDP session quality at rtt100 (0% loss, 20 ms max inter-packet gap vs 100+ ms for the TCP arms) |
| transport | `"plain"` | 10.9 / 27.7 Gbit/s (1/8 streams) on loopback |
| transport | `"noise"` | 4.8 / 14.8 Gbit/s; sub-millisecond RTT cost; CPU parity under full load |
| `pool_size` | 8 TCP / 2 UDP (defaults) | setup-to-first-byte p99 ~3.5 ms at 16-way churn; UDP shards distinct visitors across channels, never splits one session (session affinity) |

**Validate the choice** with the exposure you care about: `ping` / in-game
feel for latency, `iperf3` on the exposed port for raw throughput, and the
real traffic of your service. For local A/B of configurations,
`just bench-fast` runs a ~2-minute molehill-only benchmark matrix.

Here is the full configuration specification:

```toml
[client]
default_token = "change-me" # Necessary. Must match `[server].default_token`

[client.control] # Necessary. Control-channel defaults: authentication, registration, heartbeat
default_remote_addr = "example.com:2333" # Necessary. The address of the server
default_heartbeat_timeout = 40 # Optional. Set to 0 to disable the application-layer heartbeat test. The value must be greater than `server.control.heartbeat_interval`. Default: 40 seconds
default_retry_interval = 1 # Optional. The interval between retries to connect to the server. Default: 1 second

[client.data] # Optional. Data-plane defaults for every service (feature `multiplex`, part of the default build). Each service can override default_mode/default_count/default_carrier individually — see the per-service keys in `[client.services.*]` below
# default_data_addr = "example.com:2343" # Optional. Data-plane endpoint; defaults to the service's control endpoint (`client.services.<name>.remote_addr` when set, else `client.control.default_remote_addr`). With `default_carrier = "kcp"` the KCP sessions dial the control address over UDP — TCP control and UDP KCP can share one port (distinct protocols)
default_mode = "multiplex" # Optional. Default data-plane mode: "multiplex" (default) or "direct" (one connection per data channel; `count`/`carrier` do not apply)
default_count = 4 # Optional. Default parallel tunnel connections per service; only with `default_mode = "multiplex"`. Default: 4 (throughput + head-of-line isolation beyond a single TCP flow; 1 = single-tunnel behavior), clamped to 1..=64
default_carrier = "tcp" # Optional. Default data carrier: "tcp" (default) rides the control channel's wire stack; "kcp" uses KCP-over-UDP sessions (feature `kcp`; the server opens its KCP listener lazily on the first `kcp` registration — no server-side opt-in). Both transport types compose with KCP: with `noise` the same Noise handshake wraps each KCP session, with `plain` the session stays unencrypted

[client.transport] # Optional. How the wire is wrapped; applies to both planes
type = "plain" # Optional. Possible values: ["plain", "noise"]. Default: "plain"
proxy = "socks5://user:passwd@127.0.0.1:1080" # Optional. Client only. Connect to the server through an `http`/`socks5` proxy

[client.transport.noise] # Noise protocol. See `docs/transport.md` for further explanation
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s" # Optional. Default value as shown
local_private_key = "key_encoded_in_base64" # Optional
remote_public_key = "key_encoded_in_base64" # Optional
psk = "key_encoded_in_base64" # Optional. Pre-shared key (32 bytes, base64-encoded). The pattern must include a PSK modifier (e.g. Noise_KKpsk0_...)
psk_location = 0 # Optional. The PSK slot index used in the pattern. Default: 0

[client.services.service1] # A service that needs forwarding. The name identifies the service (shown in logs)
protocol = "tcp" # Optional. The protocol that needs forwarding. Possible values: ["tcp", "udp"]. Default: "tcp"
local_addr = "127.0.0.1:1081" # Necessary. The address of the local service that needs to be forwarded
remote_bind_addr = "0.0.0.0:8081" # Necessary. The public address this service is exposed at on the server. Must be covered by the server's `allow_ports`
nodelay = true # Optional. TCP_NODELAY for this service's data channels. Default: true even when unset; set `false` to disable
retry_interval = 1 # Optional. The interval between retry to connect to the server. Default: inherits `client.control.default_retry_interval`
token = "service-specific-token" # Optional. Override `client.default_token` for this service only — e.g. to authenticate against a server that has its own token # security-scan:allow documentation placeholder
remote_addr = "server2.example.com:2333" # Optional. Override `client.control.default_remote_addr` for this service only — its control channel (and, by default, its data plane) dials this server. Lets one client spread services across several molehill servers
heartbeat_timeout = 60 # Optional. Override `client.control.default_heartbeat_timeout` for this service only — e.g. when the service runs against a server with a different heartbeat interval
udp_forwarder_ipv6 = false # Optional. Prefer IPv6 for the UDP forwarder's connection to the local service (UDP services only). Default: false
mode = "multiplex" # Optional. Override `client.data.default_mode` for this service only. "multiplex" (default) or "direct"
count = 4 # Optional. Override `client.data.default_count` for this service only; valid only with `mode = "multiplex"`. Inherits the default when unset
carrier = "tcp" # Optional. Override `client.data.default_carrier` for this service only; valid only with `mode = "multiplex"`. Inherits the default when unset
transport = { type = "plain" } # Optional. Per-service transport override: `type` ("noise" = encrypt, "plain" = plaintext; unset = follow `client.transport.type`) and `noise` keys (used when this service is encrypted; unset = use `client.transport.noise`). Lets one client run plain and encrypted services side by side — e.g. a service dialing a different server with its own public key
pool_size = 8 # Optional. Requested number of pre-established data channels. Defaults: 8 for TCP, 2 for UDP. Clamped by the server's `max_pool_size`. For UDP this shards distinct visitors across channels; each visitor is pinned to one channel (session affinity)
health_check = { type = "tcp", interval = 10, timeout = 3, max_failed = 1 } # Optional. TCP services only. Probes the local service and removes it from the server while it is down (see "Health check" below)

[client.services.service2] # Multiple services can be defined
protocol = "udp"
local_addr = "127.0.0.1:1082"
remote_bind_addr = "0.0.0.0:8082"
udp_buffer_size = 2048 # Optional. UDP receive buffer in bytes. Default: 2048, maximum 65535
udp_idle_timeout = 60 # Optional. Seconds after which an idle UDP peer mapping is dropped on the client (its local socket, i.e. the source port the local service sees, is recycled with it). Default: 60
udp_send_queue_size = 1024 # Optional. Queue size for outbound datagrams per data channel. Default: 1024

[server]
default_token = "change-me" # Necessary. Must match `[client].default_token`
allow_ports = ["6000-6999", "8080"] # Necessary to enable dynamic registration. Empty or missing: ALL registrations are rejected. Privileged ports (<1024) must be listed explicitly
max_pool_size = 16 # Optional. Upper bound applied to every service's requested pool_size. Default: no limit

[server.control] # Necessary. Control-channel listener
bind_addr = "0.0.0.0:2333" # Necessary. The address that the server listens for clients. Generally only the port needs to be changed
heartbeat_interval = 30 # Optional. The interval between two application-layer heartbeats. Set to 0 to disable sending heartbeats. Default: 30 seconds

[server.data] # Optional. Data-plane listener (feature `multiplex`)
# bind_addr = "0.0.0.0:2343" # Optional. Data-plane listener; defaults to `server.control.bind_addr`. The KCP UDP listener binds here too on the first `kcp` registration — with the default address, TCP control and UDP KCP coexist on one port (distinct protocols)

[server.transport] # Optional. Keys only — no `type`. Whether a connection is encrypted is the client's decision (every connection starts with a v3 transport selector byte); placing the keys lets the server accept Noise connections in addition to plain ones
[server.transport.noise] # Keys. Present = the server can accept Noise (selector 0x01)
local_private_key = "key_encoded_in_base64"
remote_public_key = "key_encoded_in_base64"
psk = "key_encoded_in_base64" # Optional. Pre-shared key (32 bytes, base64-encoded). The pattern must include a PSK modifier (e.g. Noise_KKpsk0_...)
psk_location = 0 # Optional. The PSK slot index used in the pattern. Default: 0
```

## Dynamic service registration

There are no `[server.services.*]` blocks anymore. The lifecycle is:

1. The client authenticates with `default_token`.
2. For each configured service the client sends a `RegisterService` message:
   name, `protocol` (tcp/udp), `remote_bind_addr`, the data-plane `carrier`
   it will use (tcp/kcp — a `kcp` carrier triggers the server's lazy UDP
   listener), `pool_size` and the UDP buffer size.
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

The `multiplex` feature is part of the default feature set. With
`mode = "multiplex"` (the default), each service opens **N tunnel
connections** (`count`, default 4) after registering, and every subsequent
data channel becomes a yamux stream inside one of them. This removes the
per-connection handshake latency (TCP connect plus, with `noise`, the Noise
handshake) and cuts FD usage under many concurrent visitors.

- The decision belongs to the client alone (`[client.data].default_mode`); the server
  adapts per connection automatically.
- `mode = "direct"` restores the one-connection-per-channel path.
- Per-tunnel buffering is bounded by internal defaults (64 MiB yamux receive
  window, 32 streams) — bounded loss backlog without throughput loss; the
  values are fixed because yamux couples them (see internals.md).
- `count = N` opens N parallel tunnels per service and spreads data channels
  across them round-robin. Independent TCP flows isolate head-of-line
  blocking (a lost segment stalls only its own tunnel) and aggregate beyond
  a single flow's congestion window. If one tunnel dies, opens transparently
  fall through to the survivors until the usual heartbeat-driven reconnect
  re-establishes the pool. Default: 4; `1` reproduces single-tunnel behavior.
- **Experimental (transport comparison arms):** `carrier = "kcp"` runs the
  data plane as KCP-over-UDP sessions instead of TCP connections (feature
  `kcp`, in the default set). KCP is a userspace ARQ protocol: faster loss
  recovery than TCP at the cost of CPU. The crypto stack is unchanged — with
  transport `noise` the same Noise handshake wraps each KCP session — and
  yamux still carries the data channels, so `count` applies as usual. The
  server opens its UDP listener lazily — the first registration that
  declares the `kcp` carrier triggers the bind, and a bind failure is a
  precise registration rejection; servers whose clients never use KCP never
  open the UDP socket. The listener binds on the data address
  (`[server.data].bind_addr`, default = the control address) and each
  session authenticates with the control session's nonce exactly like a TCP
  tunnel. If neither side sets a data
  address, **TCP control and UDP KCP share one port number**: TCP and UDP
  are distinct protocols, so both sockets bind the same port without
  conflict (remember to open both protocols in the firewall/NAT). Fixed KCP
  parameters (recorded
  for comparability): stream mode, nodelay 10 ms interval, fast-resend 2,
  congestion control off, snd window 2048 / rcv window 4096 segments, MTU
  1400, 32 MiB socket buffers. Keepalive: a 2 s adapter-level PING/PONG
  keeps idle tunnels warm (NAT mappings) and probes the path RTT; a
  vanished peer is only confirmed on the next write (dead-link after ~20
  RTOs).
- Building without the feature removes the option entirely and always uses
  the one-connection-per-channel path.

**Per-service overrides.** `[client.data]` holds the defaults; each service
can override `mode`, `count` and `carrier` individually on its own
`[client.services.<name>]` block. The same rules as the global block apply
to the merged view: `count` and `carrier` are only valid with
`mode = "multiplex"`, and `carrier = "kcp"` additionally needs the `kcp`
feature. The tunnel pools were
already provisioned per service — this makes the switch per service too, so
one client can mix a multiplexed interactive service (few handshakes, NAT-
friendly) with a `direct` bulk service (raw throughput) without any server
configuration change: the server adapts per connection and opens its KCP
listener on the first `kcp` registration (there is no per-carrier server
configuration). The same overlay pattern covers the control defaults:
`token`, `remote_addr` and `heartbeat_timeout` override
`[client].default_token`, `[client.control].default_remote_addr` and
`[client.control].default_heartbeat_timeout` respectively, and
`retry_interval` overrides `default_retry_interval`. `default_data_addr` itself
cannot be overridden per service — the data-plane endpoint follows the
service's own server when it has one (see below).

**Multiple servers.** A service can also override the server itself:
`[client.services.<name>].remote_addr` replaces
`[client.control].default_remote_addr` for that service's control channel,
and its data plane follows by default (tunnels dial the same endpoint,
since the server's data listener defaults to its control address). One
client can therefore spread its services across several molehill servers —
a nearby replica per region, separate servers per tenant, or a migration
window while moving services one by one. Every server must authenticate
the service with a token: a service can carry its own `token` for a server
that does not share the client's `default_token`, and its own
`heartbeat_timeout` when that server's `heartbeat_interval` differs; each
server's `allow_ports` must cover the services registered on it. The
data-plane endpoint chain is: the service's own `remote_addr`, else
`[client.data].default_data_addr`, else `[client.control].default_remote_addr`
— so a global `default_data_addr` applies only to services without their own
`remote_addr`, and a server that runs its data listener on a separate port
(a distinct `[server.data].bind_addr`) needs that address set globally,
which then applies to every service that follows the client-wide endpoint.

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

The step-by-step way to pick `mode`/`count`/`carrier`/transport for your
workload is the [decision tree](#choosing-your-configuration-decision-tree)
above (with the measured costs and how to validate). This section covers
the per-connection knobs.

From v0.4.7, molehill enables TCP_NODELAY by default on every TCP connection: the control channel, the data-plane tunnels, both ends of each data channel, the visitor-facing sockets, and the client's connection towards the local service. This benefits latency and interactive applications like SSH, rdp, Minecraft servers. However, it slightly decreases the bandwidth.

TCP keepalive is also enabled by default on these sockets (20s idle time, 8s probe interval), so pooled data channels that were silently dropped by NATs or middleboxes are detected instead of being handed out to visitors.

If the bandwidth is more important, TCP_NODELAY can be opted out with `nodelay = false` per service.

## Complete examples

Ready-to-run configurations (previously shipped as separate files under
`examples/`; reproduced here as reference — every block below parses with
the current binary, enforced by the config test suite).

### Minimal

A minimal client and server pair:

```toml
# client.toml
[client]
default_token = "123"

[client.control]
default_remote_addr = "localhost:2333"

[client.services.foo1]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"
```

```toml
# server.toml
[server]
default_token = "123"
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"
```

### Every option (full reference)

```toml
# Complete client configuration example.
# Every option is documented in the specification above.

[client]
default_token = "default_token_if_not_specify" # security-scan:allow documentation placeholder # Optional. Default token for services without their own

[client.control]
default_remote_addr = "myserver.com:2333" # Necessary. The address of the server
default_heartbeat_timeout = 40 # Optional. Set to 0 to disable the application-layer heartbeat test. Must be greater than `server.control.heartbeat_interval`. Default: 40 seconds
default_retry_interval = 1 # Optional. The interval between retries to connect to the server. Default: 1 second

# Data-plane options (`[client.data]`) live here too; see the specification.
# They require the `multiplex` feature, which is part of the default build.
# Every service may also override mode/count/carrier on its own block.

[client.transport] # Optional. The whole block is optional
type = "plain" # Optional. Possible values: ["plain", "noise"]. Default: "plain"
proxy = "socks5://user:passwd@127.0.0.1:1080" # Optional. Connect to the server via a proxy. `socks5` and `http` are supported

[client.transport.noise] # Necessary only if `type` is "noise". See docs/transport.md
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s" # Optional. Default value as shown
local_private_key = "key_encoded_in_base64" # Optional
remote_public_key = "key_encoded_in_base64" # Optional
psk = "key_encoded_in_base64" # Optional. Pre-shared key (32 bytes, base64-encoded); the pattern must include a PSK modifier (e.g. Noise_KKpsk0_...)
psk_location = 0 # Optional. The PSK slot index used in the pattern. Default: 0

[client.services.ssh] # A service to forward
protocol = "tcp" # Optional. Possible values: ["tcp", "udp"]. Default: "tcp"
local_addr = "127.0.0.1:22" # Necessary. The address of the local service
nodelay = true # Optional. Per-service TCP_NODELAY override. Default: true
retry_interval = 1 # Optional. Override the global `client.control.default_retry_interval` per service
udp_forwarder_ipv6 = false # Optional. Prefer IPv6 for the UDP forwarder's connection to the local service (UDP services only)
health_check = { type = "tcp", interval = 10, timeout = 3, max_failed = 1 } # Optional. TCP services only. Remove the service from the server while the local service is down (see "Health check")
remote_bind_addr = "0.0.0.0:5202"

[client.services.dns] # A UDP service example
protocol = "udp"
local_addr = "127.0.0.1:53"
remote_bind_addr = "0.0.0.0:53"
```

```toml
# Complete server configuration example.
# Every option is documented in the specification above.

[server]
default_token = "default_token_if_not_specify" # security-scan:allow documentation placeholder # Optional. Default token for services without their own
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["53", "5202"]

[server.control]
bind_addr = "0.0.0.0:2333" # Necessary. The address that the server listens for clients
heartbeat_interval = 30 # Optional. The interval between two application-layer heartbeats; set to 0 to disable. Default: 30 seconds

# Data-plane options (`[server.data]`) live here too; see the specification.
# They require the `multiplex` feature, which is part of the default build.

[server.transport] # Optional. Keys only - no `type`: the client decides whether a connection is encrypted (v3 selector byte); placing the keys lets the server accept Noise connections too
[server.transport.noise] # Keys for accepting Noise connections. See docs/transport.md
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s" # Optional. Default value as shown
local_private_key = "key_encoded_in_base64" # Optional
remote_public_key = "key_encoded_in_base64" # Optional
psk = "key_encoded_in_base64" # Optional. Pre-shared key (32 bytes, base64-encoded); the pattern must include a PSK modifier (e.g. Noise_KKpsk0_...)
psk_location = 0 # Optional. The PSK slot index used in the pattern. Default: 0
```

### Noise (encrypted transport)

Generate a keypair with `molehill --genkey`, put the client's copy of the
server's public key on the client and the server's private key on the
server (see [Transport](./transport.md)):

```toml
# client.toml
[client]
default_token = "123"

[client.control]
default_remote_addr = "localhost:2333"

[client.transport]
type = "noise"

[client.transport.noise]
remote_public_key = "xrpknQcAagcd/b9foMwxSCD+EindWxq450NEONk8XQo="

[client.services.foo1]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"
```

```toml
# server.toml
[server]
default_token = "123"
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"

[server.transport.noise]
local_private_key = "QLYMByBnjgM254zT6YKaBVvuAA61swyZfFxoA/SKZHM="
```

### UDP service

```toml
# client.toml
[client]
default_token = "123"

[client.control]
default_remote_addr = "localhost:2333"

[client.services.foo1]
protocol = "udp"
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"
```

```toml
# server.toml
[server]
default_token = "123"
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"
```

### Server and client in one file

molehill can determine the mode from the config when only one of
`[client]` / `[server]` is present; with both, pass the mode explicitly:

```toml
# config.toml - run: molehill --server config.toml  /  molehill --client config.toml
[client]
default_token = "123"

[client.control]
default_remote_addr = "localhost:2333"

[client.services.foo1]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"

[server]
default_token = "123"
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"
```

### Connect through a proxy

```toml
# client.toml
[client]
default_token = "123"

[client.control]
default_remote_addr = "127.0.0.1:2333"

[client.services.foo1]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"

[client.transport]
type = "plain"
proxy = "socks5://myuser:mypass@127.0.0.1:1080"
```

### iperf3 test services

Forward a local iperf3 server over both TCP and UDP:

```toml
# client.toml
[client]
default_token = "123"

[client.control]
default_remote_addr = "localhost:2333"

[client.services.iperf3-udp]
protocol = "udp"
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"

[client.services.iperf3-tcp]
protocol = "tcp"
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"
```

```toml
# server.toml
[server]
default_token = "123"
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"
```

## Deployment

### systemd

Run molehill as a systemd service, with root or rootless, including
multiple instances. In the unit names, `molehills` stands for
`molehill --server`, `molehillc` for `molehill --client`, and `molehill`
for the auto-detect mode. The `@` in a unit name instantiates it per config
file. Store config files with permission `600` (they contain the shared
token).

```ini
# molehills@.service - one server instance per config: systemctl enable molehills@app1 --now
[Unit]
Description=Molehill Server Service (%i)
After=network.target

[Service]
Type=simple
Restart=on-failure
RestartSec=5s
LimitNOFILE=1048576

# with root
ExecStart=/usr/bin/molehill -s /etc/molehill/%i.toml
# without root
# ExecStart=%h/.local/bin/molehill -s %h/.local/etc/molehill/%i.toml

[Install]
WantedBy=multi-user.target
```

```ini
# molehills.service - a single server instance
[Unit]
Description=Molehill Server Service
After=network.target

[Service]
Type=simple
Restart=on-failure
RestartSec=5s
LimitNOFILE=1048576

# with root
ExecStart=/usr/bin/molehill -s /etc/molehill/molehill.toml
# without root
# ExecStart=%h/.local/bin/molehill -s %h/.local/etc/molehill/molehill.toml

[Install]
WantedBy=multi-user.target
```

```ini
# molehillc@.service - one client instance per config: systemctl enable molehillc@app1 --now
[Unit]
Description=Molehill Client Service (%i)
After=network.target

[Service]
Type=simple
Restart=on-failure
RestartSec=5s
LimitNOFILE=1048576

# with root
ExecStart=/usr/bin/molehill -c /etc/molehill/%i.toml
# without root
# ExecStart=%h/.local/bin/molehill -c %h/.local/etc/molehill/%i.toml

[Install]
WantedBy=multi-user.target
```

```ini
# molehillc.service - a single client instance
[Unit]
Description=Molehill Client Service
After=network.target

[Service]
Type=simple
Restart=on-failure
RestartSec=5s
LimitNOFILE=1048576

# with root
ExecStart=/usr/bin/molehill -c /etc/molehill/molehill.toml
# without root
# ExecStart=%h/.local/bin/molehill -c %h/.local/etc/molehill/molehill.toml

[Install]
WantedBy=multi-user.target
```

```ini
# molehill@.service - auto-detect mode, one instance per config
[Unit]
Description=Molehill Service (%i)
After=network.target

[Service]
Type=simple
Restart=on-failure
RestartSec=5s
LimitNOFILE=1048576

# with root
ExecStart=/usr/bin/molehill /etc/molehill/%i.toml
# without root
# ExecStart=%h/.local/bin/molehill %h/.local/etc/molehill/%i.toml

[Install]
WantedBy=multi-user.target
```

With root (assuming `molehill` in `/usr/bin` and configs under
`/etc/molehill/app1.toml`):

```bash
sudo cp molehills@.service /etc/systemd/system/
sudo mkdir -p /etc/molehill        # then create app1.toml inside
sudo systemctl daemon-reload
sudo systemctl enable molehills@app1 --now
```

Without root (assuming `molehill` in `~/.local/bin` and configs under
`~/.local/etc/molehill/app1.toml`): uncomment the `%h` ExecStart line in
the unit, then:

```bash
mkdir -p ~/.config/systemd/user
cp molehills@.service ~/.config/systemd/user/
mkdir -p ~/.local/etc/molehill    # then create app1.toml inside
systemctl --user daemon-reload
systemctl --user enable molehills@app1 --now
```

Multiple instances: add another config (`app2.toml`) and enable
`molehills@app2` (same for `molehillc@.service` and `molehill@.service`).

### Container

The official image `ghcr.io/niyueee/molehill:latest` is a single static
musl binary on `scratch` (~1.2 MiB), runs as non-root UID 1000 and contains
**no configuration** — mount your own `server.toml` / `client.toml`
read-only at `/app/server.toml` (or `/app/client.toml`) and pass its name
as the command-line argument.

```bash
docker run -v /etc/molehill/server.toml:/app/server.toml:ro \
  ghcr.io/niyueee/molehill:latest server.toml
```

The image carries the full default feature set (`server`, `client`, `noise`,
`hot-reload`, `multiplex`, `kcp`), so `default_carrier = "kcp"` needs no
different image. Pin a release tag (`ghcr.io/niyueee/molehill:v0.8.0`)
instead of `:latest` when you want reproducible upgrades.

Two consequences of running as UID 1000:

- The mounted config must be readable by UID 1000 — `chmod 644` it (or
  `chown 1000`), otherwise the container exits with a permission error.
- Under **host** networking the process cannot bind ports below 1024 (the
  host's `ip_unprivileged_port_start`, normally 1024, applies), so every
  `remote_bind_addr` and the control/data listeners need ports ≥ 1024. Under
  bridge networking the container's own namespace usually allows low ports,
  but the portable recipe is the same: keep the container port high and map
  the privileged host port onto it (`-p 80:8080` with
  `remote_bind_addr = "0.0.0.0:8080"`).

Docker / Podman Compose (host networking — simplest on Linux; the server
must expose arbitrary service ports):

```yaml
# compose.yaml - usage: docker compose up -d  (or: podman compose up -d)
services:
  molehill-server:
    image: ghcr.io/niyueee/molehill:latest
    container_name: molehill-server
    restart: unless-stopped
    network_mode: host
    environment:
      RUST_LOG: info
    volumes:
      - ./server.toml:/app/server.toml:ro
    command: server.toml

  molehill-client:
    image: ghcr.io/niyueee/molehill:latest
    container_name: molehill-client
    restart: unless-stopped
    network_mode: host
    environment:
      RUST_LOG: info
    volumes:
      - ./client.toml:/app/client.toml:ro
    command: client.toml
```

Bridge-network variant for Docker Desktop (macOS/Windows); the client then
reaches the server through the compose DNS name, so set
`default_remote_addr = "molehill-server:2333"` in `client.toml`:

```yaml
# compose.bridge.yaml - usage: docker compose -f compose.bridge.yaml up -d
services:
  molehill-server:
    image: ghcr.io/niyueee/molehill:latest
    container_name: molehill-server
    restart: unless-stopped
    environment:
      RUST_LOG: info
    volumes:
      - ./server.toml:/app/server.toml:ro
    command: server.toml
    ports:
      - "2333:2333"     # Control channel and TCP data plane (clients connect here)
      - "2333:2333/udp" # KCP data plane, only when a service uses carrier = "kcp"
      - "5202:5202"     # Exposed SSH service

  molehill-client:
    image: ghcr.io/niyueee/molehill:latest
    container_name: molehill-client
    restart: unless-stopped
    environment:
      RUST_LOG: info
    volumes:
      - ./client.toml:/app/client.toml:ro
    command: client.toml
```

Podman Quadlet — a `.container` file turns the image into a systemd
service (root: copy to `/etc/containers/systemd/`, `daemon-reload`,
`systemctl enable --now molehill-server`; rootless: copy to
`~/.config/containers/systemd/`, use `systemctl --user`, and change
`WantedBy=` to `default.target`):

```ini
# molehill-server.container
[Unit]
Description=Molehill server (container)
After=network-online.target
Wants=network-online.target

[Container]
Image=ghcr.io/niyueee/molehill:latest
Volume=/etc/molehill/server.toml:/app/server.toml:ro
Network=host
Environment=RUST_LOG=info
Exec=server.toml

[Service]
Restart=always

[Install]
WantedBy=multi-user.target
```

```ini
# molehill-client.container
[Unit]
Description=Molehill client (container)
After=network-online.target
Wants=network-online.target

[Container]
Image=ghcr.io/niyueee/molehill:latest
Volume=/etc/molehill/client.toml:/app/client.toml:ro
Network=host
Environment=RUST_LOG=info
Exec=client.toml

[Service]
Restart=always

[Install]
WantedBy=multi-user.target
```

## Usage notes

### Network requirements

- The **server** must be reachable from the Internet: `server.control.bind_addr`, `server.data.bind_addr` (when set) and every registered `remote_bind_addr` need inbound access (open the ports in the firewall or port-forward them on the public server). Add the matching **UDP** port whenever a service uses `carrier = "kcp"` — the KCP listener binds `server.data.bind_addr`, i.e. the control port by default, and TCP plus UDP coexist on that port number.
- The **client** only needs outbound access to `server.control.bind_addr` (and the data endpoint when it differs; TCP, plus UDP for `carrier = "kcp"`); no inbound port is required behind the NAT.
- Running in a container: the image runs as UID 1000 and cannot bind ports below 1024 — see [Container](#container) for the port and config-permission consequences.
- `client.control.default_remote_addr` must use the same port as `server.control.bind_addr` unless the server moved its control listener.

### Security

- The shared token is mandatory. Use long random values.
- `allow_ports` is your authorization boundary: only list what clients genuinely need. Without it, the server exposes nothing regardless of what clients request.
- The config file contains tokens in plain text, so restrict its permissions (e.g. `chmod 600 config.toml`). Tokens are masked (`MASKED`) in logs.
- Use the `noise` transport when traffic traverses untrusted networks; `plain` forwards unencrypted.
- Noise private keys are secrets too.

### Heartbeat

- `client.control.default_heartbeat_timeout` must be greater than `server.control.heartbeat_interval`, otherwise the client treats a healthy server as dead and reconnects in a loop.
- Set `server.control.heartbeat_interval = 0` to disable heartbeats (then set `client.control.default_heartbeat_timeout = 0` as well).

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

Transport setup — Noise keypairs, patterns and PSKs — is documented step by
step in [Transport](./transport.md): pick the `type` in the specification
above and follow that guide.

- **Proxy**: `[client.transport].proxy` only applies to the client's outbound
  connections to the server (control channel, data-plane tunnels, and direct
  data channels). Both `socks5` and `http` (CONNECT), with optional basic
  auth, are supported. It is client-only; the server rejects it.

### Hot reload

- Editing the client config while running: general changes (transport, addresses, tokens, data-plane settings) restart the instance; adding, removing, or modifying a service applies without a restart (services are unregistered/re-registered over the existing control channels).
- Keep the file valid while editing — an invalid config is rejected at startup or reload, and the previous state keeps running.

### Multiple services and instances

- One client config can forward many services (multiple `[client.services.<name>]` blocks), and several clients can connect to the same server. Each registered service name must be unique across all clients of a server.
- To run several independent molehill pairs on one host, use different ports for the control and data listeners and separate config files (the [systemd units](#systemd) show templated instances).

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `Server rejected service <name>: Port N rejected ... allow_ports` | The requested `remote_bind_addr` port is not whitelisted on the server, or the server has dynamic registration disabled. Fix `allow_ports`. |
| `Port N is already in use` | Another service (or another program) holds that port on the server. Pick a different `remote_bind_addr` port. |
| `Protocol version mismatched ... Please update` | One side runs an older molehill. Upgrade both ends together (protocol v3 since 0.8; v2 since 0.7.0). |
| `Authentication failed` on the client | `default_token` differs between client and server. |
| `Failed to connect to <addr>: Connection refused` | Server not running, wrong `client.control.default_remote_addr` port, or `server.control.bind_addr` not reachable. |
| Repeated `Heartbeat timed out` | `client.control.default_heartbeat_timeout <= server.control.heartbeat_interval`, or the network path drops the connection. |
| Noise handshake fails | Keypairs, `psk`, or pattern mismatch between the two sides. |
| `Proxy URL is missing the port` at startup | The `proxy` URL lacks a port; fix the config. |
| UDP traffic not flowing | Check `protocol = "udp"`; datagrams larger than `udp_buffer_size` are dropped; idle mappings time out after `udp_idle_timeout` seconds. |
| Stateful UDP sessions (games, QUIC, WireGuard) break mid-session | Ensure both ends run a version with UDP session affinity (≥ this fix); a peer whose traffic idles longer than `udp_idle_timeout` is re-bound to a fresh local socket (new source port) on the next datagram — raise the timeout or send periodic traffic. |
| `Failed to read cmd: early eof` warnings | The peer closed the channel (restart or shutdown); the client reconnects automatically. |
