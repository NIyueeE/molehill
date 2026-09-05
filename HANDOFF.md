# HANDOFF: Working State & Future Work

> **2026-09-05 — v0.7.1: project base rebuilt on
> [rust-agents-template](https://github.com/NIyueeE/rust-agents-template).**
> Layered githooks (fast pre-commit / heavy pre-push, `check-docs`,
> `check-secrets`), `just setup` / `just check`, changelog-gated tag-driven
> releases, pinned CI actions, CD test-build workflow, bilingual governance
> docs (checks / lint-policy / release / structure), `deny.toml`,
> dependabot, CONTRIBUTING / SECURITY. Git history was rebuilt in the same
> change: upstream rathole history intact, the 35 fork commits squashed into
> six release commits (v0.6.0…v0.7.0, trees byte-identical to the old tags),
> tags re-cut as annotated ones. Deferred from the template's lint set:
> `clippy::pedantic` deny and `missing_docs` warn — a dedicated migration for
> molehill's public API, tracked in the backlog below.

---

## Shipped in 0.7.0

Status as of the mux-stabilization commit. Completed 0.7.0 items are recorded
below as context; only genuinely future work stays in the backlog.

- **Dynamic service registration** (protocol v2): the server has no
  per-service config. Clients send `RegisterService` after auth; the server
  enforces `allow_ports` (+ explicit privileged-port listing), binds eagerly,
  clamps `pool_size` by `max_pool_size`, and returns precise rejection reasons.
  Legacy `[server.services.*]` mode is gone.
- **Multiplexed data channels**: `multiplex` is in the default feature set and
  `mux = true` is the default. One yamux tunnel per control session carries
  every data channel as a stream; `mux = false` is the tested
  one-connection-per-channel mode.
- **Safe batch**: `pool_size`, `udp_buffer_size` (u16-capped, heap-backed),
  `udp_idle_timeout`, `udp_sendq_size`.
- **Logging**: colored levels, span context, TTY-only ANSI, `NO_COLOR`.
- Protocol v2 rejects version mismatches; there is no legacy protocol
  compatibility path.

## Multiplexed data-path fix (kept as design note)

Root cause: rust-yamux opens outbound streams lazily — the SYN flag rides on
the first outbound frame. A pooled molehill channel is server-speaks-first
(`StartForward*`) and reads before writing, so the SYN was never emitted:
client and server waited on each other silently. The fix is a zero-length
write kick in `ClientTunnel::start` before handing out a stream; yamux sends
the SYN on that empty data frame. `read_first_stream_is_announced_to_the_server`
regresses this exact sequence.

## UDP session affinity (kept as design note)

Root cause of stateful-UDP breakage (Minecraft Bedrock/RakNet sessions torn
in half, one peer seen on two local source ports): the v0.6.2 UDP pool
load-balancing let every pooled worker `recv_from` the service socket, so
the kernel handed each datagram of one peer to an arbitrary channel; the
client kept a private peer→socket map per channel, so the peer's packets
left through different local sockets. `DEFAULT_UDP_POOL_SIZE` is 2, so the
default UDP config triggered it.

Fix (two halves, one contract — *one peer, one path, one source port*):

- **Server**: a single reader task owns the service socket and routes each
  peer address to one data channel via an affinity table (`UdpRoute`,
  TTL-evicted after 300 s). A full worker queue drops the datagram
  (`try_send`) instead of head-of-line blocking other peers; a dead worker
  self-removes (`UdpWorkerGuard`, unwind-safe) and the pool requests a
  replacement channel so it keeps its size.
- **Client**: a per-service `UdpHub` replaces the per-channel peer maps.
  Each peer gets exactly one local forwarder socket for its whole session
  (created on first datagram, outside the lock to avoid blocking other
  channels on DNS), and its outbound datagrams are pinned to the channel
  its inbound traffic arrives on, falling back to any live channel when
  that one dies. The peer's source port therefore survives server-side
  re-sharding and channel churn.

Inherent limit, documented in docs/configuration.md: after
`udp_idle_timeout` (default 60 s) without traffic the forwarder socket is
recycled, so the next datagram re-binds a fresh source port. Stateful UDP
needs traffic within the window (RakNet keepalives qualify).

Regression tests: `core::server::tests::*` (routing semantics, sync) and
`udp_session_affinity` (end-to-end: a 64-datagram burst from one peer must
arrive at the local service from exactly one source address and all be
echoed back).

## Verification at commit time

| Suite | Result |
|---|---|
| `cargo test --lib` (default features) | 51 passed |
| `cargo test --lib --no-default-features --features server,client` | 44 passed |
| `cargo test --lib --no-default-features --features embedded` | 45 passed |
| `cargo test --test integration_test` (default; tcp/tls/noise/websocket × mux±) | 2 passed |
| `cargo test --test integration_test` (rustls+mux, sequential) | 2 passed |
| `cargo test --test integration_test` (rustls no-mux, sequential) | 2 passed |
| `cargo clippy --all-targets -- -D warnings` (default / rustls / rustls+mux / server+client-only) | clean |
| `cargo fmt --check` | clean |
| static musl release build (rustls+mux feature set) | ok |
| container image build (buildah, scratch, amd64) + rootfs smoke + mux e2e | ok |
| release-image smoke test | added to `release.yml` |

---

## Backlog

### Next recommended improvement: single control channel per client

Dynamic registration removed per-service server config, but the client still
keeps one control connection **and** one mux tunnel per service. Consolidate
to one physical control connection per client (plus one shared tunnel), with
register/unregister messages multiplexed over it. With mux now stable this is
mostly plumbing and yields another order-of-magnitude FD/handshake reduction
for many-service clients.

### Other deferred work

- [ ] HTTP API for configuration (hot reload currently files-only)
- [ ] Per-service visitor IP allowlist (`allowed_visitors`)
- [ ] Per-service bandwidth limiting (token bucket around copy loops)
- [ ] Lower default `udp_sendq_size` (64–128)
- [ ] Gate tracing span creation on level filter if profiling shows overhead
- [ ] QUIC / KCP transport (quinn if pursued; KCP not worth it)
- [ ] Buffer pooling under high churn (measure first)
- [ ] Zero-copy splice/sendfile: deliberately not recommended (keep as-is)

## References

- rust-yamux: <https://github.com/paritytech/yamux>
- rathole benchmark: <https://github.com/rathole-org/rathole#benchmark>
- frp docs: <https://gofrp.org/en/docs/>
