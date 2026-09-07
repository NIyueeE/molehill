# HANDOFF: Working State & Future Work

> State as of 2026-09-06, on top of v0.7.2. The UDP session-affinity fix, the
> template lint migration and the benchmark-matrix rework (uv/PEP 723, schema
> v3 through-tunnel measurements) have landed; shipped work is recorded in
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
- [ ] QUIC transport (quinn; structural change — needs the loss-cell data to
      justify) / KCP as an experimental transport feature: pin `kcp` 0.6 (the
      C-reference crate binding is active; `tokio-kcp` is stale) behind a
      thin in-repo async adapter
- [ ] Buffer pooling under high churn (measure first)
- [ ] Replace the python (uv/PEP 723) bench/test entries with `cargo-script`
      once it reaches Rust stable — the single-language test entry would drop
      the uv/python runtime dependency; until then `uv run` stays the entry
- [ ] Zero-copy splice/sendfile: deliberately not recommended (keep as-is)

### Benchmark ritual (per tag, see docs/release.md)

`just bench` → `just bench-plot` → `just bench-check` must be green before
tagging: results JSON + chart + README table land in the release commit, and
performance may not regress vs the previous tag (thresholds env-tunable, see
`benches/scripts/bench/check_regression.py`; all bench entries are PEP 723
python scripts run via `uv run` — no shell test entries). Machine notes: loss
cells need `CAP_NET_ADMIN` (granted in the current container — netem cells
run; without it loss cells auto-skip and rtt cells run via the userspace
`weakproxy.py` fallback); bore's `--to` only accepts a bare host (port 7835
implied), so proxied cells shift its control port to 127.0.0.2. Runner
lifecycle: a global lock refuses concurrent runs (they used to reap each
other's live processes); Ctrl-C/SIGTERM leave a clean state (arms killed,
netem removed, full meta checkpointed); `--fresh` backs up the previous
results file to `.bak` first; the full matrix is ~3 hours at full rigor
(trim with `--tools/--cells/--variants`).

Known waiver (v0.7.2 era): results files before schema v3 measured throughput
by dialing the **backend directly** (bypassing the tunnel), so every tool
reported the loopback iperf3 ceiling (~46 Gbit/s) regardless of tool or cell;
the rtt cells ran via `weakproxy` (client↔server delay only), which the
bypassed throughput never traversed. `results-v0.7.2.json` was refreshed in
place with schema-v3 through-tunnel data; until the next tag exists, the gate
compares v3 data against the v1 `results-v0.7.0.json` baseline, whose
throughput/RTT numbers are **not comparable** — expect (and ignore) gate
failures on those metrics until v0.7.3 provides a same-methodology baseline.
The old v0.7.2 host-mismatch waiver (loopback 1-stream -10.1%) is subsumed by
this: both sides were measuring the backend, not the tunnel.

## References

- rust-yamux: <https://github.com/paritytech/yamux>
- rathole benchmark: <https://github.com/rathole-org/rathole#benchmark>
- frp docs: <https://gofrp.org/en/docs/>
