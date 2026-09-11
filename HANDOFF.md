# HANDOFF: Working State & Future Work

> State as of 2026-09-10, on the v0.8.0 development line (branch
> `merge-tcp4`, preparing the merge to `main`). The UDP session-affinity fix,
> the template lint migration and the benchmark-matrix rework (uv/PEP 723,
> schema v3 through-tunnel measurements) have landed, and the benchmark
> measurement method was revised on 2026-09-10/11 (rate-cell shaping, per-rep
> throughput isolation, a UDP capacity ladder — see the "Method revision"
> paragraph below) and the v0.8.0 baseline was then re-measured in full from
> it on host `0b073ddbf222` (52 arms, zero holes, charts and README
> regenerated). Shipped work is
> recorded in [CHANGELOG.md](CHANGELOG.md), and design details (protocol,
> muxing, UDP session affinity) live in [docs/internals.md](docs/internals.md).
> This file only tracks what is still open.

## Backlog

### Next recommended improvement: single control channel per client

Dynamic registration removed per-service server config, but the client still
keeps one control connection **and** one mux tunnel per service. Consolidate
to one physical control connection per client (plus the shared tunnel
pool), with
register/unregister messages multiplexed over it. With mux now stable this is
mostly plumbing and yields another order-of-magnitude FD/handshake reduction
for many-service clients. (Per-service `mode`/`count`/`carrier` overrides
landed in 0.8, so mixing data paths per service is already possible without
waiting for the consolidation.)

### Other deferred work

- [ ] Leaner Noise record stream: the wrapper is now in-repo
      (`src/transport/noise_stream.rs`, vendored from snowstorm 0.4.0 and
      adapted to snow 0.10 — snowstorm is unmaintained and pinned to
      snow 0.9, so the upgrade required vendoring it). The ring-accelerated
      cipher measures ~6.6 Gbps/direction in snow's TransportState but only
      ~4.9 Gbps end-to-end; the gap is the wrapper's per-record copies +
      2-byte length read. A leaner AsyncRead/Write (read the length with
      the payload, decrypt straight into the caller's buffer) could recover
      part of it — easier now that the code is ours. The default pattern
      stays BLAKE2s: the cipher is ring-served for every pattern (the hash
      only runs in the handshake), so a pattern change would add wire churn
      for zero gain.
- [ ] KCP pacer slow-recovery study: PONG-timeout cuts (x0.75) recover at
      only +5% per 4 clean PONGs (2 s cadence) — on sustained loss the
      pacing rate can pin low for minutes. Not the dominant factor in the
      measured cells (window-bound), but worth revisiting if KCP gets
      production use.
- [ ] HTTP API for configuration (hot reload currently files-only)
- [ ] Per-service visitor IP allowlist (`allowed_visitors`)
- [ ] Per-service bandwidth limiting (token bucket around copy loops)
- [ ] Lower default `udp_send_queue_size` (64–128)
- [ ] Gate tracing span creation on level filter if profiling shows overhead
- [ ] QUIC transport on main (implemented and measured; parked in the
      `archive/transport-test` tag — N×TCP won every comparable cell and
      the QUIC leg lacks peer auth). Revisit if a UDP-only path or
      multi-stream loss isolation becomes a requirement
- [ ] Buffer pooling under high churn (measure first)
- [ ] Replace the python (uv/PEP 723) bench/test entries with `cargo-script`
      once it reaches Rust stable — the single-language test entry would drop
      the uv/python runtime dependency; until then `uv run` stays the entry
- [ ] Zero-copy splice/sendfile: deliberately not recommended (keep as-is)

- [x] Re-baseline the README benchmark chapter on the merged defaults
      (done in the v0.8.0 matrix): results-v0.8.0.json, six charts (incl.
      the new cost chart) and the tables now describe the count=4 default,
      the ring-accelerated noise rows, and the new cells/metrics.
      **Regression-gate verdict (v0.8.0 vs v0.7.2): void as a gate.** The
      v0.7.2 baseline ran on a DIFFERENT container (host 621a9d1d3f40 vs
      2f9bbec0ea67) with the single-tunnel default and unbounded probe
      durations (200-sample steady pings, e.g.), so the % thresholds are
      not comparable; the 6 flagged rows split into probe-methodology
      artifacts (steady RTT sampling, udp-loss dilution) and two signals
      that spot-verification confirmed stable on the new host: rtt100
      1-stream ~0.63-0.67 Gbps vs the old host's 0.752, and loss2b25 UDP
      loss ~5-7.5% vs 2.5% — both plausibly container-variance, recorded
      here as low-priority follow-ups (a same-host A/B would settle them).
      results-v0.8.0.json is the new regression baseline.

### Benchmark ritual (per tag — see docs/release.md)

The ritual steps, gate thresholds and the uv/PEP 723 runner are documented
in [docs/release.md](docs/release.md). Environment-specific notes: loss
cells need `CAP_NET_ADMIN` (granted in the current container — netem cells
run; without it loss cells auto-skip and rtt cells run via the userspace
`weakproxy.py` fallback); bore's `--to` only accepts a bare host (port 7835
implied), so proxied cells shift its control port to 127.0.0.2. Runner
lifecycle: a global lock refuses concurrent runs (they used to reap each
other's live processes); Ctrl-C/SIGTERM leave a clean state (arms killed,
netem removed, full meta checkpointed); `--fresh` backs up the previous
results file to `.bak` first; the full matrix is ~3 hours at full rigor
(trim with `--tools/--cells/--variants`).

Known measurement limits (2026-09-09, fixed in the runner, recorded in the
README methodology): on rate-limited cells (netem `rate` on loopback),
eight parallel iperf3 streams wedge iperf3's single-test server — netem's
packet `limit` counts GSO-sized segments (up to 64 KiB), so the buffer
holds seconds of data at 20 Mbit/s and the final results exchange never
completes; the wedged server then dies with EBADF, which used to poison
every later arm of the cell with refused dials. The runner now isolates
backends per arm (SIGKILL cleanup + EADDRINUSE retry) and runs the 8-stream
test after the cheap probes; the rate20 8-stream slot is null with the
timeout reason in `partial_metrics` (rate100 measures both). The 2026-09-09
targeted re-runs refreshed the rate cells, the loss2b25 steady-RTT probes
and the fake-zero HoL entries on the same host — `results-v0.8.0.json`
remains the regression baseline.

**Method revision (2026-09-10, this thread) — the above diagnosis was
half-wrong and is superseded.** Decisive measurements on this host with a
plain (tunnel-free) iperf3 pair across the shaped `lo`:
- The shaper itself was the bigger problem. `netem rate 100mbit delay
  20ms limit 1` carries 18 Mbit/s with `-P 8`; the same test at `limit
  1000` carries 99.6 Mbit/s. The rate cells had been shaped with a queue
  so shallow that whole GSO segments were tail-dropped, so a "rate cell"
  number was largely a property of the shaper. `bench_lib.RATE_QUEUE_LIMIT`
  is now 2000 and is recorded in the results meta (`netem_rate_limit`).
- The remaining `null`s were harness repetition hygiene, not the path: with
  per-rep isolation plus a client timeout scaled to the test length, the
  same kcp4 `rate100_rtt20` cell that used to return `null` measures 3/3
  reps at both 1 stream (~0.098-0.103 Gbit/s) and 8 streams
  (~0.091-0.106 Gbit/s) — i.e. at the 100 Mbit/s link ceiling. A stalled
  rep used to wedge the single-test iperf3 server and starve every later
  rep (`Backends.run_throughput` now restarts it after a failure).
- Throughput convention: the headline is bytes over the measured window,
  with the receiver's own (drain-inclusive) window recorded too, because at
  a shaped cell `sum_received.seconds` runs well past the sender's and made
  the two figures look like different tests.
- UDP capacity: one 20k-pps burst sat ~2x above the forwarder's knee
  (measured 2k/5k pps -> 0% loss, 10k -> 30% at ~9.5 Mbit/s delivered,
  20k -> 100%), so `loss_pct` carried no information. It is now two points
  (paced + saturating, reported by delivered Mbit/s), sampled BEFORE the
  bulk-UDP HoL blast because the UDP path does not recover within an arm
  after a 50 Mbit/s burst (the HoL probe's own pinger then records 100%
  loss) — a forwarder finding worth its own look.
- Two further measurement bugs were caught while validating that revision,
  both from reading the per-rep raw JSON the runner now keeps:
  1. iperf3's per-interval `seconds` is not the interval span (the interval
     after `-O` reports warm-up + interval), so summing it gave 9.0 s for an
     8 s test and deflated the loopback headline ~12%. The window is now
     `sum(end - start)` over non-omitted intervals.
  2. At `rate20_rtt40` the sender's post-omit count is 0 for the whole
     measured window — its `-O` warm-up dumped 153 MB at 1.22 Gbit/s into
     the shaper and backpressure blocked the rest — so every tool (molehill
     AND the TCP peers) reported a 0.0 Gbit/s 8-stream cell. The headline is
     now the sender's bytes over the measured window, falling back to the
     receiver's count only when the sender's accounting is degenerate
     (receiver > 2x sender); the rate20 8-stream value is then ~0.02 Gbit/s
     (the shaped link rate) instead of 0.
- `iperf-raw/` artifacts are written per ARM **and CELL** (the work dir is
  per run, so arms used to overwrite each other's evidence; a first fix
  without the cell name still let cells overwrite each other), and
  `audit_results.py` is the completeness gate: `None` holes, arm errors,
  per-stream inconsistencies, missing sampler output, and the throughput
  endpoint invariant (exposed port != backend port) — it exits non-zero on
  holes/errors. That endpoint check exists because the opposite mistake
  shipped a whole invalid baseline (see the VOID note above).

Consequence: **no pre-revision number is comparable to the refreshed
baseline** (the revision changed the shaping model and the throughput
window/accounting, and the refresh ran on `ebb615bff576`); the full
re-measure was done on 2026-09-10 (52 arms; `audit_results.py` reports zero
holes and zero arm errors) and `results-v0.8.0.json` + the charts + the
README chapter now describe that run. The v0.7.2 regression gate is
therefore informational only, and a rate-cell-only difference against it is
never a signal.

### Baseline refresh notes (2026-09-10, host ebb615bff576)

> **The refresh was re-run on 2026-09-10 after an endpoint bug, and the
> corrected baseline is what is committed now.** An intermediate refresh
> was measured with `Backends.run_throughput` dialing the iperf3 BACKEND
> instead of the tunnel's exposed port, so its TCP figures were the
> loopback ceiling with every tool bypassed (loopback 1-stream 48-54
> Gbit/s instead of the tunnel's ~10). Fixed by `fix(bench): dial the
> exposed endpoint and isolate every sample`: the endpoint
> is explicit (`_throughput_exposed_port` / `_bench_backend_port`), equal
> ports raise, and `audit_results.py` fails such a run. The final baseline is
> one same-host run of the corrected method (52 arms, zero holes, zero arm
> errors, guard clean) and the README/strategy numbers are derived from it.
> Caught by diffing a raw artifact's `connected` port against the cell's port
> map — the reason raw artifacts are kept, and the first rule of AGENTS.md
> §10.

- Corrected outcome, tunnel-measured on `0b073ddbf222` (one run of the
  revised method): Noise retains ~58%/76% of plain throughput, `count = 4`
  aggregates at 8 streams (loopback 19.5 vs 9.2 Gbit/s, 1% loss 12.3 vs
  4.5), and the KCP carrier stays far behind TCP wherever the path is not
  the bottleneck (loopback 8-stream 1.1 vs 14.9 Gbit/s, ~3x RSS = 83 vs
  26 MiB), with UDP-only paths and rtt100 session quality as its uses.
- **The UDP-under-load lead is retracted.** The head-of-line probe's paced
  pinger lost 100% of its datagrams on the default arms in two earlier runs
  and 2% in this one; the ladder probe shows no reproducible penalty either.
  Treat it as variance, not a path property. The code-level hypothesis
  (sticky per-peer affinity plus drop-on-full in `route_udp_datagram`, with
  the server's routed queue hardcoded to `DEFAULT_UDP_SENDQ_SIZE` = 1024 and
  `udp_send_queue_size` honoured only client-side) is still worth a look as a
  fairness question. **The instruments were run (2026-09-11) and settle
  nothing**: the stock pinger lost 8.3% then 10.8% in the same arm, and the
  rolling-socket control lost 100% — but that control is invalid (a 200 ms
  socket lifetime is shorter than the lazy UDP session establishment plus
  the ~100 ms path RTT, so replies to a closed socket are simply lost). No
  `queue full` drops were captured at that loss level. Net: the lead is
  variance; the fairness question stays open with no demonstrated defect.
  Re-running `~/tmp/udp_affinity_probe.py` (or `focused_run.py --hol-udp`)
  with a socket lifetime of ~1 s and `RUST_LOG=molehill_rathole=debug` is
  the next step if it is ever picked up.
- Structural gaps that remain (each with a typed reason in the data): the
  20 Mbit/s rate cell's 8-stream slot (`null`, client timeout — eight
  parallel streams cannot finish through that bottleneck) and kcp4's rtt100
  8-stream slot. Everything else is measured.
- The UDP ladder replaces the two-point probe: 500 pps to the configured
  burst rate, 2000-datagram bursts, 1.5 s drain, tolerance `max(2%, cell
  loss + 2pp)`. Measured: unshaped loopback is a lower bound (27.2 Mbit/s at
  the top step), the 10 ms cell bends at 12 000 pps (10.9 Mbit/s), the
  100 ms cell at 1 000-2 000 pps (0.7-1.4 Mbit/s); loss cells have no step
  inside tolerance, so the knee plus its delivered rate is the informative
  pair there.
- Container note: the environment was recycled twice during this work
  (hostnames `ebb615bff576` -> `fbf069a0506b` -> `0b073ddbf222`; the final
  baseline is entirely from `0b073ddbf222`, verified via the results meta).
  A recycle wipes `/tmp` AND the benchmark tooling: `sudo apt-get install -y
  iperf3 iproute2` (i.e. `just bench-deps`) must run again, and runs should
  keep their scratch under `~/tmp` (`TMPDIR=$HOME/tmp`), which survives.
- `focused_run.py` no longer applies a netem to unshaped cells and dials the
  exposed port, so it is a usable independent check (loopback ~10 Gbit/s,
  matching the matrix).
- History note (2026-09-11): the 24 fix-on-fix commits on top of `0f44213`
  were folded into the seven topical commits that follow it and `merge-tcp4`
  was force-pushed once. The pre-rewrite tip is preserved as
  `backup/merge-tcp4-20260911` (`4c70eeb`, identical tree); delete it once
  this branch has merged to `main`.
- Open before the next release (none block the merge): the UDP fairness
  question above (instrument ready, no claim), a single-window full-matrix
  re-run on the target host (this baseline was assembled across one run plus
  three targeted merges after the recycle), and the `merge-tcp4` -> `main`
  merge itself. The KCP loss cells were re-measured with the current
  post-conserve binary in this baseline, so that item is closed.

Chart/runner follow-up (2026-09-10): absent measurements used to render as a
fake `0.0` bar in the cost chart (`mux1`'s unmeasured 64-stream and
mixed-bulk slots), and the two structural nulls looked like opaque runner
failures. `plot_bench.py` now marks a value missing inside a compared panel
with a grey `x`, and no longer draws rows/cells that are not part of a
comparison: the molehill-vs-peers per-cell panels plot only the five cells
the peers run, the mux chart is a single loopback row, and the cost chart's
64-stream panel omits `mux1`. `bench.py` skips by design the 64-stream scale
point above an arm's `count × 32` yamux ceiling (reason in
`partial_metrics`);
`bench_lib.throughput()` surfaces iperf3's own error/exit status instead of a
bare `KeyError`; `hol_probe.ping_tcp` records a connected pinger that
completes zero or one round trip as a stall (`ping_max_gap_ms` = the elapsed
wait) instead of `null`.

Targeted refreshes on the re-created container (`ebb615bff576`; the file-wide
meta still reads the original baseline host `2f9bbec0ea67` / 2026-09-09):
`mux1` loopback (mixed bulk 9.0 Gbit/s, previously inherited the wedged
iperf3 server), and frp/rathole `rate20_rtt40` (HoL now 3003 / 1986 ms; both
had zero-or-one pinger replies over repeated runs, so the probe change — not
a transient — is what records them). The host could not reproduce the
baseline for `mux-off` (~20% low: 22.6 vs 28.0 Gbit/s 8-stream under current
load), so the rest of `results-v0.8.0.json` is untouched; a full-matrix
re-run on a quiet host is still needed before the next tag.

KCP optimization refresh (2026-09-10, full rigor, kcp4 arm only, merged
into `results-v0.8.0.json`; meta now reads `ebb615bff576` / 2026-09-10):
loopback 1-stream 2.496 -> 3.191 Gbit/s (+28%, A/B against the
pre-optimization code on the same host: 2.405), rtt10 8-stream 0.39 ->
0.653 (+67%), loss1 0.356/0.413 -> 0.418/0.686 (+17%/+66%), loss2b25
0.353/0.382 -> 0.393/0.691 (+11%/+81%), loss5 0.025/0.045 -> 0.029/0.038
(two complementary passes merged), rate100 0.023 -> 0.028, rate20 0.005
-> 0.004 (floor), jitter 0.127/0.114 -> 0.151/0.176, rtt100 0.04/0.051
-> 0.038/0.064. Cell fragility notes: loopback 8-stream measures ~1.4
today for BOTH code versions (A/B: old code 1.116, new 1.453; the
committed 5.9 was not reproducible on this host — same story as
`mux-off` above); rtt100 1-stream flaked to 0.008 in one pass and the
backfill's 0.038/old-code's 0.04 stand; rate100 1-stream flaked to
0.008 in the first pass, the backfill's 0.028 matches the old code's
0.025; rate100/rate20 8-stream stayed unrecordable: the 8-parallel-stream
iperf3 client timed out on every rep at the shaped bottleneck
(`iperf3 -P 8` wedges the single-test server; each attempt was killed
by the 30 s harness timeout — same reason the committed rate100 8s
was a fake zero and rate20 8s was already None). **Superseded by the
2026-09-10 method revision below: these slots are measurable now.** Peer
binaries are
cached under `~/tmp/bench-peers` (not `/tmp`, which session cleanup wipes)
and pinned to the baseline versions (frp 0.71.0 / rathole 0.5.0 / bore
0.6.0), fetched directly because the unauthenticated GitHub API was
rate-limited.

KCP in-repo self-maintenance (2026-09-10): the KCP protocol engine moved
from the `third_party/kcp` path dependency into `src/kcp/`, maintained as
molehill's own module whose algorithm follows the reference C
implementation by skywind3000. A path dependency cannot be published —
cargo strips it and compiles against the registry version, so `cargo
publish` would have failed against the unpatched crates.io kcp 0.6.0 (the
release workflow's `publish-crate` job runs in parallel with
release/docker, so the failure would surface only after the GitHub Release
and image were already out). `cargo package` now verifies cleanly.

KCP alignment pass (2026-09-10, audited against skywind3000/kcp master):
fastack-conserve — the kcp crate 0.6.0 cargo feature was INVERTED vs the C
define (feature ON = the aggressive/unconditional variant), so every
molehill release so far shipped the aggressive variant while its docs
claimed "conserve"; the engine now implements the reference's ts-gated
conserve semantics. Wire-visible only under loss/reordering: the kcp4 loss
cells in the benchmark were measured with the aggressive variant and
should be re-verified on the next full-matrix run (already pending, see
above). Other fixes: the timeout ssthresh now halves the flush-entry cwnd
(`prior_cwnd`) like the reference; `KCP_PROBE_INIT` 7000→5000; stream-mode
`send` reports partial progress instead of erroring after appending
(unreachable in the adapter, which chunks at 64 KiB); `peeksize` no longer
overflows on a hostile `frg = 255`; the dead conv-adoption hook
(`input_conv`) removed (the adapter routes by (addr, conv) via `get_conv`).
Remaining benign deltas: relative `check()` return, `Err(NeedUpdate)` on
pre-first-update flush, bool `nodelay` (the reference's nodelay=2 mode is
unreachable — the adapter uses 1).

KCP optimization pass (2026-09-10, two tuning changes on top of the
absorption, both validated against the committed baseline on the same
host):
- datagram IO batching on Linux (`recvmmsg`/`sendmmsg`, up to 32
  datagrams per syscall — `src/transport/udp_batch.rs`, the second
  audited unsafe site after `src/common/multi_map.rs`): loopback
  1-stream +28-33% (A/B: 2.405 -> 3.191), weak cells flat-to-better;
  the EAGAIN path runs through `UdpSocket::try_io`, which clears
  tokio's cached readiness, so the park-then-drain loops never spin;
  a full kernel send buffer drops the datagram and KCP's ARQ re-emits
  it on the next flush.
- SACK thresholds 50 ms / 32 segments -> 10 ms / 16: the old cooldown
  sat ABOVE the nodelay RTO floor (~30 ms), so the SACK fired only
  after the RTO had already retransmitted — it was inert on loss
  cells. Tuned, the SACK beats the RTO backoff: the loss cells in the
  refresh above improved +11-81% (1-stream and 8-stream).
MTU is NOT a tuning direction (IP-fragmentation risk on real paths):
the `set_mtu` probe (1400 -> 8000 measured +84%..+476% on the weak
cells but -29% loopback 8-stream) was reverted and removed from the
tuning space.

Known waiver (v0.7.2 era): results files before schema v3 measured throughput
by dialing the **backend directly** (bypassing the tunnel), so every tool
reported the loopback iperf3 ceiling (~46 Gbit/s) regardless of tool or cell;
the rtt cells ran via `weakproxy` (client↔server delay only), which the
bypassed throughput never traversed. `results-v0.7.2.json` was refreshed in
place with schema-v3 through-tunnel data; `results-v0.7.0.json` is the
pre-matrix (v1) baseline and is only kept for history. The live gate verdict
is v0.8.0 vs v0.7.2 — 6 metric violations, waivered because the v0.7.2
baseline ran a different container with the single-tunnel default (see the
baseline paragraph above).

## Transport comparison: 4 arms implemented, 3 merged (decision record)

All four arms were implemented, committed and integration-tested end to end
on the `transport-test` branch (kcp_tunnel and quic_tunnel ran their full
lifecycle in the serial suite). The branch is **deleted**; the comparison
history (including the QUIC arm) survives in the local tag
`archive/transport-test`. **Fork decision:** arms 0-2 (N×TCP default, KCP
optional) merged into main as `merge-tcp4`; **arm 3 (QUIC) was left out** —
behind on loss cells (quinn "too many gaps" at rtt10), no peer auth on the
QUIC leg, and N×TCP measured better in every comparable cell. The bench
matrix on main covers mux / mux-off / mux1 / noise / kcp4 only.

| Arm | Topology | Stack | Status |
|-----|----------|-------|--------|
| 0 (baseline) | 1 TCP tunnel | transport `T` (Noise by default) + yamux | main (`[client.data].default_count = 1`) |
| 1 | N TCP tunnels (bench: N=4) | N × (`T` + yamux), round-robin stream placement | main (default `default_count = 4`) |
| 2 | N KCP sessions (bench: N=4) | N × (UDP + KCP + Noise + yamux) — KCP replaces only the plaintext TCP leg | main (optional `default_carrier = "kcp"`) |
| 3 | 1 QUIC connection | quinn (built-in transport crypto, no Noise/yamux); data channels = native QUIC bi-streams | **archived** (`archive/transport-test` tag; not on main) |

Design decisions (merged part):

- Config surface on main (0.8 layout): `[client.data]` holds the client-wide
  defaults (`default_mode` = `multiplex`/`direct`, `default_count` = N
  (default **4**; arms 1/2), `default_carrier` = `"tcp"`|`"kcp"`,
  `default_data_addr` = data-plane endpoint, defaults to the service's control
  endpoint) and each `[client.services.<name>]` can overlay
  `mode`/`count`/`carrier` plus `remote_addr`/`heartbeat_timeout`/
  `retry_interval`/`token` (defaults in `[client.control]` / `[client]`);
  per-service transport: `[client.services.<name>].transport` with
  `type` ("noise" / "plain" / unset = follow `[client.transport].type`)
  and per-service `noise` keys (fallback to the global keys); the client is dual-transport per service
  (`ClientStream`/`ClientTransport` enums, the KCP Noise wrapping uses the
  service's effective keys);
  the server declares no carriers (v3): the registration carries the
  service's carrier, and the server lazily binds its KCP UDP listener on
  the first `kcp` registration (bind failures are precise registration
  rejections); the listener uses `[server.data].bind_addr` (default = the
  control address). The
  yamux window/stream knobs (`mux_receive_window` / `mux_max_streams`) were
  removed from the config surface and fixed at 64 MiB / 32 — yamux couples
  them and independent tuning measured 30x regressions. Server needs no
  arm-1 change: every tunnel hello carries the same session nonce and feeds
  the same pool.
- KCP: crate `kcp` 0.6 (pure-Rust port of the C reference; state machine
  only) behind a thin in-repo tokio adapter (`src/transport/kcp.rs`):
  per-session pump task owning `Kcp` + UDP socket (select: writer channel /
  socket recv / `check()`-driven timer), bounded channels both ways for
  backpressure (`wait_snd()` cap on the write side), server listener
  demultiplexes sessions by `(peer addr, conv)` from `get_conv()`.
  **Fixed parameters (recorded for the comparison):** stream mode,
  `set_nodelay(true, 10ms, fast-resend 2, nc=1)`, snd window 2048 / rcv
  window 4096 segments (~2.8/5.7 MiB in flight at MTU 1400), 32 MiB socket
  buffers (a full-window burst overflows the ~208 KiB kernel default even on
  loopback, and every drop escalates that segment's RTO x1.5), dead_link
  default 20. Noise rides on top of the
  KCP byte stream (`NoiseStream<KcpStream>`) iff the control transport is
  `noise` (same keys/pattern); `carrier = "kcp"` composes with both `plain`
  and `noise`.
  Keepalive: a 2 s adapter-level PING/PONG keeps idle tunnels warm (NAT
  mappings) and doubles as the pacer's RTT/congestion signal; a vanished
  peer is still only confirmed on the next write (dead-link after ~20
  RTOs).
  **v3 (0.8):** every connection starts with a one-byte transport
  selector (`0x00` plain / `0x01` noise) — the server accepts both on one
  listener and `[server.transport]` holds only the Noise keys (no `type`;
  the client decides, informed by the noise-vs-plain bench); KCP sessions
  use the same selector on their byte stream. The `kcp` carrier is
  client-declared in the registration, so `[server.data].carriers` is
  gone and the UDP listener binds lazily on first use (no KCP clients =
  no UDP socket open).
  **Matrix trims (0.8):** peers run the loopback / rtt10 / loss1 cells
  plus the rate cells (reference points), probe durations shortened
  (hol 5 s, steady ping 100, weak cells 10 s), and `just bench-fast`
  gives a ~2-min molehill-only dev smoke matrix into results-dev.json
  (excluded from plot/regression). `just test-fast` runs the lib + core
  integration subset (~1 min vs the ~72 s full suite).
  **New metrics (0.8):** CPU% (per-process utime/stime delta, one-core
  base — the noise/KCP tradeoff rows finally have data), connection
  churn (connects/sec + setup-to-first-byte p50/p99 under 16 concurrent
  short connectors — the mux-vs-direct and pool_size guidance data),
  sustained UDP capacity (paced 20k pps, delivered pps + loss), a
  64-stream scale point, a mixed workload (iperf bulk + interactive
  latency on two services of one client concurrently), and per-rep
  min/max spread on throughput. New cells: rate-limited r100/20 and
  r20/40 (netem rate; fall back to plain weakproxy without a modern
  iproute2) and a jittery j20/10. Two real findings surfaced by the new
  probes: (1) yamux's fixed 32-streams-per-tunnel ceiling caps mux at
  count x 32 concurrent connections — at the default count=4 that is
  128, where the path starts failing (64 is the measured working point);
  **Throttle design (0.8):** the full matrix saturates every core by
  design and once froze the dev host; it now self-throttles — bench.py
  raises its nice value to 10 and every arm waits for loadavg < 0.7 x
  nproc (max 30 s) before starting, which both keeps the host responsive
  and makes each arm start from a quiet machine (cleaner numbers). The
  churn probe was cut to 16 concurrent connectors x 3 s (still a storm;
  ~1/4 the CPU) and the 64-stream scale metric runs a single rep (it is
  a working-point reference, not a statistic).
  (2) an unpaced UDP blast (>100k pps) wedges the shared mux tunnel
  (subsequent TCP metrics fail) while direct mode stays healthy — mux
  shares one sendq across services, so a pathological UDP flood can
  starve TCP on the same client. Neither is gated by bench-check yet.
  **Measured outcomes (optimization round, results-opt-*.json):** window
  doubling bought +29% single-stream on loss1/rtt10 and +20% on
  loss5/rtt100 (loopback flat) at ~2x RSS; a 5 ms flush interval was
  rejected (loopback 8-stream -3x). KCP still loses throughput to TCP
  carriers in every cell (loopback ~2.5 vs ~11 Gbps 1-stream; loss5/rtt100
  ~0.03 vs ~0.28) but wins the latency axis on the worst cell: udp p50
  601 vs 826 ms and hol max gap 1515 vs 1808 ms over the noise-TCP arm.
  Its defensible use case: UDP-only paths (TCP blocked/throttled by
  firewall/NAT) plus latency-first interactive traffic on high-loss,
  high-RTT links.
- QUIC: quinn (ring backend) + rcgen self-signed ephemeral server cert,
  client uses a no-op cert verifier — the tunnel payload is still bound to
  an authenticated control channel via the session-nonce hello, but the
  arm is **experimental: no peer authentication on the QUIC leg**. First
  bi-stream carries `DataChannelTunnelHello`/ack, later bi-streams are data
  channels (`DataChannel::Quic` on the server, one QUIC connection per
  control session).
- Feature `kcp` is additive and in the **default set** so the single
  `target/release/molehill` the bench tooling builds covers the tcp/kcp
  arms (embedded/minimal/container builds keep their explicit slim feature
  lists and are unaffected).
- Merge status: the merged part (arms 0-2 + the bench arms below) is
  delivered on main via the `merge-tcp4` branch (not yet merged into main);
  the QUIC arm and the full comparison history live in the local tag
  `archive/transport-test`. Bench variants on main are `mux` (plain,
  `count = 4`, the default baseline), `mux-off` (plain, `mode = "direct"`,
  loopback-only control), `mux1` (plain, `count = 1`), `noise`
  (`count = 4`) and `kcp4` (noise, `count = 4`, `carrier = "kcp"`). `kcp`
  is in the default feature set so
  the tooling's single `target/release/molehill` covers the merged arms
  (repro_e2e.py documents that default-features expectation).

## References

- rust-yamux: <https://github.com/paritytech/yamux>
- rathole benchmark: <https://github.com/rathole-org/rathole#benchmark>
- frp docs: <https://gofrp.org/en/docs/>
- KCP reference implementation: <https://github.com/skywind3000/kcp>
- quinn: <https://github.com/quinn-rs/quinn>
