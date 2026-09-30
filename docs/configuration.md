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
> (`default_data_addr`, `default_mode`, `default_carrier`) — so they read
> distinctly from the per-service overlay keys on
> `[client.services.<name>]` (`protocol`, `remote_addr`, `token`,
> `retry_interval`, `mode`, `carrier`, `transport`, `udp_workers`,
> `udp_forwarder_ipv6`, `udp_send_queue_size`, ...; new in 0.8).
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
>
> **Upgrading to 0.10 (protocol v4)**: the client speaks v4 — one control
> session per endpoint, carrying every service that dials it. A v0.9.0 server
> refuses it (its own version check fails and it closes the connection; the
> client says so and stops instead of retrying), so upgrade the server first,
> or both ends together. A v0.10.0 server still serves a v0.9.0 client.

### Migrating to 0.10: removed keys

The tunnel pool is one elastic, per-carrier pool per session now, and it starts
cold — so the keys that described a pool's *initial* size, a per-service pool,
or a late-0.8 health check are gone. A config that still carries one starts for
one release and logs a warning naming the replacement; from the next release it
is an error (`deny_unknown_fields`). Write this instead:

| Removed key | Write instead |
|---|---|
| `[client.data].default_count` | Nothing: the pool starts cold and grows on demand. `[client.data.tcp].max_tunnels` (or `[client.data.kcp].max_tunnels`) is the cap it grows to, default 4 |
| `[client.services.<name>].count` | Nothing: same cold start, and the pool belongs to the session and carrier rather than to one service. `[client.data.tcp\|kcp].max_tunnels` is the cap |
| `[client.services.<name>].pool_size` | `[client.services.<name>].udp_workers` for a UDP service (default 2). A TCP service opens one data channel per visitor, on demand |
| `[client.services.<name>].heartbeat_timeout` | Nothing: the server declares its cadence in the session ack and the client derives the timeout from it. `[client.control].default_heartbeat_timeout` remains as an optional floor |
| `[server].max_pool_size` | `[server.data].max_tunnels_per_client` (the tunnels one client may hold; 0 = unlimited). It also clamps a v3 client's requested channel count |
| `[client.services.<name>].health_check` | Nothing: a service stays registered for as long as its client runs; a request that cannot be forwarded fails for that visitor |

The next section states what each of the replacements does and what it costs;
[CHANGELOG.md](../CHANGELOG.md) records why the removals happened.

## Choosing your configuration (decision tree)

The defaults — `mode = "multiplex"`, `max_tunnels = 4`, `carrier = "tcp"`,
plain transport — are the right starting point for almost everyone. Deviate
only when the tree says so, change one thing at a time, and measure the result
on your own path: the published runs, their numbers and how to reproduce them are
in [Benchmarks](benchmarks.md). This page owns **what each setting does**.

```mermaid
flowchart TD
    A["Start: defaults<br/>multiplex, max_tunnels=4,<br/>carrier=tcp, plain"] --> B{"Traffic crosses an<br/>untrusted network?"}
    B -- Yes --> C["transport type = noise<br/>+ keypair (Transport doc)"]
    B -- No --> D{"One service or a few<br/>long-lived connections?"}
    C --> D
    D -- "Yes, raw throughput first" --> E["mode = direct"]
    D -- "No: many services,<br/>many users, churn" --> F{"Many concurrent<br/>connections?"}
    E --> Z["Done - tune per service<br/>via [client.services.*] overrides"]
    F -- "> ~256 concurrent" --> G["max_tunnels = 8 or higher"]
    F -- Typical --> H["keep max_tunnels = 4"]
    G --> I{"Path quality?"}
    H --> I
    I -- "High pure latency +<br/>UDP game (100ms+ RTT)" --> J["A/B test carrier = kcp"]
    I -- Otherwise --> Z
    J --> Z
```

### What each choice costs (what you trade)

| Decision | Option | What you give up / gain |
|---|---|---|
| `mode` | `"multiplex"` (default) | highest connection count per FD and per NAT mapping; one slow stream shares its tunnel with the others |
| `mode` | `"direct"` | one physical connection per stream: raw single-flow throughput, at an FD / port / NAT mapping per stream |
| `max_tunnels` | `1` | one tunnel for everything: no aggregation across flows, and one loss event stalls every stream sharing the retransmit domain |
| `max_tunnels` | `4` (default) | aggregates beyond a single flow and isolates head-of-line blocking between tunnels; `4 × 64` concurrent connections |
| `max_tunnels` | `8+` | more parallel tunnels (more NAT mappings) and a proportionally higher connection ceiling |
| `carrier` | `"tcp"` (default) | the well-behaved default on lossy and rate-limited paths; TCP tunnels must not be blocked by the network |
| `carrier` | `"kcp"` | latency-first UDP transport when TCP tunnels are blocked or throttled; it does not multiplex, so pair it with `noise` + a raised `max_tunnels` for the ceiling |
| transport | `"plain"` | no encryption; lowest per-byte cost |
| transport | `"noise"` | encrypted wire with a single pre-shared keypair; a sub-millisecond RTT cost and no CPU penalty under full load |
| cold pool | (no key) | the pool starts cold: the first visitor after an idle period pays one tunnel setup before its bytes move — 2.0-3.2 ms on loopback (M2a), then it is warm again up to `max_tunnels` |
| `udp_workers` | 2 (default) | UDP only: how many data channels the service's worker set uses. Distinct visitors shard across them; one visitor is never split across channels (session affinity) |

The measured cost of each option — including the figures these trade-offs come
from, and their provenance — is in [Benchmarks](benchmarks.md#what-each-configuration-choice-costs-per-decision-measurements).

**Validate the choice** with the exposure you care about: `ping` / in-game
feel for latency, `iperf3` on the exposed port for raw throughput, and the
real traffic of your service. To compare two configurations or two builds on
your own hardware, [Benchmarks](benchmarks.md#reproduce-it-yourself) has the
commands.

Here is the full configuration specification:

```toml
[client]
default_token = "change-me" # Necessary. Must match `[server].default_token`

[client.control] # Necessary. Control-channel defaults: authentication, registration, heartbeat
default_remote_addr = "example.com:2333" # Necessary. The address of the server
# default_heartbeat_timeout = 65 # Optional. Application-layer heartbeat timeout. Unset (the default) derives it from the cadence the server declares in the session ack: `max(10 s, 2 × server.control.heartbeat_interval + 5 s)`. A value below that floor is refused at startup (it would time out a healthy server); 0 disables the check
default_retry_interval = 1 # Optional. Cap of the reconnect backoff, not a fixed interval: the delay starts at 1 s, grows by a factor of 3 with jitter and is capped at this value (jitter can make one sleep up to twice the cap), for 3 retries; once the backoff is exhausted the client falls back to a fixed 1 s retry loop. Default: 1 second

[client.data] # Optional. Data-plane defaults for every service (feature `multiplex`, part of the default build). Each service can override default_mode/default_carrier individually — see the per-service keys in `[client.services.*]` below
# default_data_addr = "example.com:2343" # Optional. Data-plane endpoint; defaults to the service's control endpoint (`client.services.<name>.remote_addr` when set, else `client.control.default_remote_addr`). With `default_carrier = "kcp"` the KCP sessions dial the control address over UDP — TCP control and UDP KCP can share one port (distinct protocols)
default_mode = "multiplex" # Optional. Default data-plane mode: "multiplex" (default) or "direct" (one connection per data channel; `carrier` does not apply)
default_carrier = "tcp" # Optional. Default data carrier: "tcp" (default) rides the control channel's wire stack; "kcp" uses KCP-over-UDP sessions (feature `kcp`; the server opens its KCP listener lazily on the first `kcp` registration — no server-side opt-in). Both transport types compose with KCP: with `noise` the same Noise handshake wraps each KCP session, with `plain` the session stays unencrypted
# shared_pool = false # Optional. Serve every service of one control session from ONE tunnel pool per carrier (true), instead of one pool per service (false, the default). Both are one code path; they differ only in the pool's key
# idle_timeout = 60 # Optional. Seconds a tunnel pool with no streams, no pending opens and no pinned UDP peers must stay idle before it removes one tunnel. Default: 60. The pool never shrinks below one tunnel, nor below the UDP-derived floor
[client.data.tcp] # Optional. The TCP carrier's elastic-pool cap
# max_tunnels = 4 # Optional. The cap the pool may grow to for this carrier; it starts cold and grows on demand up to it. Validated `>= 1`, clamped to 1..=64. Default: 4
[client.data.kcp] # Optional. The KCP carrier's cap, the same key and rules
# max_tunnels = 4

[client.transport] # Optional. How the wire is wrapped; applies to both planes
type = "plain" # Optional. Possible values: ["plain", "noise"]. Default: "plain"
proxy = "socks5://user:passwd@127.0.0.1:1080" # Optional. Client only. Connect to the server through an `http`/`socks5` proxy

[client.transport.noise] # Noise protocol. See `docs/transport.md` for further explanation
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s" # Optional. Default value as shown
local_private_key = "key_encoded_in_base64" # Optional
remote_public_key = "key_encoded_in_base64" # Optional
psk = "key_encoded_in_base64" # Optional. Pre-shared key, base64-encoded; it must decode to exactly 32 bytes, a length checked only when a connection's Noise handshake is set up. The psk is used only when the configured `pattern` carries a PSK modifier at `psk_location` (e.g. Noise_KKpsk0_...); with a non-PSK pattern it is silently ignored, not rejected
psk_location = 0 # Optional. The PSK slot index used in the pattern. Default: 0
resume = true # Optional. Noise session resume: a reconnect proves possession of the previous session's handshake hash instead of repeating the handshake's key exchanges (selector 0x02). Default: false. See `docs/transport.md`, "Noise session resume"

[client.services.service1] # A service that needs forwarding. The name identifies the service (shown in logs)
protocol = "tcp" # Optional. The protocol that needs forwarding. Possible values: ["tcp", "udp"]. Default: "tcp"
local_addr = "127.0.0.1:1081" # Necessary. The address of the local service that needs to be forwarded
remote_bind_addr = "0.0.0.0:8081" # Necessary. The public address this service is exposed at on the server. Must be covered by the server's `allow_ports`
nodelay = true # Optional. TCP_NODELAY for this service's data channels. Default: true even when unset; set `false` to disable
retry_interval = 1 # Optional. Per-service cap of the reconnect backoff, with the same semantics as `client.control.default_retry_interval`. Default: inherits `client.control.default_retry_interval`
token = "service-specific-token" # Optional. Override `client.default_token` for this service only — e.g. to authenticate against a server that has its own token # security-scan:allow documentation placeholder
remote_addr = "server2.example.com:2333" # Optional. Override `client.control.default_remote_addr` for this service only — its control channel (and, by default, its data plane) dials this server. Lets one client spread services across several molehill servers
mode = "multiplex" # Optional. Override `client.data.default_mode` for this service only. "multiplex" (default) or "direct"
carrier = "tcp" # Optional. Override `client.data.default_carrier` for this service only; valid only with `mode = "multiplex"`. Inherits the default when unset
transport = { type = "plain" } # Optional. Per-service transport override: `type` ("noise" = encrypt, "plain" = plaintext; unset = follow `client.transport.type`) and `noise` keys (used when this service is encrypted; unset = use `client.transport.noise`). Lets one client run plain and encrypted services side by side — e.g. a service dialing a different server with its own public key

[client.services.service2] # Multiple services can be defined
protocol = "udp"
local_addr = "127.0.0.1:1082"
remote_bind_addr = "0.0.0.0:8082"
udp_workers = 2 # Optional. UDP services only: how many data channels this service's worker set uses; distinct visitors shard across them, and one visitor is never split across channels. The tunnel pool keeps at least the tunnels these channels need. Default: 2
udp_forwarder_ipv6 = false # Optional. UDP services only: prefer IPv6 for the UDP forwarder's connection to the local service. Default: false
udp_buffer_size = 2048 # Optional. UDP receive buffer in bytes. Default: 2048, maximum 65535
udp_idle_timeout = 60 # Optional. Seconds after which an idle UDP peer mapping is dropped on the client (its local socket, i.e. the source port the local service sees, is recycled with it). Default: 60
udp_send_queue_size = 1024 # Optional. Queue size for outbound datagrams per data channel. Default: 1024

[server]
default_token = "change-me" # Necessary. Must match `[client].default_token`
allow_ports = ["6000-6999", "8080"] # Necessary to enable dynamic registration. Empty or missing: ALL registrations are rejected. A requested port is admitted when one of these entries contains it — a literal port, or a range covering it, privileged ports (<1024) included

[server.control] # Necessary. Control-channel listener
bind_addr = "0.0.0.0:2333" # Necessary. The address that the server listens for clients. Generally only the port needs to be changed
heartbeat_interval = 30 # Optional. The interval between two application-layer heartbeats; the client derives its own timeout from this declared cadence. Set to 0 to disable sending heartbeats. Default: 30 seconds

[server.data] # Optional. Data-plane listener (feature `multiplex`)
# bind_addr = "0.0.0.0:2343" # Optional. Data-plane listener; defaults to `server.control.bind_addr`. The KCP UDP listener binds here too on the first `kcp` registration — with the default address, TCP control and UDP KCP coexist on one port (distinct protocols)
# stripe_count = 4 # Optional. Data channels per visitor connection, clamped to 1..=64. Default: 1 — one data channel per visitor. A higher count spreads every visitor connection over that many parallel channels (a stripe group): its throughput ceiling and in-flight window become the sum of the channels', at the cost of per-connection reorder buffering. Applies to TCP services only. Both ends need the striped data-channel framing (see docs/internals.md, "Data-channel striping"). **Not supported in 0.10**: a 0.10 client is served unstriped — a stripe group's bulk path deadlocks with the elastic tunnel pool, so the server logs one warning and uses a single channel per visitor. Experimental measurement override: the `MOLEHILL_STRIPE_COUNT` environment variable replaces this value when it is set to a valid count (1..=64); an unparsable or out-of-range value is ignored with a warning
# max_tunnels_per_client = 0 # Optional. The operator's valve on the elastic pool: how many multiplexed data tunnels ONE client may hold across every service of its session. 0 (the default) is unlimited. Over the cap a tunnel is refused with a typed answer naming the cap; the session keeps running. It also clamps the channel count a v3 client asks for

[server.transport] # Optional. Keys only — no `type`. Whether a connection is encrypted is the client's decision (every connection starts with a v3 transport selector byte); placing the keys lets the server accept Noise connections in addition to plain ones
[server.transport.noise] # Keys. Present = the server can accept Noise (selector 0x01)
local_private_key = "key_encoded_in_base64"
remote_public_key = "key_encoded_in_base64"
psk = "key_encoded_in_base64" # Optional. Pre-shared key, base64-encoded; it must decode to exactly 32 bytes, a length checked only when a connection's Noise handshake is set up. The psk is used only when the configured `pattern` carries a PSK modifier at `psk_location` (e.g. Noise_KKpsk0_...); with a non-PSK pattern it is silently ignored, not rejected
psk_location = 0 # Optional. The PSK slot index used in the pattern. Default: 0
resume = true # Optional. Noise session resume: a reconnect proves possession of the previous session's handshake hash instead of repeating the handshake's key exchanges (selector 0x02). Default: false. See `docs/transport.md`, "Noise session resume"
```

## Dynamic service registration

There are no `[server.services.*]` blocks anymore. The lifecycle is:

1. The client authenticates with `default_token`.
2. For each configured service the client sends a `RegisterService` message:
   name, `protocol` (tcp/udp), `remote_bind_addr`, the data-plane `carrier`
   it will use (tcp/kcp — a `kcp` carrier triggers the server's lazy UDP
   listener) and the UDP buffer size. The channel count is not part of the
   message: the client opens the channels it configured (one per visitor for
   TCP, `udp_workers` for UDP) and the server asks for another when a visitor
   arrives.
3. The server validates:
   - **whitelist**: the requested port must be covered by `allow_ports`;
     an empty/missing `allow_ports` rejects *every* registration (this is
     how you disable the feature entirely);
   - **privileged ports**: there is no separate rule — a whitelist entry
     (literal or range) admits the ports below 1024 inside it like any
     other port. Binding one still fails unless the server has the OS
     privilege (root, or a lowered
     `net.ipv4.ip_unprivileged_port_start`), so when the server is
     privileged, list the privileged ports it should expose literally;
   - **conflicts**: if the port is already bound, the registration fails
     with `Port already in use`.
4. On success the server binds the port immediately and starts forwarding.

Rejections are permanent for that service run: the client logs the exact
reason from the server and gives up until you fix the configuration or
restart it. Service names must be unique per server; re-registration from a
restarting client takes over cleanly.

## Multiplexing (`multiplex` feature)

The `multiplex` feature is part of the default feature set. With
`mode = "multiplex"` (the default), a registered service runs over an
**elastic pool of tunnel connections** (up to
`[client.data.tcp|kcp].max_tunnels`, default 4), and every subsequent data
channel becomes a yamux stream inside one of them. This removes the
per-connection handshake latency (TCP connect plus, with `noise`, the Noise
handshake) and cuts FD usage under many concurrent visitors.

- The decision belongs to the client alone (`[client.data].default_mode`); the server
  adapts per connection automatically.
- `mode = "direct"` restores the one-connection-per-channel path.
- Per-tunnel buffering is bounded by internal defaults (64 MiB yamux receive
  window, 64 streams) — bounded loss backlog without throughput loss; the
  values are fixed because yamux couples them (see internals.md).
- **The pool starts cold.** Nothing is dialed until something needs a tunnel:
  a service's first visitor grows the pool synchronously, so that visitor pays
  one tunnel setup before its bytes move (2.0-3.2 ms on loopback, M2a); every
  later visitor finds a warm tunnel, and the pool keeps growing on demand up to
  `max_tunnels`. An idle pool gives tunnels back after
  `[client.data].idle_timeout` (default 60 s), never below one and never below
  the floor a UDP service's workers need.
- `max_tunnels = N` is the cap the pool may grow to for that carrier.
  Independent TCP flows isolate head-of-line blocking (a lost segment stalls
  only its own tunnel) and aggregate beyond a single flow's congestion window.
  If one tunnel dies, opens transparently fall through to the survivors until
  the usual heartbeat-driven reconnect re-establishes the pool. Default: 4;
  `1` reproduces single-tunnel behavior.
- **Experimental (transport comparison arms):** `carrier = "kcp"` runs the
  data plane as KCP-over-UDP sessions instead of TCP connections (feature
  `kcp`, in the default set). KCP is a userspace ARQ protocol that trades
  throughput for UDP session quality: it loses to the TCP carriers in every
  measured cell (often by an order of magnitude) while its UDP echo is
  measurably cleaner under loss and at high RTT (0% loss and a ~20 ms max
  inter-packet gap at rtt100, where the TCP arms sit above 100 ms), at
  several times the CPU and RSS. The crypto stack is unchanged — with
  transport `noise` the same Noise handshake wraps each KCP session — and
  yamux still carries the data channels, so `max_tunnels` applies as usual. The
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
- Building without the feature removes the option entirely, and such a
  build must not see the corresponding tables at all: a config that
  contains `[client.data]` or `[server.data]` is rejected there (unknown
  keys — `deny_unknown_fields`). Delete those tables and the data plane
  always uses the one-connection-per-channel path.

**Per-service overrides.** `[client.data]` holds the defaults; each service
can override `mode` and `carrier` individually on its own
`[client.services.<name>]` block. The same rules as the global block apply
to the merged view: `carrier` is only valid with `mode = "multiplex"`, and
`carrier = "kcp"` additionally needs the `kcp` feature. A service's carrier
selects which of the two caps (`[client.data.tcp|kcp].max_tunnels`) its pool
grows to; with `[client.data].shared_pool` every service of the session shares
one pool per carrier. So one client can mix a multiplexed interactive service
(few handshakes, NAT-friendly) with a `direct` bulk service (raw throughput)
without any server configuration change: the server adapts per connection and
opens its KCP listener on the first `kcp` registration (there is no per-carrier
server configuration). The same overlay pattern covers the control defaults:
`token` and `remote_addr` override `[client].default_token` and
`[client.control].default_remote_addr`, and `retry_interval` overrides
`default_retry_interval`. A service's heartbeat is not a per-service knob: one
session carries one timer, derived from the cadence the server declares (see
`[client.control].default_heartbeat_timeout`). `default_data_addr` itself
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
that does not share the client's `default_token`; each server's `allow_ports`
must cover the services registered on it, and the client derives each
session's heartbeat timeout from the cadence that server declares, so servers
with different cadences coexist. The
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

### What each level means

The level says who has to act, not how alarming the event sounds:

| Level | Means | Examples |
|-------|-------|----------|
| `ERROR` | a human has to do something; the tool cannot fix it | a registration the server rejected, a listener that cannot accept, the backoff giving up |
| `WARN` | the software handled it, and it is worth one line | a config key that was removed and is ignored, a token the server rejected |
| `INFO` | lifecycle: something started, stopped or changed state | a service registered, a control channel established, a shutdown |
| `DEBUG` | one connection's or one session's business | a visitor whose local service refused the connection, a data channel that ended, a retry after the first |

Consequences worth stating, because they are what keeps a busy log readable:

- **A failed request is not a WARN.** A visitor whose `local_addr` refuses the connection is one closed connection; that line is `DEBUG`, and `docs/configuration.md#a-local-service-that-is-down` describes what the visitor sees.
- **A repeating condition is reported once.** A client that starts before its server, or retries with the wrong token, produces one `INFO`/`WARN` and then `DEBUG` until it recovers; a healthy run emits no `WARN` or `ERROR` at all. `tests/log_budget_test.rs` measures exactly that against the real binary, so the guarantee is enforced rather than intended.
- **`RUST_LOG=debug` is the troubleshooting level** and is expected to be voluminous: it is where per-connection detail lives.

## Tuning

The step-by-step way to pick `mode`/`max_tunnels`/`carrier`/transport for
your workload is the [decision tree](#choosing-your-configuration-decision-tree)
above (with the measured costs and how to validate). This section covers
the per-connection knobs.

From v0.4.7, molehill enables TCP_NODELAY by default on every TCP connection: the control channel, the data-plane tunnels, both ends of each data channel, the visitor-facing sockets, and the client's connection towards the local service. This benefits latency and interactive applications like SSH, rdp, Minecraft servers. However, it slightly decreases the bandwidth.

Only the client honours `nodelay`, and only on the two socket kinds the client creates for a service: its data-channel connections on the one-connection-per-channel path, and its TCP connection towards the local service. Every other socket stays nodelay regardless: the control channel is always set up with TCP_NODELAY at both ends, the client's multiplexed tunnels use those same control-channel options, and the server always applies its fixed latency-friendly defaults (nodelay + keepalive) to its end of every data channel and to the visitor-facing sockets. `nodelay = false` therefore cannot turn Nagle back on there.

TCP keepalive is also enabled by default on these sockets (20s idle time, 8s probe interval), so pooled data channels that were silently dropped by NATs or middleboxes are detected instead of being handed out to visitors.

If the bandwidth is more important, TCP_NODELAY can be opted out with `nodelay = false` per service — on the client-side sockets above.

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
# default_heartbeat_timeout = 65 # Optional. Unset derives it from the cadence the server declares: `max(10 s, 2 × server.control.heartbeat_interval + 5 s)`. Below that floor it is refused at startup; 0 disables the check
default_retry_interval = 1 # Optional. Cap of the reconnect backoff, not a fixed interval: the delay starts at 1 s, grows by a factor of 3 with jitter and is capped at this value (jitter can make one sleep up to twice the cap), for 3 retries; once the backoff is exhausted the client falls back to a fixed 1 s retry loop. Default: 1 second

# Data-plane options (`[client.data]`) live here too; see the specification.
# They require the `multiplex` feature, which is part of the default build.
# Every service may also override mode/carrier on its own block.

[client.transport] # Optional. The whole block is optional
type = "plain" # Optional. Possible values: ["plain", "noise"]. Default: "plain"
proxy = "socks5://user:passwd@127.0.0.1:1080" # Optional. Connect to the server via a proxy. `socks5` and `http` are supported

[client.transport.noise] # Necessary only if `type` is "noise". See docs/transport.md
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s" # Optional. Default value as shown
local_private_key = "key_encoded_in_base64" # Optional
remote_public_key = "key_encoded_in_base64" # Optional
psk = "key_encoded_in_base64" # Optional. Pre-shared key, base64-encoded; it must decode to exactly 32 bytes, a length checked only when a connection's Noise handshake is set up. The psk is used only when the configured `pattern` carries a PSK modifier at `psk_location` (e.g. Noise_KKpsk0_...); with a non-PSK pattern it is silently ignored, not rejected
psk_location = 0 # Optional. The PSK slot index used in the pattern. Default: 0
resume = true # Optional. Noise session resume: a reconnect proves possession of the previous session's handshake hash instead of repeating the handshake's key exchanges (selector 0x02). Default: false. See `docs/transport.md`, "Noise session resume"

[client.services.ssh] # A service to forward
protocol = "tcp" # Optional. Possible values: ["tcp", "udp"]. Default: "tcp"
local_addr = "127.0.0.1:22" # Necessary. The address of the local service
nodelay = true # Optional. Per-service TCP_NODELAY override. Default: true
retry_interval = 1 # Optional. Override the global `client.control.default_retry_interval` per service
remote_bind_addr = "0.0.0.0:5202"

[client.services.dns] # A UDP service example
protocol = "udp"
local_addr = "127.0.0.1:53"
remote_bind_addr = "0.0.0.0:53"
udp_workers = 2 # Optional. UDP services only: how many data channels the worker set uses
udp_forwarder_ipv6 = false # Optional. UDP services only: prefer IPv6 for the forwarder's connection to the local service
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
heartbeat_interval = 30 # Optional. The interval between two application-layer heartbeats; the client derives its timeout from it. Set to 0 to disable. Default: 30 seconds

# Data-plane options (`[server.data]`) live here too; see the specification.
# They require the `multiplex` feature, which is part of the default build.

[server.transport] # Optional. Keys only - no `type`: the client decides whether a connection is encrypted (v3 selector byte); placing the keys lets the server accept Noise connections too
[server.transport.noise] # Keys for accepting Noise connections. See docs/transport.md
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s" # Optional. Default value as shown
local_private_key = "key_encoded_in_base64" # Optional
remote_public_key = "key_encoded_in_base64" # Optional
psk = "key_encoded_in_base64" # Optional. Pre-shared key, base64-encoded; it must decode to exactly 32 bytes, a length checked only when a connection's Noise handshake is set up. The psk is used only when the configured `pattern` carries a PSK modifier at `psk_location` (e.g. Noise_KKpsk0_...); with a non-PSK pattern it is silently ignored, not rejected
psk_location = 0 # Optional. The PSK slot index used in the pattern. Default: 0
resume = true # Optional. Noise session resume: a reconnect proves possession of the previous session's handshake hash instead of repeating the handshake's key exchanges (selector 0x02). Default: false. See `docs/transport.md`, "Noise session resume"
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
different image. Pin a release tag (`ghcr.io/niyueee/molehill:v0.9.0`)
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

- The client derives its timeout from the cadence the server declares in the session ack: `max(10 s, 2 × server.control.heartbeat_interval + 5 s)`. Leave `client.control.default_heartbeat_timeout` unset unless you want a different one; a value below the derived floor is refused at startup, naming the server's interval and the floor it needs — and `0` disables the check.
- One session carries one timer, so the timeout is a session-level fact: a service cannot override it (that key is gone — see the migration table above), because a service that wanted faster detection would still share the timer with its siblings.
- Set `server.control.heartbeat_interval = 0` to disable heartbeats; the client then has no cadence to derive a timeout from.

### A local service that is down

- **A service is registered for as long as its client runs.** There is no health check and no health-driven deregistration: `local_addr` does not have to be up when the client starts, and nothing is withdrawn from the server when it goes down.
- A visitor whose request cannot be forwarded to `local_addr` (connection refused, timeout, ...) gets a **failed request for that connection only** — the same thing any reverse proxy in front of a dead backend does. The visitor's client sees the connection close or reset; the reason is logged on the client (`service=<name>`). Other visitors and every other service of that client are unaffected.
- The consequence for operations: recovering a backend needs no action from molehill. Start it whenever you like, and the already-registered service forwards again — and a backend that flaps does not cost the client a re-registration cycle.
- **Upgrading from 0.9.0 or earlier:** the `health_check` key was removed. Delete it from `[client.services.<name>]`. A config that still carries it starts and logs a warning in this release; from the next release the key is an error.

### UDP services

- The datagram limit follows the service's `udp_buffer_size` (default 2048 bytes, up to 65535); larger datagrams are dropped while the channel stays usable. Configure it identically on the service and remember that the server enforces its own copy received at registration time.
- **Session affinity**: all datagrams from one visitor address travel a single data channel and leave the client through one dedicated local socket for the visitor's whole session, so stateful UDP services (game servers like Minecraft Bedrock/RakNet, QUIC, WireGuard, ...) see a stable `(ip, port)` and their sessions stay intact. `udp_workers` shards *distinct visitors* across channels for parallelism; it never splits one visitor across channels, and the pool keeps at least the tunnels those channels need.
- A mapping (and its local socket) is cleaned up after `udp_idle_timeout` seconds (default 60) without traffic in either direction; the next datagram re-binds a fresh socket, which changes the source port the local service sees. Keep the default or raise it for long-lived stateful sessions.

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
| `Protocol version mismatched ... Please update` | One side runs an older molehill. Upgrade both ends together (protocol v4 since 0.10 — upgrade the server first; v3 since 0.8; v2 since 0.7.0). |
| The client stops with `protocol v4` after a server's hello never arrives | The server is older than 0.10: it reads version 4, fails its own check and closes that connection. Upgrade the server. |
| `Authentication failed` on the client | `default_token` differs between client and server. |
| `Failed to connect to <addr>: Connection refused` | Server not running, wrong `client.control.default_remote_addr` port, or `server.control.bind_addr` not reachable. |
| Config starts but the connection fails with a resolve error (`failed to lookup address information`) | These address keys are only checked for a `:` in the string, not parsed as socket addresses: `client.control.default_remote_addr`, `client.services.<name>.remote_addr`, `client.data.default_data_addr`, `server.data.bind_addr`. A bare IPv6 literal such as `"::1"` therefore passes startup and has no port, failing when the address is resolved. Always write host **and** port, bracketing IPv6 literals — `"[::1]:2333"`. (A service's `remote_bind_addr` is parsed as a `SocketAddr` and rejected at startup instead.) |
| Repeated `Heartbeat timed out` | The network path drops the connection, or the server stalls. A configured timeout *below* the derived floor does not appear here — it is refused at startup. |
| Noise handshake fails | Keypairs, `psk`, or pattern mismatch between the two sides. |
| `Proxy URL is missing the port` at startup | The `proxy` URL lacks a port; fix the config. |
| UDP traffic not flowing | Check `protocol = "udp"`; datagrams larger than `udp_buffer_size` are dropped; idle mappings time out after `udp_idle_timeout` seconds. |
| Stateful UDP sessions (games, QUIC, WireGuard) break mid-session | Ensure both ends run a version with UDP session affinity (≥ this fix); a peer whose traffic idles longer than `udp_idle_timeout` is re-bound to a fresh local socket (new source port) on the next datagram — raise the timeout or send periodic traffic. |
| `Failed to read cmd: early eof` warnings | The peer closed the channel (restart or shutdown); the client reconnects automatically. |
