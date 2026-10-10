# Configuration

`molehill` can automatically determine to run in the server mode or the client mode, according to the content of the configuration file, if only one of `[server]` and `[client]` block is present, like the example in the [Quickstart](../README.md#quickstart).

But the `[client]` and `[server]` block can also be put in one file. Then on the server side, run `molehill --server config.toml` and on the client side, run `molehill --client config.toml` to explicitly tell `molehill` the running mode.

Ready-to-run configurations, systemd units and container deployments live
in [Deployment & examples](./deployment.md).

See [Transport](./transport.md) for more details about encryption and the `transport` block.

Page index:

- [How to configure (v0.7+ model)](#how-to-configure-v07-model)
- [Choosing your configuration (decision tree)](#choosing-your-configuration-decision-tree)
- [Dynamic service registration](#dynamic-service-registration)
- [Multiplexing (`multiplex` feature)](#multiplexing-multiplex-feature)
- [Transparent (L3) services](#transparent-l3-services)
- [Logging](#logging)
- [Tuning](#tuning)
- [Examples & deployment](#examples--deployment)
- [Usage notes](#usage-notes)
- [Troubleshooting](#troubleshooting)

## How to configure (v0.7+ model)

Since v0.7 the **client owns the service definitions** and the server owns
only the policy:

- The client declares each forwarded service in its own config — including
  the public address it should be exposed at (`remote_bind_addr`).
- The server has **no** per-service configuration. When a client connects, it
  registers its services at runtime; the server validates every registration
  against its `allow_ports` whitelist before exposing anything.
- Both sides authenticate with one shared secret (`default_token`).

A process runs in exactly **one** of three modes, and the file says which:
`[server]`, `[client]` (forwarding) or `[transparent]` (L3 — see
[Transparent (L3) services](#transparent-l3-services)). A mode is a property of
the *process* — its capabilities, its sockets, its TUN device — so a file
carrying two blocks is refused, and a host that wants two roles runs two
processes. `--server` / `--client` / `--transparent` override what the file
says.

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
> `[client].mux = true` → `[client.data].default_mode = "multiplex"` — and
> `default_mode` was itself removed afterwards, because the shape is derived
> from the service type now (see "Migrating: the data plane's shape is no
> longer a key") (the
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
> (`default_data_addr`, `default_carrier`) — so they read
> distinctly from the per-service overlay keys on
> `[client.services.<name>]` (`protocol`, `remote_addr`, `token`,
> `retry_interval`, `carrier`, `transport`, `udp_workers`,
> `udp_forwarder_ipv6`, `udp_send_queue_size`, ...; new in 0.8).
> `[client.transport]` keeps `type`/`noise` unprefixed: its per-service
> overrides live in the nested `transport` table, so there is no same-name
> collision at the service level (the `default_` prefix exists to
> disambiguate exactly that).
> Old keys are rejected (`deny_unknown_fields`), never silently ignored.
>
> **0.8 protocol**: every connection starts with a one-byte transport
> selector (`0x00` plain / `0x01` noise) and the registration carries the
> data-plane carrier — both ends must upgrade together; a version mismatch
> is a hard error.
>
> **Upgrading to 0.10 (protocol v4)**: the client speaks v4 — one control
> session per endpoint, carrying every service that dials it, with the server
> naming a stripe group before its channels are opened — and 0.10.0 is the
> first release that serves **v4 only**: a v3 client's connection is
> refused on the connection it happens on, with nothing sent back. Upgrade
> both ends together, then; either order works, because each side refuses the
> other's dialect instead of continuing, and the refused connection names the
> version that was expected.

### Migrating to 0.10: removed keys

The tunnel pool is one per-carrier pool per session, pinned at the count its
configuration names — so the keys that described a pool's *initial* size, a
per-service pool, or a late-0.8 health check are gone. A config that still carries one does not
start: the refusal names every key it found and what to write instead (a bare
"unknown field" tells you *that* something is wrong without telling you what to
write). Write this instead:

| Removed key | Write instead |
|---|---|
| `[client.data].default_count` | Nothing: the pool's width is `[client.data.tcp].tunnels` (or `[client.data.kcp].tunnels`), default 4 |
| `[client.services.<name>].count` | Nothing: the pool belongs to the session and carrier rather than to one service. `[client.data.tcp\|kcp].tunnels` is its width |
| `[client.services.<name>].pool_size` | `[client.services.<name>].udp_workers` for a UDP service (default 2). A TCP service opens one data channel per visitor, on demand |
| `[client.services.<name>].heartbeat_timeout` | Nothing: the server declares its cadence in the session ack and the client derives the timeout from it. `[client.control].default_heartbeat_timeout` remains as an optional floor |
| `[server].max_pool_size` | `[server.data].max_tunnels_per_client` (the tunnels one client may hold; 0 = unlimited) |
| `[client.services.<name>].health_check` | Nothing: a service stays registered for as long as its client runs; a request that cannot be forwarded fails for that visitor |

The next section states what each of the replacements does and what it costs;
[CHANGELOG.md](../CHANGELOG.md) records why the removals happened.

### Migrating: the data plane's shape is no longer a key

`mode` is gone. A config that still carries `[client.data].default_mode` or a
per-service `[client.services.<name>].mode` does not start: the refusal names
the key and what to do. What the key used to express is a property of the
service type now:

| Removed key | Write instead |
|---|---|
| `[client.data].default_mode` | Nothing: a forwarding service always multiplexes. `[client.data.tcp\|kcp].tunnels` is how wide its pool is |
| `[client.services.<name>].mode` | Nothing: the shape follows the protocol — a forwarding service multiplexes, a transparent claim never does |

A claim's channels *are* its carrier connections — a claim's throughput is the
sum of connections rather than of streams — so multiplexing one is framing for
nothing: on the same host and workload the same claim on its own connection
moved 6 % fewer wire bytes, took 33 % less CPU per packet and carried 65 % more
round trips per second
([Benchmarks](./benchmarks.md#the-transparent-l3-wire-question-the-acceptance-harness)).

## Choosing your configuration (decision tree)

The first choice is the mode: `[client]` forwards to a local application,
`[transparent]` owns public addresses instead (see
[Transparent (L3) services](#transparent-l3-services)) — and it is a choice
about the process, not about a service, so it comes before anything below.

The defaults — a pinned pool of `tunnels = 4` multiplexed connections,
`carrier = "tcp"`, plain transport — are the right starting point for almost
everyone. Deviate only when the tree says so, change one thing at a time, and
measure the result on your own path: the published runs, their numbers and how
to reproduce them are
in [Benchmarks](benchmarks.md). This page owns **what each setting does**.

```mermaid
flowchart TD
    A["Start: defaults<br/>tunnels=4,<br/>carrier=tcp, plain"] --> B{"Traffic crosses an<br/>untrusted network?"}
    B -- Yes --> C["transport type = noise<br/>+ keypair (Transport doc)"]
    B -- No --> D{"Many concurrent<br/>visitor connections?"}
    C --> D
    D -- "> ~256 concurrent" --> G["tunnels = 8 or higher"]
    D -- Typical --> H["keep tunnels = 4"]
    G --> I{"Path quality?"}
    H --> I
    I -- "High pure latency +<br/>UDP game (100ms+ RTT)" --> J["A/B test carrier = kcp"]
    I -- Otherwise --> Z["Done - tune per service<br/>via [client.services.*] overrides"]
    J --> Z
```

### What each choice costs (what you trade)

| Decision | Option | What you give up / gain |
|---|---|---|
| `tunnels` | `1` | one tunnel for everything: no aggregation across flows, and one loss event stalls every stream sharing the retransmit domain. Measured as the worst configuration on every path |
| `tunnels` | `4` (default) | aggregates beyond a single flow and isolates head-of-line blocking between tunnels; `4 × 64` concurrent connections |
| `tunnels` | `8+` | more parallel tunnels (more NAT mappings) and a proportionally higher connection ceiling; measured worth +77 % throughput from one tunnel to eight on a clean fast path |
| `carrier` | `"tcp"` (default) | the well-behaved default on lossy and rate-limited paths; TCP tunnels must not be blocked by the network |
| `carrier` | `"kcp"` | UDP transport for paths where TCP is blocked, throttled or lossy. It costs throughput where the path is clean, so choose it for the path, not by default |
| transport | `"plain"` | no encryption; lowest per-byte cost |
| transport | `"noise"` | encrypted wire with a single pre-shared keypair, at a negligible RTT cost and no CPU penalty under full load |
| pool establishment | (no key) | the pool's `tunnels` connections are dialed at service start, not on the first visitor: nothing is paid at request time, and the cost is `tunnels` idle connections (measured at 0.5–0.8 MiB RSS and 2.6 FDs each, no threads) |
| `udp_workers` | 2 (default) | UDP only: how many data channels the service's worker set uses. Distinct visitors shard across them; one visitor is never split across channels (session affinity). It is a fan-out, not a capacity knob: it does not raise the service's datagram ceiling, whose measurement is in [Benchmarks](benchmarks.md#the-udp-queue-question-a-molehill-only-diagnostic) |

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

[client.data] # Optional. Data-plane defaults for every service (feature `multiplex`, part of the default build). Each service can override default_carrier individually — see the per-service keys in `[client.services.*]` below
# default_data_addr = "example.com:2343" # Optional. Data-plane endpoint; defaults to the service's control endpoint (`client.services.<name>.remote_addr` when set, else `client.control.default_remote_addr`). With `default_carrier = "kcp"` the KCP sessions dial the control address over UDP — TCP control and UDP KCP can share one port (distinct protocols)
default_carrier = "tcp" # Optional. Default data carrier: "tcp" (default) rides the control channel's wire stack; "kcp" uses KCP-over-UDP sessions (feature `kcp`; the server opens its KCP listener lazily on the first `kcp` registration — no server-side opt-in). Both transport types compose with KCP: with `noise` the same Noise handshake wraps each KCP session, with `plain` the session stays unencrypted. The sessions are this carrier's pool's tunnels
# shared_pool = false # Optional. Serve every service of one control session from ONE tunnel pool per carrier (true), instead of one pool per service (false, the default). Both are one code path; they differ only in the pool's key
[client.data.tcp] # Optional. The TCP carrier's tunnel count
# tunnels = 4 # Optional. How many tunnels this carrier's pool establishes at service start and keeps. Validated `>= 1` and `>= ` the UDP-derived floor of the services that share it; clamped to 1..=64. Default: 4
[client.data.kcp] # Optional. The KCP carrier's count, the same key and rules
# tunnels = 4

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
protocol = "tcp" # Optional. The protocol that needs forwarding. Possible values: ["tcp", "udp"]. Default: "tcp". A service that must own its public ip:port instead is not a forwarding service at all: it belongs to a `[transparent]` block, which is its own run mode — see "Transparent (L3) services" below
local_addr = "127.0.0.1:1081" # Necessary. The address of the local service that needs to be forwarded
remote_bind_addr = "0.0.0.0:8081" # Necessary. The public address this service is exposed at on the server. Must be covered by the server's `allow_ports`
nodelay = true # Optional. TCP_NODELAY for this service's data channels. Default: true even when unset; set `false` to disable
retry_interval = 1 # Optional. Per-service cap of the reconnect backoff, with the same semantics as `client.control.default_retry_interval`. Default: inherits `client.control.default_retry_interval`
token = "service-specific-token" # Optional. Override `client.default_token` for this service only — e.g. to authenticate against a server that has its own token # security-scan:allow documentation placeholder
remote_addr = "server2.example.com:2333" # Optional. Override `client.control.default_remote_addr` for this service only — its control channel (and, by default, its data plane) dials this server. Lets one client spread services across several molehill servers
carrier = "tcp" # Optional. Override `client.data.default_carrier` for this service only. Inherits the default when unset
transport = { type = "plain" } # Optional. Per-service transport override: `type` ("noise" = encrypt, "plain" = plaintext; unset = follow `client.transport.type`) and `noise` keys (used when this service is encrypted; unset = use `client.transport.noise`). Lets one client run plain and encrypted services side by side — e.g. a service dialing a different server with its own public key

[client.services.service2] # Multiple services can be defined
protocol = "udp"
local_addr = "127.0.0.1:1082"
remote_bind_addr = "0.0.0.0:8082"
udp_workers = 2 # Optional. UDP services only: how many data channels this service's worker set uses; distinct visitors shard across them, and one visitor is never split across channels. The tunnel pool keeps at least the tunnels these channels need. Default: 2. It is a fan-out, not a capacity knob: the datagram ceiling is a property of the service and does not move with this value (many visitors saturate it at roughly 1 Gbit/s of 1400-byte datagrams), and datagrams beyond the ceiling are dropped — the design accepts that instead of head-of-line blocking other visitors, and `MOLEHILL_UDP_STATS` counts it (`queue_full`). Measurement: [Benchmarks](benchmarks.md#the-udp-queue-question-a-molehill-only-diagnostic)
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
# stripe_count = 4 # Optional. Data channels per visitor connection, clamped to 1..=64. Default: 1 — one data channel per visitor. A higher count spreads every visitor connection over that many parallel channels (a stripe group): its throughput ceiling and in-flight window become the sum of the channels', at the cost of per-connection reorder buffering. Applies to TCP services only, and never to a `[transparent]` client's claims: nothing stripes the packets of a claimed address. Both ends need the striped data-channel framing (see docs/internals.md, "Data-channel striping"): the group's channels land on distinct tunnels whenever the pool has that many, and share them when it does not. Experimental measurement override: the `MOLEHILL_STRIPE_COUNT` environment variable replaces this value when it is set to a valid count (1..=64); an unparsable or out-of-range value is ignored with a warning
# max_tunnels_per_client = 0 # Optional. The operator's valve on multiplexed tunnels: how many data tunnels ONE client may hold across every service of its session. A client whose configured `tunnels` exceeds it has the extra establishments refused at startup (and retried by the repair tick), so it serves on what it got. 0 (the default) is unlimited. Over the cap a tunnel is refused with a typed answer naming the cap; the session keeps running

[server.transport] # Optional. Keys only — no `type`. Whether a connection is encrypted is the client's decision (every connection starts with a one-byte transport selector); placing the keys lets the server accept Noise connections in addition to plain ones
[server.transport.noise] # Keys. Present = the server can accept Noise (selector 0x01)
local_private_key = "key_encoded_in_base64"
remote_public_key = "key_encoded_in_base64"
psk = "key_encoded_in_base64" # Optional. Pre-shared key, base64-encoded; it must decode to exactly 32 bytes, a length checked only when a connection's Noise handshake is set up. The psk is used only when the configured `pattern` carries a PSK modifier at `psk_location` (e.g. Noise_KKpsk0_...); with a non-PSK pattern it is silently ignored, not rejected
psk_location = 0 # Optional. The PSK slot index used in the pattern. Default: 0
resume = true # Optional. Noise session resume: a reconnect proves possession of the previous session's handshake hash instead of repeating the handshake's key exchanges (selector 0x02). Default: false. See `docs/transport.md`, "Noise session resume"

[server.transparent] # Optional, and this table IS the switch: its presence is what lets the server serve transparent (L3) clients at all. Without it a claim is refused by policy, before any device is looked at — serving L3 is what asks this process for CAP_NET_ADMIN and a TUN device, so the decision belongs to the operator, never to a remote client
tun = "molehill0" # Optional. The TUN device the server attaches to. It must already exist and have a route for every claimed address — that is the operator's job, not the daemon's. Default: molehill0
```

## Dynamic service registration

There are no `[server.services.*]` blocks anymore. The lifecycle is:

1. The client authenticates with `default_token`.
2. For each configured service the client sends a `RegisterService` message:
   name, `protocol` (tcp/udp/transparent), `remote_bind_addr`, the data-plane
   `carrier` it will use (tcp/kcp — a `kcp` carrier triggers the server's lazy
   UDP listener) and the UDP buffer size. The channel count is not part of the
   message: the client opens the channels it configured (one per visitor for
   TCP, `udp_workers` for UDP, the carrier's lane share — long-lived connections
   — for a transparent claim) and the server asks for another when a visitor
   arrives or a channel ends.
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

The `multiplex` feature is part of the default feature set. A registered
forwarding service runs over a **fixed pool of tunnel connections**
(`[client.data.tcp|kcp].tunnels`, default 4), and every data channel it carries
becomes a yamux stream inside one of them. This removes the per-connection
handshake latency (TCP connect plus, with `noise`, the Noise handshake) and cuts
FD usage under many concurrent visitors. The shape is not a key: a forwarding
service always multiplexes (there is no `mode` to write), and a
[transparent claim](#transparent-l3-services) never does.

- The decision belongs to the client alone; the server adapts per connection
  automatically.
- Per-tunnel buffering is bounded by internal defaults (32 MiB yamux receive
  window, 64 streams) — bounded loss backlog without throughput loss; the
  values are fixed because yamux couples them (see internals.md).
- **The pool is established at service start and pinned.** All `tunnels`
  connections are dialed when the service activates, so the first visitor pays
  nothing (the setup cost is paid before it arrives) and the capacity a
  deployment offers does not depend on what it happened to be doing a minute
  ago. The pool is never resized for load, and idle tunnels are never reaped.
- **Repair is the one exception**, and it is not growth: a tunnel that dies is
  replaced until the count is met again, so a single failure cannot shrink a
  deployment permanently. What is *not* repaired is the count itself — a pool
  that needs to be wider needs `tunnels` raised and the client restarted, which
  is the point: capacity is a configuration decision, not a runtime one.
- `tunnels = N` is how many carrier connections that carrier's pool holds.
  Independent TCP flows isolate head-of-line blocking (a lost segment stalls
  only its own tunnel) and aggregate beyond a single flow's congestion window.
  If a tunnel dies, opens transparently fall through to the survivors and the
  repair tick dials a replacement. Default: 4 (raised to a UDP service's worker
  count when that is larger); `1` reproduces single-tunnel behavior at a
  measured cost — one tunnel was the worst configuration on every path in the
  model's cells, because every stream then shares one congestion window.
- **What happens when the load exceeds the pool** is sharing, not growth: the
  streams spread over the tunnels that exist, up to 56 per tunnel, and a
  visitor that arrives when every tunnel is at that ceiling waits briefly for a
  stream to retire and is refused if none does. Size `tunnels` for the
  concurrency you expect; the measurements behind the sizing are in
  [Benchmarks](benchmarks.md#what-each-configuration-choice-costs-per-decision-measurements).
- **Experimental (transport comparison arms):** `carrier = "kcp"` runs the
  data plane as KCP-over-UDP sessions instead of TCP connections (feature
  `kcp`, in the default set). KCP is a userspace ARQ protocol that trades
  throughput for UDP session quality, so treat it as an A/B arm rather than
  a default: the measured comparison against the TCP carriers is in
  [Benchmarks](benchmarks.md#what-each-configuration-choice-costs-per-decision-measurements).
  The crypto stack is unchanged — with
  transport `noise` the same Noise handshake wraps each KCP session — and
  yamux still carries the data channels, so `tunnels` applies as usual. The
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
  congestion control off, snd window 2048 / rcv window 4096 segments,
  **datagram size follows the path** (1400 bytes — what any path carries —
  up to 8 KiB where the path's own MTU allows it; the kernel's path-MTU
  answer is re-read once a second, so a session on a 1500-byte path never
  sends a datagram that would have to be fragmented), 32 MiB socket
  buffers. Keepalive: a 2 s adapter-level PING/PONG
  keeps idle tunnels warm (NAT mappings) and probes the path RTT; a
  vanished peer is only confirmed on the next write: a session ends when a
  segment has been retransmitted ~20 times **and** the send window has stood
  still for five seconds, so a peer that is merely slow — acknowledgements
  queued behind a shaper, say — is not mistaken for one that is gone.
- Building without the feature removes the option entirely, and such a
  build must not see the corresponding tables at all: a config that
  contains `[client.data]` or `[server.data]` is rejected there (unknown
  keys — `deny_unknown_fields`). Delete those tables and every data channel is
  a connection of its own.

**Per-service overrides.** `[client.data]` holds the defaults; each service
can override `carrier` individually on its own
`[client.services.<name>]` block. The same rules as the global block apply
to the merged view: `carrier = "kcp"` needs the `kcp` feature. A service's
carrier selects which of the two counts (`[client.data.tcp|kcp].tunnels`) its
pool is established with; with `[client.data].shared_pool` every service of the
session shares one pool per carrier. So one client can put a latency-sensitive
service on TCP and a service whose path blocks TCP on KCP, or share one pool
between two services, without any server configuration change: the server adapts
per connection and opens its KCP listener on the first `kcp` registration (there
is no per-carrier server configuration). The same overlay pattern covers the
control defaults: `token` and `remote_addr` override `[client].default_token` and
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

## Transparent (L3) services

A transparent (L3) client gives the **client** the public `ip:port` instead of
having the server bind it. The client's host carries the claimed address on a
TUN device, the server routes whole IP packets into the tunnel, and the client's
own kernel answers the visitor — so the backend sees the visitor's real source
address, TCP keeps its end-to-end semantics, and the server holds no socket and
no per-flow state for the connection.

It is **its own run mode, with its own model**: a `[transparent]` block, started
with `molehill <config> --transparent` (or on its own, since the block says what
the process is). Every service it has is a **claim** on a public address, so
there is no `protocol` key to switch an entry's meaning and the keys a
forwarding service would use have no home in the schema at all: `local_addr`,
`nodelay` and the UDP-only keys cannot be written, rather than being written and
refused. A `[client.services.<name>]` entry with `protocol = "transparent"` is
refused with a message pointing here, and a file carrying both a `[client]` and
a `[transparent]` block is refused too — a host that both forwards and claims
runs **two processes**, which is also what keeps `CAP_NET_ADMIN` off the one
that does not need it.

It is **Linux only** and needs `CAP_NET_ADMIN` on both ends (each side attaches
to a TUN device); the `transparent` feature is part of the default set. A config
that asks for it on another platform, or in a build without the feature, is
refused at parse time with `... carries whole IP packets through a TUN device,
and this platform is not Linux`, or with a message naming the missing
`transparent` feature. Only IPv4 is carried today — a packet that is not IPv4 is
dropped and counted.

**Serving L3 is the server operator's decision.** On the server the
`[server.transparent]` table is the switch: without it a transparent
registration is refused by policy, before any device is looked at, so a client
can never be what makes the server reach for `/dev/net/tun` or ask the kernel
for `CAP_NET_ADMIN`. A server that only forwards `tcp`/`udp` services therefore
needs no capability for this feature at all. Enabling L3 does have a
consequence worth stating plainly: such a client is never encrypted, so a
server that serves one accepts plaintext connections from it, whatever
`[server.transport.noise]` says — the Noise keys keep applying to the clients
that do negotiate them.

**The daemon never configures the network.** It has no netlink code and never
shells out to `ip`: the operator creates the TUN device and installs the
addresses and routes, and the daemon verifies what it depends on and refuses
with the exact command to run when something is missing. The two recipes — an
address routed to the server, and a single-IP server — are in
[Deployment](./deployment.md#transparent-services).

| Key | Meaning |
|---|---|
| `[transparent]` | **the mode**: this block is what makes the process an L3 client, and it is where the client-wide half lives (`default_token`, `tun`, `control`, `data`, `transport`) |
| `[transparent.claims.<name>]` | one claimed public address. The name identifies the claim (shown in logs) |
| `[transparent.claims.<name>].remote_bind_addr` | the public `ip:port` the client **claims**. Its port must be covered by the server's `allow_ports` — a claim is a registration like any other; the address has to be local on the client (the recipes assign it to the TUN device) |
| `[transparent].tun` | the TUN device the client attaches to. Default: `molehill0` |
| `[transparent.data.tcp\|kcp].tunnels` | the carrier's **lane budget**: how many carrier connections this client holds for all of its claims together on that carrier, divided equally among the claims that draw on it. One lane **is** one connection (a claim never multiplexes), and every claim keeps at least one. Default: one lane per claim — the shape a claim has always had — and a budget below the claim count is refused with the number to write. At most 1024 |
| `[server.transparent]` | **the switch**: the presence of this table is what lets the server serve L3 at all. Absent, every transparent registration is refused by policy before any device is looked at |
| `[server.transparent].tun` | the TUN device the server attaches to. Default: `molehill0` |

Per-claim keys mirror a forwarding service's: `token`, `remote_addr`,
`retry_interval`, `carrier`. `[transparent.transport]` holds
one key, `proxy`, because what an L3 client sends is the visitor's own traffic:
this hop is a plain link by design, so the model has no encryption keys to offer,
and `[transparent].tun` is where a device is named.

`carrier` is the only data-plane choice a claim has: `tcp` (the default) is one
TCP connection per lane, `kcp` is one KCP session per lane — the carrier's
session *is* the data channel, with no multiplexer above it. A claim never
multiplexes: its lanes are its carrier connections, and its throughput is the
sum of them. On one host and workload the same claim on its own connection
moved 6 % fewer wire bytes, took 33 % less CPU per packet and carried 65 % more
round trips per second than the same claim as one stream of a pool
([Benchmarks](./benchmarks.md#the-transparent-l3-wire-question-the-acceptance-harness)).

`[transparent.data]` takes the same keys as `[client.data]` — `default_data_addr`,
`default_carrier` and the two per-carrier `tunnels` counts — with a different
meaning for the count. A forwarding `tunnels` is a pool's width; a claim's is a
budget of connections, because there is no pool: each claim's share is
`tunnels / claims` on its carrier, never below one, so
`[transparent.data.tcp].tunnels = 4` with two claims gives each of them two
connections, and `tunnels = 1` with two claims is refused with `2` to write. A
claim's inner flows are spread across its lanes (one flow per lane, by a hash of
its five-tuple), so more lanes are how a claim that carries many concurrent
connections gets more than one connection's worth of throughput.

### What the operator must prepare

Both ends attach to an **existing** device named by their `tun` key; the daemon
deliberately does not create one, because the operator's addresses and routes
live on it.

- **The device exists** (both ends). A missing device is refused with the two
  commands that create it — `ip tuntap add dev <tun> mode tun` and
  `ip link set <tun> up mtu 1400`.
- **The client carries every address it claims.** The claimed IP is the one in
  `remote_bind_addr`, and the client must own it: the refusal prints
  `ip addr add <ip>/32 dev <tun>`, `ip rule add from <ip> lookup 100` and
  `ip route add default dev <tun> table 100`. The address must be local because
  the application binds it; the source rule is what sends the replies that
  application emits back into the tunnel.
- **Reverse-path filtering is off.** `net.ipv4.conf.<tun>.rp_filter` **and**
  `net.ipv4.conf.all.rp_filter` must both read `0` — injected packets carry the
  visitor's source address, which a strict check drops — and the refusal prints
  the exact `sysctl -w` line. This check runs on the client; on the server the
  daemon only verifies that its device exists.
- **The routing brings the claimed address to the server** and lets the
  client's replies out. Both recipes are in
  [Deployment](./deployment.md#transparent-services).
- `CAP_NET_ADMIN` for both processes; the systemd units in
  [Deployment](./deployment.md#systemd) show the `AmbientCapabilities=` line.

### A worked example

```toml
# server.toml - the server binds nothing: it routes 10.99.0.1/32 into its TUN
# device. The last table is the switch; without it the registration is refused.
[server]
default_token = "change-me"
allow_ports = ["8443"]

[server.control]
bind_addr = "0.0.0.0:2333"

[server.transparent]
tun = "molehill0"
```

```toml
# client.toml - the client owns 10.99.0.1:8443 and carries the address on its own
# TUN device, where the application binds it. One block, one mode: claims only.
[transparent]
default_token = "change-me"
tun = "molehill0"

[transparent.control]
default_remote_addr = "203.0.113.5:2333"

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
```

The service on the far side of that claim is the visitor's own traffic: a
transparent client forwards nothing, so it has no `[transparent.services]` — a
host that *also* forwards runs a second process with a `[client]` block.

Two services may claim the same address when their ports differ, and a packet
with no port to route by — ICMP, and the fragments after the first — is
delivered only when exactly one service claims that address; otherwise it is
dropped rather than guessed. `MOLEHILL_L3_STATS=1` prints the data path's
cumulative counters once a second (see
[Diagnostics switches](#diagnostics-switches-opt-in)); how each side decides
which packet belongs to which service is in
[Internals](./internals.md#transparent-l3-services).

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

### Diagnostics switches (opt-in)

Six environment variables turn on aggregated diagnostics, one `INFO` line per
second per subject. They are off by default, they never change the forwarding
path, and turning one on is the consent — a line an operator would have to
raise `RUST_LOG` to see never reaches anything:

| Switch | Emitted | Carries |
|---|---|---|
| `MOLEHILL_MUX_STATS=1` | one line per second per tunnel | cumulative yamux framing counters (`written`, `read`, `bytes`) — frames per second, and with a CPU sample, CPU per frame |
| `MOLEHILL_KCP_STATS=1` | one line per second per process | the KCP adapter's cumulative counters (`datagrams_in`/`out`, `retransmits`, `acks_out`, `sacks_sent`, `blobs_out`, pump rounds) and the coarse per-phase timings that split a segment's userspace cost into intake, delivery, writer drain, wire drain and ARQ update |
| `MOLEHILL_POOL_STATS=1` | one line per second per live pool | the pool's key, carrier, size, configured count, UDP floor, live streams, pinned peers, the per-tunnel `streams/pending/pinned` triple, and the timeline of size changes with the reason for each (`+repair:1->2`, `-dead:2->1`) — in a healthy run there are none, and a pool whose `size` sits below its `count` is a pool whose repair is being refused |
| `MOLEHILL_PLACEMENT_STATS=1` | one line per second per process | that interval's placements: how many, how many fell back to another tunnel, the candidate and chosen load sums, `mean_spread` — the average gap in stream slots between the best and the worst candidate at the instant of a placement, i.e. what a smarter rule could have won — and the open latency's mean and maximum |
| `MOLEHILL_UDP_STATS=1` | one line per second per process | the UDP affinity table's size, its evictions, and each worker's pinned peers |
| `MOLEHILL_L3_STATS=1` | one line per second per transparent data path, plus one line per lane slot of every claim on its device | the transparent data path's cumulative counters: `forwarded`, `dropped(not_ipv4, malformed, unclaimed, no_channel)` and `channel_errors`; then, per claim, each slot's `live`, `flows`, `forwarded` and `no_channel` — which lane carried how much, how many flows are placed in it, and which one a replacement window cost. This is how a lane set is told from a stack (all the traffic on `member=0` is a claim that is not spreading), and `no_channel` on a slot is the traffic a dead lane dropped |

The counters are cumulative, so a reader that knows the window — or takes the
first and the last line of a run — gets per-second rates and cost per unit. The
pool and placement lines are the S1 observation of the shared pool (what it
does, and why the numbers are aggregated rather than per event:
[internals.md](internals.md#the-tunnel-pool)). `MOLEHILL_STRIPE_COUNT` is the
one switch that changes behaviour rather than observing it; it is documented
beside `stripe_count`, the value it replaces.

## Tuning

The step-by-step way to pick `tunnels`/`carrier`/transport for
your workload is the [decision tree](#choosing-your-configuration-decision-tree)
above (with the trade-offs and how to validate them). This section covers
the per-connection knobs.

From v0.4.7, molehill enables TCP_NODELAY by default on every TCP connection: the control channel, the data-plane tunnels, both ends of each data channel, the visitor-facing sockets, and the client's connection towards the local service. This benefits latency and interactive applications like SSH, rdp, Minecraft servers. However, it slightly decreases the bandwidth.

Only the client honours `nodelay`, and only on the two socket kinds the client creates for a service: its data-channel connections (a claim's lanes, and every channel in a build without the `multiplex` feature), and its TCP connection towards the local service. Every other socket stays nodelay regardless: the control channel is always set up with TCP_NODELAY at both ends, the client's multiplexed tunnels use those same control-channel options, and the server always applies its fixed latency-friendly defaults (nodelay + keepalive) to its end of every data channel and to the visitor-facing sockets. `nodelay = false` therefore cannot turn Nagle back on there.

TCP keepalive is also enabled by default on these sockets (20s idle time, 8s probe interval), so pooled data channels that were silently dropped by NATs or middleboxes are detected instead of being handed out to visitors.

If the bandwidth is more important, TCP_NODELAY can be opted out with `nodelay = false` per service — on the client-side sockets above.

## Examples & deployment

Worked examples for the common scenarios — minimal, Noise, UDP, one file,
proxy, iperf3 — plus the systemd units and the container / compose / Quadlet
deployments are in [Deployment & examples](./deployment.md).

## Usage notes

### Heartbeat

- The client derives its timeout from the cadence the server declares in the session ack: `max(10 s, 2 × server.control.heartbeat_interval + 5 s)`. Leave `client.control.default_heartbeat_timeout` unset unless you want a different one; a value below the derived floor is refused at startup, naming the server's interval and the floor it needs — and `0` disables the check.
- One session carries one timer, so the timeout is a session-level fact: a service cannot override it (that key is gone — see the migration table above), because a service that wanted faster detection would still share the timer with its siblings.
- Set `server.control.heartbeat_interval = 0` to disable heartbeats; the client then has no cadence to derive a timeout from.

### A local service that is down

- **A service is registered for as long as its client runs.** There is no health check and no health-driven deregistration: `local_addr` does not have to be up when the client starts, and nothing is withdrawn from the server when it goes down.
- A visitor whose request cannot be forwarded to `local_addr` (connection refused, timeout, ...) gets a **failed request for that connection only** — the same thing any reverse proxy in front of a dead backend does. The visitor's client sees the connection close or reset; the reason is logged on the client (`service=<name>`). Other visitors and every other service of that client are unaffected.
- The consequence for operations: recovering a backend needs no action from molehill. Start it whenever you like, and the already-registered service forwards again — and a backend that flaps does not cost the client a re-registration cycle.
- **Upgrading from 0.9.0 or earlier:** the `health_check` key was removed. Delete it from `[client.services.<name>]` — a config that still carries it does not start, and the refusal names the key and what to write instead (see the migration table above).

### UDP services

- The datagram limit follows the service's `udp_buffer_size` (default 2048 bytes, up to 65535): a datagram larger than it is **truncated to that size** on the way in — the first `udp_buffer_size` bytes are delivered and the rest is discarded — so the channel stays usable but the payload is short. Measured on a `udp_buffer_size = 1024` service: a 2000-byte datagram arrives at the backend as 1024 bytes and its reply reaches the visitor as 1024 bytes. Size it for the largest datagram the service sends, configure it identically on both ends, and remember that the server enforces its own copy received at registration time.
- **Session affinity**: all datagrams from one visitor address travel a single data channel and leave the client through one dedicated local socket for the visitor's whole session, so stateful UDP services (game servers like Minecraft Bedrock/RakNet, QUIC, WireGuard, ...) see a stable `(ip, port)` and their sessions stay intact.
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
- To run several independent molehill pairs on one host, use different ports for the control and data listeners and separate config files (the [systemd units](./deployment.md#systemd) show templated instances).

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `Server rejected service <name>: Port N rejected ... allow_ports` | The requested `remote_bind_addr` port is not whitelisted on the server, or the server has dynamic registration disabled. Fix `allow_ports`. |
| `Port N is already in use` | Another service (or another program) holds that port on the server. Pick a different `remote_bind_addr` port. |
| `Protocol version mismatched ... Please update` | One side runs a molehill that does not speak protocol v5 (the 0.10 line serves v4 only), so an older client *or* an older server gets this; upgrade both ends together. |
| The client stops with `protocol v5` after a server's hello never arrives | The server is older than this build: it reads version 5, fails its own check and closes that connection. Upgrade the server. |
| `Authentication failed` on the client | `default_token` differs between client and server. |
| `Failed to connect to <addr>: Connection refused` | Server not running, wrong `client.control.default_remote_addr` port, or `server.control.bind_addr` not reachable. |
| Config starts but the connection fails with a resolve error (`failed to lookup address information`) | These address keys are only checked for a `:` in the string, not parsed as socket addresses: `client.control.default_remote_addr`, `client.services.<name>.remote_addr`, `client.data.default_data_addr`, `server.data.bind_addr`. A bare IPv6 literal such as `"::1"` therefore passes startup and has no port, failing when the address is resolved. Always write host **and** port, bracketing IPv6 literals — `"[::1]:2333"`. (A service's `remote_bind_addr` is parsed as a `SocketAddr` and rejected at startup instead.) |
| Repeated `Heartbeat timed out` | The network path drops the connection, or the server stalls. A configured timeout *below* the derived floor does not appear here — it is refused at startup. |
| Noise handshake fails | Keypairs, `psk`, or pattern mismatch between the two sides. |
| `Proxy URL is missing the port` at startup | The `proxy` URL lacks a port; fix the config. |
| UDP traffic not flowing | Check `protocol = "udp"`; datagrams larger than `udp_buffer_size` are truncated to it; idle mappings time out after `udp_idle_timeout` seconds. |
| Stateful UDP sessions (games, QUIC, WireGuard) break mid-session | Ensure both ends run a version with UDP session affinity (≥ this fix); a peer whose traffic idles longer than `udp_idle_timeout` is re-bound to a fresh local socket (new source port) on the next datagram — raise the timeout or send periodic traffic. |
| `Failed to read cmd: early eof` warnings | The peer closed the channel (restart or shutdown); the client reconnects automatically. |
| `Interface <tun> does not exist. Prepare it first` (client, or a registration rejection on the server) | A transparent service attaches to a device the operator creates; the daemon never creates one. Run the `ip tuntap add dev <tun> mode tun` and `ip link set <tun> up mtu 1400` lines the message prints (recipes: [Deployment](./deployment.md#transparent-services)). |
| `This server does not serve transparent (L3) services: [server.transparent] is not configured` (registration rejection) | The server's own switch is off, so the registration was refused by policy before the device was looked at. Add the table to the **server's** config if this host is meant to route the claimed address, or drop the service from the client. |
| `Transparent service claims <ip>, but no local interface carries it` | The client must own the address it claims: run the `ip addr add <ip>/32 dev <tun>`, `ip rule add from <ip> lookup 100` and `ip route add default dev <tun> table 100` lines the message prints. |
| `Reverse-path filtering is on (net.ipv4.conf.<tun>.rp_filter = 1)` | Injected packets carry the visitor's source address, which a strict check drops. Run the `sysctl -w` line the message prints; both `net.ipv4.conf.<tun>.rp_filter` and `net.ipv4.conf.all.rp_filter` must read `0`. |
| `Address <ip:port> is already claimed by another transparent service on this server` | Two clients claim the same endpoint; the first claim is held for the lifetime of its registration. Give one of them another address or port. |
| `protocol = "transparent"` is refused inside a `[client.services.<name>]` entry, with a message naming `[transparent.claims.<name>]` | A client that owns its public address is not a forwarding client: move the entry to a `[transparent]` block and start that process with `--transparent`. See [Transparent (L3) services](#transparent-l3-services). |
| `[client.transparent]` is refused as "configuration this version moved" | The device belongs to the mode now: `[client.transparent].tun` → `[transparent].tun`, and each transparent service becomes a `[transparent.claims.<name>]` entry. |
| A `[transparent]` block refused at startup with `... and this platform is not Linux`, or with a message naming the missing `transparent` feature | Carrying whole IP packets needs a Linux build with the `transparent` feature (part of the default set). On another platform, keep to a forwarding `[client]`. |
| A claimed address's visitors get nothing, and `MOLEHILL_L3_STATS=1` counts `unclaimed` drops | The routing is incomplete: the server needs a route for the claimed address into its device and the client the `from <ip>` rule plus its table route (see [Deployment](./deployment.md#transparent-services)). `unclaimed` is also the reconnect window, while no member holds the endpoint; `no_channel` means a member's queue was full, or that the member a flow is placed on is gone — the per-slot lines say which. |
