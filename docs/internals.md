# Internals

## Concepts

- **Service**: the entity whose traffic needs forwarding (e.g. an SSH server)
- **Server**: the publicly accessible host running molehill in server mode
- **Client**: the host behind NAT running molehill in client mode; it holds the services to be exposed
- **Visitor**: someone who connects to a service through the server
- **Control channel**: a connection between the server and the client that carries control commands for one registered service
- **Data channel**: one stream of forwarded traffic between the server and the client — either a dedicated transport connection, or (with `multiplex`) a yamux stream inside the tunnel
- **Tunnel** (`multiplex` feature): an extra connection, upgraded to a yamux session, that carries many data channels as streams

## Startup and registration

In client mode, molehill creates one control channel per configured service, all connecting to `server.bind_addr`. The server owns no service configuration: after authentication the *client registers* each service by sending its name, type, public endpoint (`remote_bind_addr`) and desired pool size, and the server validates the registration against its policy before exposing anything — `allow_ports` whitelist, explicit privileged ports, and port conflicts (which surface as precise rejections because the bind precedes the ack). The user-facing rules and error messages are in [Configuration](configuration.md).

On success the server binds the endpoint and starts serving visitors; on failure it replies with the exact reason and the client gives up for that service instead of hammering the server with doomed retries.

## Authentication

When a control channel is established, the server challenges the client with a random nonce. The client responds with `sha256(default_token || nonce)`. A wrong token makes the server close the channel; the client logs `Authentication failed` and retries. The protocol version rides in every hello; mismatched versions are rejected loudly ("please update").

## Forwarding

When a visitor connects to a registered service's endpoint, the server sends a `CreateDataChannel` command over the corresponding control channel. The client opens a data channel back, the server marks it with a `StartForwardTcp`/`StartForwardUdp` command, and the pair copies the visitor's bytes bidirectionally.

To reduce first-visitor latency, data channels are pre-created as a pool (per-service `pool_size`, default 8 for TCP / 2 for UDP, clamped by the server's `max_pool_size`). New channels are requested on demand: per visitor for TCP, and whenever a UDP channel dies so the pool keeps its size.

For UDP, the server maintains a per-service **session-affinity table**: a single reader task accepts datagrams from the service socket and routes every peer address to one data channel for the entry's lifetime (TTL-evicted after 300 s of inactivity). Routing every peer to a fixed channel — instead of letting all workers race on the socket — is what keeps one peer's packets on one path; the pool shards *distinct peers*, not packets.

### Multiplexing

With the `multiplex` feature (part of the default feature set) and `mux = true` (the default), the client dials one extra connection per control session right after registering — the *tunnel* — announced with a distinct hello so the server upgrades it to a yamux session too. From then on:

- `CreateDataChannel` no longer dials a fresh TCP(+TLS/Noise) connection; the client simply opens a new stream on the tunnel.
- The server feeds accepted streams into the same pool/pairing logic used for plain channels.
- Per-stream framing is identical to the plain path (`StartForward*` command first), which keeps both modes testable against each other.
- rust-yamux auto-tunes each stream's receive window towards the bandwidth-delay product, avoiding the fixed-small-window throttling known from stock yamux deployments.
- yamux opens outbound streams lazily (the SYN flag rides on the first outbound frame). Because this protocol is server-speaks-first, the client driver kicks each fresh stream with a zero-length write so a read-only pooled stream is announced immediately.

`mux = false` restores the one-connection-per-channel behavior, which measures slightly higher raw throughput on fast reliable links at the cost of handshakes.

## UDP

UDP services are forwarded over the same data channels, framed with a small header (source address + length). On the server side, each peer is pinned to one data channel by the session-affinity table above. On the client side, a per-service hub maps every peer address to exactly one local forwarder socket for the peer's whole session — the `(ip, port)` tuple the local service sees stays stable across channel re-sharding and channel loss — and pins the peer's outbound traffic to the channel its inbound traffic arrives on, falling back to any live channel when that one died. Idle forwarders are cleaned up after `udp_idle_timeout` seconds (default 60); re-binding after that changes the local source port, which stateful protocols notice as a new session. Datagrams larger than the service's `udp_buffer_size` are dropped in-stream while the channel stays usable. All queues enqueue with `try_send` and drop on overflow: UDP semantics, and a single slow peer can never stall others sharing the channel.

## Heartbeat

The server sends application-layer heartbeats on each control channel every `heartbeat_interval` seconds (`0` disables sending). The client expects some control command within `heartbeat_timeout` seconds; otherwise it treats the channel as dead and reconnects. `heartbeat_timeout` must be greater than `heartbeat_interval`.

## Hot reload

When the config file changes, the watcher compares the old and new configs: general changes (transport, addresses, tokens, mux settings) trigger a full restart of the instance; client service-level changes (add, remove, or modify a service) are applied without restarting — the affected control channels are torn down or created, which unregisters/re-registers exactly those services on the server.
