# HANDOFF: Future Performance Optimization Designs

This document records the design and roadmap items that are **deliberately
deferred** from the current release. Each design below is self-contained
enough to pick up in a later milestone without re-doing the research; the
frp architecture comparison they are based on was reviewed against the frp
source and official docs (full citations are in the git history, commit
`docs: handoff of future perf designs, frp research report, bump to 0.6.4`).

## Current state (already shipped)

- TCP_NODELAY + keepalive on every leg of a forwarded service (data
  channels, visitor sockets, the client's local connection)
- 32 KiB per-direction copy buffers (`copy_bidirectional_with_sizes`)
- UDP framing: one `write_all` per datagram into a reused scratch buffer,
  zero-allocation receive path on the server (`read_slice`)
- Oversized UDP packets are dropped in-stream instead of killing the data
  channel
- Client-side health check that removes/re-registers a service on the server

Known resource trade-offs that remain hardcoded: `TCP_POOL_SIZE = 8`,
`UDP_POOL_SIZE = 2`, `UDP_SENDQ_SIZE = 1024`, `UDP_BUFFER_SIZE = 2048`.

---

## Roadmap (deferred feature requests)

Formerly tracked in the README "Planning" section; consolidated here so all
plans live in one place.

- [ ] HTTP APIs for configuration (hot-reload currently relies on file
      watching)
- [ ] Configurable UDP receive buffer size (`udp_buffer_size`) for large
      datagrams (e.g. games) — wire-compatible up to 65535 (`u16` length
      field); see also the smaller follow-up on `UDP_BUFFER_SIZE` below
- [ ] Per-service visitor IP allowlist (`allowed_visitors`)
- [ ] Configurable UDP idle timeout (`udp_idle_timeout`; currently
      hardcoded `UDP_TIMEOUT = 60` seconds)
- [ ] Per-service connection limit (`max_connections`)

---

## Design A: Data channel multiplexing (yamux) — highest value

**Problem.** Every forwarded connection costs a fresh TCP handshake plus
1–2 crypto handshakes (TLS or Noise) on each side. The pre-established pool
(`TCP_POOL_SIZE = 8`) exists only to amortize that cost; on high-RTT links
it is invisible to new visitors, and on embedded devices 8 pooled
connections × N services is a real FD/memory burden.

**Approach.** Multiplex many logical streams over one physical connection,
like frp does with its yamux fork (default on).

- Implement as a new `Transport` wrapper, e.g. `MultiplexedTransport<T>`,
  so the existing generic `Client<T>`/`Server<T>` code is reused:
  - server side: after the control-channel handshake, upgrade the
    connection into a yamux server session; each accepted stream is a data
    channel;
  - client side: the data-channel connect path opens a stream on the
    existing session instead of dialing a new connection.
- Crate: `rust-yamux` (libp2p's yamux, maintained). It is a session
  abstraction over any `AsyncRead + AsyncWrite`, which fits the
  `Transport::Stream` model well.
- Protocol compatibility: add a negotiation bit to `Hello`/`Auth`
  (postcard; `CURRENT_PROTO_VERSION` machinery already exists). If both
  sides agree, upgrade; otherwise fall back to the current one-connection-
  per-channel behavior. Old peers keep working — rollback is a config flip.
- **Critical lesson from frp**: upstream yamux's default stream window
  (256 KiB) throttles high-BDP links. frp forks yamux solely to raise
  `MaxStreamWindowSize` to 6 MiB. Whatever crate is chosen, the window
  must be an explicit configuration item with a generous default
  (≥ 4–6 MiB). Do not ship the upstream default.
- Feature-gate (`multiplex`) so the `minimal`/`embedded` builds stay small.

**Expected benefits.** Eliminates per-connection handshake latency; cuts
FD usage by an order of magnitude under many concurrent connections; the
pool becomes nearly irrelevant (frp's docs note the pool only pays off
without mux), so `TCP_POOL_SIZE` can drop to 0–1 in mux mode.

**Risks.** Transport-level head-of-line blocking (one stream's loss delays
others); extra framing overhead per stream; more moving parts in the
connection lifecycle. frp keeps `tcpMux = false` as an escape hatch —
molehill should keep the non-mux path selectable for throughput-sensitive
deployments (rathole's own benchmark shows the no-mux model wins
loopback/CPU-bound throughput).

**Suggested milestone:** major version (protocol negotiation + core data
path refactor), paired with Design B and Design F.

---

## Design B: Configurable connection pool sizes

**Problem.** `TCP_POOL_SIZE = 8` / `UDP_POOL_SIZE = 2` are compile-time
constants (`src/core/server.rs`). 20 services = 160 pre-established
TLS/Noise connections kept alive at all times.

**Approach.** Mirror frp's `poolCount`/`maxPoolCount`:

- `ServerServiceConfig.pool_size` (default 8 for TCP, 2 for UDP) — how
  many data channels the server pre-creates per service;
- optionally a server-global `max_pool_size` cap so one misconfigured
  service cannot exhaust FDs;
- document the latency-vs-resources trade-off (smaller pool = fewer idle
  connections, higher first-visitor latency).

Low risk, ~30 lines plus docs; can ship in any minor release.

---

## Design C: QUIC / KCP transports

**Problem.** TCP+TLS/Noise handshake per data channel is the dominant
connection-setup cost on high-RTT links. frp offers KCP and QUIC as
transports; QUIC gives 0-RTT resumption and built-in stream multiplexing.

**Approach.** Add a `quic` transport implementing the existing `Transport`
trait with `quinn` (client) / `s2n-quic` (server). With 0-RTT, a data
channel is a single stream open on a resumed session. This subsumes
Design A for QUIC deployments.

**Cost.** Large: new dependency surface, certificate/ALPN config, NAT
traversal behavior differs (UDP-based). Treat as a long-term option; KCP
is probably not worth it (frp keeps it for legacy reasons, QUIC is the
better answer).

---

## Design D: Buffer pooling

**Problem.** `copy_bidirectional_with_sizes` allocates its 32 KiB buffers
per connection; under high connection churn (many short visitors) this is
repeated 2×32 KiB allocation/free churn. frp pools copy buffers in a
size-tiered `sync.Pool`.

**Approach.** A small `object-pool`-style pool (or a hand-rolled
`Vec<BytesMut>` stack, there is no need for a crate) handing out
`BytesMut` of 32 KiB with LIFO reuse, bounded (e.g. 1024 entries) to
avoid unbounded retention. Wire it into the two `copy_bidirectional_with_sizes`
call sites (client local leg, server pool loop). Measure before/after —
the win is only visible under connection churn, and it adds a small
`unsafe`-free complexity cost.

---

## Design E: Zero-copy (splice/sendfile) — deliberately not recommended

frp does not use it either; both projects are userspace double-copy. On
the unencrypted `tcp` transport one could `splice(2)` between the two
sockets, but it is Linux-only, interacts badly with TLS/Noise/WebSocket
transports (which need userspace), and the kernel-side copy is already
fast. Do not spend effort here; revisit only if a benchmark shows the
userspace copy as the bottleneck on a specific deployment.

---

## Design F: Single control channel per client (consolidation)

**Problem.** Each service owns a control channel: N services = N physical
TLS/Noise connections + N heartbeat streams + N× pool overhead. frp uses
one control connection per frpc with per-proxy messages.

**Approach.** Evolve the protocol: one control channel per client that
registers/unregisters services (`RegisterService`/`UnregisterService`
messages); the server keeps a session-level service map instead of
indexing control channels by service digest. This is a protocol + server
state refactor — pair it with Design A so the consolidated channel can
also carry multiplexed data streams ("connection-model modernization").
Hot-reload service add/remove becomes a message, not a connection
lifecycle event.

**Cost.** High; keep for a major version. The health-check feature already
uses "drop control channel = unregister" semantics, which fits the
per-service model; a consolidation would move that to an explicit message.

---

## Smaller follow-ups (low cost, low risk)

- Make `UDP_SENDQ_SIZE` (currently 1024) smaller or configurable: each
  peer mapping can buffer up to ~2 MiB; under many short-lived peers this
  multiplies. 64–128 is likely enough to absorb bursts.
- Make `UDP_BUFFER_SIZE` (2048) configurable — note the framing carries a
  `u16` length, so up to 65535 is wire-compatible.
- Per-service bandwidth limiting (token bucket around the copy loops) —
  resource control, not throughput; useful for multi-tenant servers.
- Tracing: connection-scoped spans are created unconditionally; if
  profiling shows overhead at `info` level, gate span creation on the
  current level filter.

## References

- rathole benchmark (no-mux throughput): <https://github.com/rathole-org/rathole#benchmark>
- frp source and official docs used for the comparison: <https://github.com/fatedier/frp>,
  <https://gofrp.org/en/docs/> (the full annotated research report with
  per-file citations is preserved in the git history)
