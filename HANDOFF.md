# HANDOFF: Working State & Future Work

> State as of 2026-09-05, on top of v0.7.1. The UDP session-affinity fix and
> the template lint migration have landed; shipped work is recorded in
> [CHANGELOG.md](CHANGELOG.md), and design details (protocol, muxing, UDP
> session affinity) live in [docs/internals.md](docs/internals.md). This file
> only tracks what is still open.

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
