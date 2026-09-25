# Internals

## Concepts

- **Service**: the entity whose traffic needs forwarding (e.g. an SSH server)
- **Server**: the publicly accessible host running molehill in server mode
- **Client**: the host behind NAT running molehill in client mode; it holds the services to be exposed
- **Visitor**: someone who connects to a service through the server
- **Control channel**: a connection between the server and the client that carries control commands for one registered service
- **Data channel**: one stream of forwarded traffic between the server and the client — either a dedicated transport connection, or (with `multiplex`) a yamux stream inside the tunnel
- **Tunnel** (`multiplex` feature): an extra connection, upgraded to a yamux session, that carries many data channels as streams
- **Stripe group**: a set of `K` data channels that carry one visitor connection together (see "Data-channel striping")

## Startup and registration

In client mode, molehill creates one control channel per configured service — by default all connecting to `server.control.bind_addr`, or to each service's own `remote_addr` (`[client.services.<name>]`) when that overrides the client-wide control endpoint. The server owns no service configuration: after authentication the *client registers* each service by sending its name, type, public endpoint (`remote_bind_addr`) and desired pool size, and the server validates the registration against its policy before exposing anything — `allow_ports` whitelist, explicit privileged ports, and port conflicts (which surface as precise rejections because the bind precedes the ack). The user-facing rules and error messages are in [Configuration](configuration.md).

On success the server binds the endpoint and starts serving visitors; on failure it replies with the exact reason and the client gives up for that service instead of hammering the server with doomed retries.

## Authentication

When a control channel is established, the server challenges the client with a random nonce. The client responds with `sha256(default_token || nonce)`. A wrong token makes the server close the channel; the client logs `Authentication failed` and retries. The protocol version rides in every hello; mismatched versions are rejected loudly ("please update").

## Forwarding

When a visitor connects to a registered service's endpoint, the server sends a `CreateDataChannel` command over the corresponding control channel. The client opens a data channel back, the server marks it with a `StartForwardTcp`/`StartForwardUdp` command, and the pair copies the visitor's bytes bidirectionally.

To reduce first-visitor latency, data channels are pre-created as a pool (per-service `pool_size`, default 8 for TCP / 2 for UDP, clamped by the server's `max_pool_size`). New channels are requested on demand: per visitor for TCP, and whenever a UDP channel dies so the pool keeps its size.

For UDP, the server maintains a per-service **session-affinity table**: a single reader task accepts datagrams from the service socket and routes every peer address to one data channel for the entry's lifetime (TTL-evicted after 300 s of inactivity). Routing every peer to a fixed channel — instead of letting all workers race on the socket — is what keeps one peer's packets on one path; the pool shards *distinct peers*, not packets.

### Data-channel striping

With `[server.data]stripe_count = K` (default `1`), the server pairs every visitor connection with `K` data channels instead of one and labels each with a `StartForwardStripedTcp(group, index, K)` command. The pair then forwards the connection over the group (`src/stripe.rs`):

- Each direction numbers its chunks (`[u64 seq][u16 len][payload]` frames, 32 KiB payloads) and spreads them round-robin over the group's channels.
- The receiving side reassembles by sequence number: out-of-order chunks wait in a bounded reorder map, contiguous ones are written to the destination. A frame always travels whole on one channel (a half-written frame cannot move — it would corrupt that channel's framing); a channel that refuses a frame *before* any byte of it is committed is skipped for that frame, so one backpressured channel does not stall the group.
- A channel that ends mid-frame (not at a frame boundary) breaks the group instead of leaving the reassembler waiting for a sequence number that will never arrive.

Three things follow from the arithmetic: the visitor's throughput ceiling is the sum of its channels' ceilings (a single stream is no longer capped by one tunnel), its in-flight window is the sum of the channels' windows, and each channel's framing work is driven by its own task. The cost is the reorder buffering (bounded by the engine's per-stream window plus one reorder queue per direction) and one data-channel wire addition — the striped command rides *after* the unchanged `StartForward*` commands, and channels that do not carry it are byte-identical to the unstriped path, so the yamux wire format (and 0.8.x peer interoperability) is untouched. Striping applies to TCP services; UDP keeps its one-channel-per-peer shape, where session affinity is the stronger constraint.

### Multiplexing

With the `multiplex` feature (part of the default feature set) and `mode = "multiplex"` (the default), the client dials N connections per control session right after registering (`[client.data].default_count`, default 4, overridable per service) — the *tunnels* — each announced with a distinct hello so the server upgrades them to yamux sessions too. Data-channel opens spread across the tunnels round-robin (a dead tunnel is skipped transparently until the heartbeat-driven reconnect replaces the pool). The tunnels dial the service's data endpoint (`[client.services.<name>].remote_addr` when set, else `[client.data].default_data_addr`, else the control endpoint) and are accepted by the server's data listener — the control listener itself when the addresses match, otherwise `[server.data].bind_addr`. From then on:

- `CreateDataChannel` no longer dials a fresh TCP(+Noise) connection; the client simply opens a new stream on the tunnel.
- The server feeds accepted streams into the same pool/pairing logic used for plain channels.
- Per-stream framing is identical to the plain path (`StartForward*` command first), which keeps both modes testable against each other.
- The framing engine is maintained in-repo (`src/mux/`, vendored from rust-yamux 0.14 — wire-identical with the yamux specification; the vendoring rationale and its per-lever outcomes are recorded in HANDOFF.md "What landed" / "Optimization route"). It is tokio-native (tokio IO traits, no compatibility shim on the data path) and auto-tunes each stream's receive window towards the bandwidth-delay product, avoiding the fixed-small-window throttling known from stock yamux deployments.
- yamux opens outbound streams lazily (the SYN flag rides on the first outbound frame). Because this protocol is server-speaks-first, the client driver kicks each fresh stream with a zero-length write so a read-only pooled stream is announced immediately.

`mode = "direct"` restores the one-connection-per-channel behavior: every data channel is its own transport connection, so nothing is multiplexed and each visitor connection pays the full connection setup (TCP connect plus, with `noise`, the Noise handshake). That is a design trade-off, not a performance claim — the measured comparison lives in [Benchmarks](benchmarks.md#what-each-configuration-choice-costs-per-decision-measurements), and the choice between the two is the decision tree in [Configuration](configuration.md).

## UDP

UDP services are forwarded over the same data channels, framed with a small header (source address + length). On the server side, each peer is pinned to one data channel by the session-affinity table above. On the client side, a per-service hub maps every peer address to exactly one local forwarder socket for the peer's whole session — the `(ip, port)` tuple the local service sees stays stable across channel re-sharding and channel loss — and pins the peer's outbound traffic to the channel its inbound traffic arrives on, falling back to any live channel when that one died. Idle forwarders are cleaned up after `udp_idle_timeout` seconds (default 60); re-binding after that changes the local source port, which stateful protocols notice as a new session. Datagrams larger than the service's `udp_buffer_size` are dropped in-stream while the channel stays usable. All queues enqueue with `try_send` and drop on overflow: UDP semantics, and a single slow peer can never stall others sharing the channel.

### UDP drop counters (`MOLEHILL_UDP_STATS`)

The visitor-datagram reader never blocks: a full worker queue drops the
datagram (what UDP peers already tolerate) rather than head-of-line blocking
every other visitor, and the reader separates "no data channel is ready yet" —
the registration/reconnect window — from queue pressure. Both drops were
previously visible only as `debug!` lines, so "is the queue depth right?" could
not be answered with evidence.

With `MOLEHILL_UDP_STATS=1` the server logs a cumulative
`udp-stats: cumulative visitor-datagram drops` line once a second carrying
`queue_full` and `no_worker` separately; being cumulative, per-second rates
come from consecutive lines. The switch is independent of
`MOLEHILL_KCP_STATS` because the default carrier is TCP — a run can exercise
the UDP path with no KCP session in existence. The runner records whichever
`MOLEHILL_*` switches a run inherited in its results meta (`instrumentation`),
so an instrumented run is never mistaken for a clean one.

## Heartbeat

The server sends application-layer heartbeats on each control channel every `[server.control].heartbeat_interval` seconds (`0` disables sending). The client expects some control command within `[client.control].default_heartbeat_timeout` seconds (overridable per service); otherwise it treats the channel as dead and reconnects. The timeout must be greater than the server's `heartbeat_interval`.

## Hot reload

When the config file changes, the watcher compares the old and new configs: general changes (transport, addresses, tokens, data-plane settings) trigger a full restart of the instance; client service-level changes (add, remove, or modify a service) are applied without restarting — the affected control channels are torn down or created, which unregisters/re-registers exactly those services on the server.
