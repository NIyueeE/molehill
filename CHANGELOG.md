# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **A transparent (L3) claim's carrier connections come from its carrier's
  budget, and its inner flows are spread across them.**
  `[transparent.data.tcp|kcp].tunnels` is a **lane budget** for a transparent
  client: one lane *is* one carrier connection (a claim never multiplexes),
  divided equally among the claims that draw on that carrier and never below one
  each. A client that writes no budget keeps exactly the old shape — one
  connection per claim — and `tunnels = 4` with two claims gives each of them
  two connections. A budget below the claim count is refused with the number to
  write, and its bound is 1024 rather than the pool's 64, because a lane is a
  connection rather than a stream. The claim's inner flows are **spread across
  its lanes**, one flow per lane, by a hash of the flow's five-tuple — computed
  on the canonical (sorted) endpoint pair, so both directions of a flow ride the
  same lane — which is what stops a claim's throughput from being one
  connection's ceiling; a flow is never split, so nothing is reordered. A lane
  that dies keeps its slot: its replacement inherits the index and the flows
  placed in it resume there, while the surviving lanes are untouched. A flow
  whose lane is gone is dropped rather than moved onto a survivor (moving it
  would reorder it against what the dead lane had already delivered);
  `MOLEHILL_L3_STATS=1` counts those drops on the dead lane's own line, and
  prints one line per lane slot (`live`, `forwarded`, `no_channel`), which is
  how a set is told from a stack. On a jumbo path with eight inner flows, four
  lanes carry 13.9 Gbit/s against one lane's 7.0 while a single flow is
  unchanged; the method, the numbers and the run's own noise floor are in
  [docs/benchmarks.md](docs/benchmarks.md).

- **The KCP carrier can carry one channel with no multiplexer above it.** A
  transparent claim's lane may ride KCP: the client opens one KCP session per
  lane and the server's KCP listener reads the data-channel hello exactly as its
  TCP listener does, so `carrier = "kcp"` costs a claim nothing but the
  carrier's own behaviour. The previous refusal (a KCP session was only ever a
  multiplexed tunnel) is gone, in the client model and in the transparent (L3)
  model alike. What the carrier costs and what it buys is measured, per path
  condition, in
  [docs/benchmarks.md](docs/benchmarks.md#the-carrier-axis-tcp-versus-kcp).

### Changed

- **The data plane's shape is derived from the service type: `mode` is gone.**
  A forwarding service always multiplexes over its pinned pool; a transparent
  claim never does, because its channels *are* its carrier connections.
  `[client.data].default_mode` and `[client.services.<name>].mode` — and their
  `[transparent.data]`/`[transparent.claims.*]` counterparts, which were never
  released — are refused with an upgrade message naming the key, as are
  `[transparent.data].shared_pool` and the transparent
  `members`/`default_members` (a claim's connections come from its lane budget
  now). Why: multiplexing a claim is framing for nothing — on one host and
  workload the same claim on its own connection moved 6 % fewer wire bytes, took
  33 % less CPU per packet and carried 65 % more round trips per second than the
  same claim as one stream of a pool, and one stream on a two-tunnel pool
  carries 9.15 Gbit/s against 18.95 on a connection of its own, at +87 % CPU per
  byte, against that run's own 15.35 % A/A floor
  ([HANDOFF.md](HANDOFF.md), "The mux×1 gap, measured before touching it"). The
  wire protocol is unchanged.

- **The tunnel pool is pinned: `max_tunnels` becomes `tunnels`, established at
  service start.** The client's data plane used to run an *elastic*
  per-carrier pool: it started cold, grew on load, for a stripe group and to a
  UDP service's worker count, and gave tunnels back after an idle timeout. A
  pool's width is now a configuration decision (`[client.data.tcp].tunnels` /
  `[client.data.kcp].tunnels`, default 4, raised to a UDP service's declared
  worker count) and nothing moves it at runtime: all of them are dialed when the
  service activates, none is reaped for idleness, and a tunnel that dies is
  **repaired** back to the count — repair is the one establishment after
  startup, and it is not growth.

  Why: an elastic pool's width is a *result*, and a result depends on the
  history of the process — which makes capacity unpredictable for an operator
  and a run incomparable for a measurement. The measurements that size the new
  key are unchanged and on [docs/benchmarks.md](docs/benchmarks.md#what-each-configuration-choice-costs-per-decision-measurements):
  one tunnel is the worst configuration on every path measured (an L3 claim on
  one carrier connection carries the same for one inner flow and for eight),
  two captures most of the lossy-path gain (8 flows: 3.34 → 5.92 Gbit/s on the
  model's `loss1` cell), and more keeps paying on a clean fast path (12.85 →
  22.81 Gbit/s from one tunnel to eight). Establishment is paid at startup
  rather than by the first visitor, and the cost of a tunnel is 0.5–0.8 MiB
  RSS and 2.6 file descriptors, with no threads.

  What changed for a config: `[client.data.tcp|kcp].max_tunnels` is refused with
  an upgrade message naming `tunnels`; `[client.data].idle_timeout` is refused
  because there is nothing to reap; an explicit `tunnels` below what the
  carrier's UDP services need is refused with the number to write instead. The
  server-side valve `[server.data].max_tunnels_per_client` is unchanged — it now
  bites at a client's *startup* (the extra establishments are refused and
  retried) rather than during growth. `MOLEHILL_POOL_STATS` reports the
  configured `count` beside the live `size`, and the timeline's reasons are
  `+repair` / `-dead`. A pinned pool cannot grow, so the refusal that used to
  trigger growth now reports itself: the first time a visitor is refused
  because every tunnel is at placement's ceiling (56 streams of the engine's
  64), one INFO line names the count and the key to raise, and later refusals
  are DEBUG. The wire protocol is unchanged.

### Fixed

- **A refused KCP datagram is held, not dropped.** The send path discarded
  anything the pacer would not admit or the kernel would not take (a denied
  span, a partial `sendmmsg`, an `EAGAIN`) and let the ARQ re-send it a round
  trip later — so every act of rate control and every full socket buffer became
  packet loss. Refusals are now parked, in order and bounded by half the
  retransmission timeout, and retried: **+12 % throughput on the benchmark's
  lossy leg** (1.824 against 1.627 Gbit/s, disjoint ranges) at an identical wire
  ratio, neutral on the clean and long-haul cells. Pacing the send burst to the
  bandwidth-delay product on top of that is worth another +21 % on the
  long-haul cell but costs 14 % more wire bytes there; it is measured, recorded
  and **not** carried.

- **A KCP retransmission timeout now clears the round trip it is timing.** The
  reference's timeout is `srtt + max(interval, 4·rttvar)`, and this adapter sets
  the flush interval to 10 ms for latency — which left a 200 ms path with a
  **205 ms** timeout, so acknowledgements that arrived at 200 ms plus the
  receiver's own batching were treated as losses: on the model's `rtt100` cell
  (100 ms each way, no loss configured) 21 % of the datagrams never reached the
  receiver and 28 % of what was sent was recovery traffic. A margin of `srtt/8`
  (nothing changes below an 80 ms round trip) cuts RTO-driven resends by 42 %
  and the wire ratio from 1.20 to 1.09, with throughput unchanged. Related
  repairs measured and **not** carried, with their numbers on the benchmarks
  page: pacing the send burst to the bandwidth-delay product (35 % slower) and a
  window byte budget (two thirds of the lossy leg's throughput).

- **A KCP session no longer throttles itself for being busy.** The carrier's
  send pacer, which stands in for congestion control (`nc=1`), took its only
  signal from a keepalive PONG that arrived later than 2.5 s — and a PONG queued
  behind a peer that is busy sending is exactly what a *fast* path looks like.
  The cut is 25 % and the recovery 5 % per four clean PONGs, so one heavy
  transfer ratcheted the rate down four times (12 → 3.8 Gbit/s, measured) and
  left it there for the rest of the session: the next transfer in that session
  read a fraction of the same transfer on a fresh one (**1.00 against 7.79
  Gbit/s** on the same cell, reproducibly). A late PONG now only cuts when the
  send window is *not* moving — a peer that is acknowledging data is working,
  whatever its keepalive looks like — and the slow regimes confirm the fix costs
  nothing there (`rtt100` 0.169 → 0.182 Gbit/s, `loss5` unchanged). The
  `MOLEHILL_KCP_STATS` line also gained the gauges this needed: reader-channel
  and spill residency, the peak send-queue depth, the advertised and peer
  windows, and the pacer's current allowance.

- **A KCP session is no longer declared dead for being slow.** The dead-link
  rule was the reference implementation's — a segment retransmitted twenty times
  ends the session — which cannot tell a peer that is *gone* from one that is
  *slow*. On a rate-limited path the acknowledgements queue behind the shaper,
  so a segment collects its twenty retransmissions while the peer is answering
  everything else, and the session (and the visitor's connection with it) is
  closed: measured on the benchmark's 100 Mbit/s, 20 ms, 1 %-loss leg, the
  multiplexed KCP arm failed its bulk cell in **every** round before this change
  and completes **both** rounds after it. A session now ends only when the count
  is reached *and* the send window has not moved for five seconds — a gone peer
  stops acknowledging, a slow one keeps acknowledging something
  ([docs/benchmarks.md](docs/benchmarks.md#the-carrier-axis-tcp-versus-kcp-2026-10-10-this-model)).

### Changed

- **The KCP carrier's datagram size follows the path.** It was pinned at KCP's
  1400-byte default, and the adaptation that existed was shrink-only: a small
  path lowered the size, a jumbo path never raised it. Since the carrier's cost
  is per *datagram* — a UDP send, a receive, a header, an acknowledgement and a
  loss event — a jumbo path paid that 5.7× more often than it had to. The
  carrier now sizes its segments from the kernel's path-MTU answer, from 1400 up
  to an 8 KiB ceiling, re-read once a second, so a session on a 1500-byte path
  behaves exactly as before (measured neutral) while one on a jumbo path
  measures **+39 % throughput and −28 % CPU per byte** on a clean path, and
  **4.2×** on a lossy one ([docs/benchmarks.md](docs/benchmarks.md#the-carrier-axis-tcp-versus-kcp-2026-10-10-this-model)).
  No configuration changes; the kernel's own answer is the policy.

- **The benchmark model is one runner again: the soak sweep is a profile of
  it.** `benches/scripts/bench/` now owns everything a performance number is
  produced by — the metric registry, the topology, the workloads, the scripted
  conditions and timelines, the reference tools (frp, rathole, nps) as arms,
  the noise floor, the verdicts, the charts and the release gate — and the
  retired sweep's scripts are gone. What that means for a reader of the
  numbers: the staged schedule and the load ramp travel in one results file
  under one schema (`--profile sweep`), a chart is rendered from that file by
  `just bench-plot`, a release run is gated by `just bench-gate` (coverage, the
  endpoint invariant, the SLO on the clean stages, the drift and wedge axes,
  the ramp, and the regression half against a baseline), and the published
  records of the previous model stay in `benches/records/` as history — never a
  regression baseline, because the gate refuses a baseline whose method record
  does not match. The method, the metric definitions and the profiles are on
  [docs/benchmarks.md](docs/benchmarks.md#the-bench-model-the-measurement-standard).
  An arm now states its **carrier** as well as its mode (`l3`, `l3-mux`,
  `l3-kcp`, `l3-mux-kcp`, `l4-kcp`), so the two axes are measured apart, and the
  condition vocabulary gained `loss1_rate100` — a 100 Mbit uplink that also
  loses packets, the lossy WAN the carrier choice is actually about. First
  measurements of that axis: [docs/benchmarks.md](docs/benchmarks.md#the-carrier-axis-tcp-versus-kcp-2026-10-10-this-model).
  Two metrics changed scope after an audit of their instrument:
  `bytes_per_syscall` and `syscalls_per_s` are **device-I/O** readings
  (`/proc/<pid>/io` does not account socket payload — 200 MB through a
  socketpair moves `rchar` by 105 KB, the same bytes through a pipe by 209 MB),
  so only an L3 arm publishes them now and a forwarding arm records a typed
  absence instead of a number that described the runtime's plumbing.

### Added

- **Transparent (L3) clients: the client owns the public `ip:port`.** A
  `[transparent]` block is a run mode of its own, and every entry it has is a
  claim: the client owns a public `ip:port` on a TUN device, the server routes
  whole IP packets into the tunnel, and the client's own kernel answers the
  visitor. Choosing a mode is choosing what the *process* is — its capabilities,
  its device — so the block is a peer of `[server]` and `[client]`
  (`--transparent`, or the block alone), a file carrying two of them is refused,
  and a host that both forwards and claims runs two processes. That is also why
  a claim has no `protocol` key and no home for `local_addr`, `nodelay` or the
  UDP-only keys: they cannot be written, rather than being written and refused,
  and `protocol = "transparent"` inside a `[client]` service is refused with a
  message naming the entry's new home. The model is a front end, not a second
  engine: it lowers into the same `ClientConfig` the forwarding path runs on, so
  the client, the data path and the wire are unchanged. The L3 data path moves
  **batches**: the TUN device is drained and every packet is framed where it is
  read, so a run of frames leaves as one write — the carrier pays one header and
  one acknowledgement per batch instead of per packet, and a batch is flushed
  the moment the device runs dry, so nothing waits for company. Measured on one
  host and workload, that took 12 % off the wire on a bulk flow, and its
  `[transparent.data]` defaults to `mode = "direct"` (a claim has exactly one
  channel) for another 6 % of the wire, a third of the CPU per packet and 65 %
  more small round trips per second than `multiplex`. Both halves of the data
  path batch, reads as well as writes: one socket read carries a run of frames
  and the inject loop walks the run, so a burst costs one read and then a run of
  device writes instead of two awaits per packet (bulk: +27 % throughput, −56 %
  CPU per packet on the arm those were measured on). Numbers, the build they
  came from, and the no-tunnel controls that keep them honest are in
  docs/benchmarks.md, "The transparent-L3 wire question". The backend therefore sees the visitor's real
  source address, TCP keeps its end-to-end semantics, and the server holds no
  socket and no per-flow state for the connection. Both ends attach to an
  **existing** TUN device named by `[client.transparent].tun` /
  `[server.transparent].tun` (default `molehill0`) — the daemon never creates,
  addresses or routes it — and a prerequisite that is missing (the device, an
  address the client does not carry, a non-zero `rp_filter`) is refused with
  the exact command to run instead of silent packet loss. Linux only, with
  `CAP_NET_ADMIN` on the client and on any server that serves it; the `transparent` feature is in the default
  set, and a config that asks for the type on another platform, or in a build
  without the feature, is refused with a precise reason. Serving L3 is the
  **server operator's** decision: the `[server.transparent]` table is that
  switch, and without it a registration is refused by policy before any device
  is looked at — so a client can never be what makes the server reach for
  `/dev/net/tun` or ask for `CAP_NET_ADMIN`, and a server that only forwards
  `tcp`/`udp` needs no capability for this feature at all. `local_addr` and
  `nodelay` are refused for the type — the local application binds the claimed
  address itself, so nothing is dialed — as are the UDP-only keys. The wire
  moves to **protocol v5** (one service type and one data-channel command,
  `StartForwardTransparent`, whose channel carries `[u16 length][packet]`
  frames), so both ends upgrade together as with every protocol change.
  `MOLEHILL_L3_STATS=1` prints the data path's cumulative counters. Such a
  service is **never encrypted**: the visitor's own end-to-end protection is
  the content's and this hop is a plain link by design, so both a per-service
  `transport` and a client-wide `[client.transport].type = "noise"` are refused
  for it. Enabling the server switch therefore means that server accepts
  plaintext connections from such a client, whatever its `[server.transport]`
  says; the Noise keys keep applying to the clients that do negotiate them. Not
  in this
  version: IPv6 (dropped and counted), per-flow channels (every flow of one
  claimed endpoint shares the service's single channel), and any automatic
  network configuration. Keys and the two operator recipes are in
  `docs/configuration.md` ("Transparent (L3) services") and
  `docs/deployment.md` ("Transparent services"); the acceptance harness is
  `just l3-accept` (root-only, Linux-only, outside the check chain), which also
  runs the negative half: a server without `[server.transparent]` refuses a
  claim by policy, with its own device deleted for that run so the ordering is
  proven rather than asserted.

## [0.10.0] - 2026-10-01

### Changed

- **Data-channel striping works, and a group's channels land on distinct
  tunnels by construction.** `[server.data]` `stripe_count > 1` spreads every
  visitor connection over that many data channels (a stripe group) on the
  elastic tunnel pool: the gather asks the client for one channel per stripe —
  a registration opens none itself, so a gather that merely waited for channels
  nobody was told to open timed out — and re-requests only the stripes still
  missing when a budget expires. The gather now **names the group** before its
  channels are opened (`CreateDataChannelForStripe`: service, group, stripe
  index and count), so the
  client knows which opens belong together: it grows the pool to the group's
  own count first (bounded by `max_tunnels`) and reserves one tunnel per stripe,
  falling back to sharing when the pool cannot grow that far — a group that
  cannot spread still works, exactly as before. Until this the K requests were
  indistinguishable from K unrelated visitors, and a cold pool — the elastic
  pool's default state is *zero* tunnels — put the whole group on one tunnel.
  `tests/integration_test.rs::striped_data_channels` now also asserts the
  client's pool reached its four stripes, and
  `tests/session_test.rs::a_striped_gather_names_its_group_on_every_request`
  pins the request vocabulary against a hand-written peer.

- **The tunnel pool can be shared and is elastic.** Three new client settings
  decide what a pool is, how large it may get and how long it lives; the
  shipped defaults keep today's shape. `[client.data].shared_pool` (default `false`)
  serves every service of a control session from **one** tunnel pool per
  carrier instead of one pool per service — the streams name their service on
  the wire already, so nothing else changes, and a session with several
  services pays for one set of tunnels instead of one set per service.
  `[client.data].idle_timeout` (default 60 s) is how long a pool with no
  streams, no pending opens and no pinned UDP peers must stay idle before it
  gives one tunnel back, and `[client.data.tcp|kcp].max_tunnels` (default 4,
  validated `>= 1`) is the cap it may grow to. Growth is the client's own
  decision — an open that would queue, a pool at 12 % of a tunnel's stream
  capacity, or a UDP service whose configured workers need more tunnels —
  and shrink is
  deliberately conservative: a tunnel carrying a live UDP peer is never
  removed, so a stateful UDP session keeps the source port its service sees.
  Two opt-in switches make the policy measurable rather than asserted:
  `MOLEHILL_POOL_STATS=1` prints one line per live pool per second (size,
  per-tunnel streams/pending/pinned, and the reason for each size change) and
  `MOLEHILL_PLACEMENT_STATS=1` aggregates each second's placements and their
  open latency. The server's `MOLEHILL_UDP_STATS=1` line now carries the
  affinity table's size, its evictions and each worker's pinned peers. See
  `docs/internals.md`, "The tunnel pool".


- **BREAKING (configuration)**: the pool has no initial size any more, so the
  keys that described one are gone — **`[client.data].default_count`, a
  service's `count`, a service's `pool_size` and a service's
  `heartbeat_timeout`, plus `[server].max_pool_size`**. A config that still
  carries one is refused before the start, naming every key and its
  replacement. What to write instead
  (the full table is in `docs/configuration.md`, "Migrating to 0.10"):
  per-service `udp_workers` (default 2) replaces the UDP `pool_size`; a TCP
  service's channels are opened on demand, one per visitor; the heartbeat
  timeout is no longer per service because one session carries one timer; and
  the server's valve is `[server.data].max_tunnels_per_client` (default 0 =
  unlimited). UDP-only
  keys on a TCP service (`udp_buffer_size`, `udp_idle_timeout`,
  `udp_send_queue_size`, `udp_forwarder_ipv6`, `udp_workers`) are **errors**
  now instead of being accepted and silently ignored, which is the defect this
  closes. See `docs/configuration.md` for every key's meaning.

- **The tunnel pool starts cold, and the first visitor pays for it.** With no
  initial size to configure, a service's pool is empty until something needs a
  tunnel: that first open grows the pool synchronously, so the first visitor
  after an idle period waits for one tunnel setup before its bytes move —
  2.0-3.2 ms on loopback (the M2a measurement). Nothing else changes: every
  later visitor finds a warm tunnel, the pool still grows on demand up to
  `max_tunnels`, and an idle pool still gives tunnels back after
  `idle_timeout` (never below one). A client with many rarely used services
  pays the setup per service on first use instead of holding a tunnel for each
  one for its whole lifetime.

- **BREAKING (protocol v4)**: the client speaks protocol v4 and the server
  serves v4 only, so **upgrade both ends together** — there is no compatibility
  window in either direction: a v0.9.0 server cannot serve a v0.10.0 client,
  and a v0.10.0 server refuses a v0.9.0 client's hello. Each side refuses the
  other's dialect on the connection it happens on and sends nothing back, the
  side that refused names the version it expected, and the client stops instead
  of retrying into the void. Both directions are pinned against the released
  v0.9.0 binary by `just interop`; the migration note is in
  `docs/configuration.md`, "Upgrading to 0.10 (protocol v4)". What the dialect
  buys: one
  control session per endpoint carries *every* service that dials it, each
  service proving its own credential, so a service the server refuses — a port
  outside `allow_ports`, a token that is not the server's — is rejected on its
  own instead of taking the connection and its siblings down with it, and a
  service whose listener died is re-registered on the same session. The
  heartbeat timeout is no longer a 40 s value the client guesses with: the
  server declares its cadence in the session ack, the client derives
  `max(10 s, 2 × interval + 5 s)` from it, and a configured
  `client.control.default_heartbeat_timeout` below that floor is refused the
  moment the session establishes — with both numbers in the message, and
  without retrying — rather than timing out a healthy server. (It cannot be
  checked earlier: the cadence is the server's to declare.) Existing
  configurations keep working unchanged; leaving that key unset now means
  "derive from the server", and `0` still disables the check. See
  `docs/configuration.md`, "Upgrading to 0.10 (protocol v4)", and
  `docs/internals.md`.

- **The log has a level contract, and a healthy run is quiet.** `ERROR` now
  means a human has to act, `WARN` something the tool handled and is worth one
  line, `INFO` lifecycle, and `DEBUG` one connection's business — so a visitor
  whose local service refused the connection is a `DEBUG` line, not the `WARN`
  it used to be, and a client retrying (started before its server, wrong token)
  is reported once and then at `DEBUG`. Sixteen `Failed to run the data
  channel: early eof` lines from a shutdown that went perfectly are gone; a
  client that starts before its server no longer prints a page of connection
  refusals. `tests/log_budget_test.rs` drives the real binary and fails if a
  healthy run emits a single `WARN` or `ERROR`, or repeats one message shape
  more than three times — the guarantee is measured, not intended. Details and
  the per-level table: `docs/configuration.md`, "What each level means".

- **The benchmark gate now judges what it says it judges.** `just soak-check`
  compares a run against the previous release only when the two are comparable
  (the runs record their host, and a number from another host is context, not a
  baseline — the rule was documented but not enforced); the fd/RSS drift limits
  are applied to the `soak` leak axis they are calibrated for instead of to
  every test type; the interactive error-rate limit compares percentage points
  rather than a ratio, which had turned a 0.03pp wobble into a "+13%" failure;
  and a reference peer's swing is reported with its numbers instead of blocking
  this release. Anyone reading a `soak-check` verdict, or reproducing one, is
  affected; the reasoning each fix rests on is in HANDOFF.md.
- **The gate checks the bulk spine per stage, not per run.** `just soak-check`
  used to ask only that a stage-scheduled run carry *some* throughput samples
  somewhere; a stage whose bulk probe never connected reported "complete"
  because the other stages had plenty. Completeness now counts each stage's
  intervals inside its own window against a floor of one per 30 s (minimum
  one), and reports the recorded reason for a stage that carried none. The
  clean-tree verdict in a results file is fixed in the same pass: the pathspec
  that excludes the run's own output from `git status` never matched, so every
  artifact recorded `tree_clean: false` whatever the tree looked like — a field
  that had quietly stopped meaning anything.
- **The host key is now stable, so the comparison half of the gate can run.**
  Results carried the container hostname as the host identity, which changes on
  every container restart while the hardware does not — so two runs on the same
  machine were *refused* as different hosts, and every stored baseline was from
  a differently-named container. Every run now records `host_id` (machine-id +
  CPU model + core count, hashed) beside the hostname, and the gate compares
  that; files that predate the field fall back to the hostname and the gate
  says which key it used. Per-stage comparisons are also matched by
  **occurrence** rather than by name, so the return-to-`clean` stage — the
  recovery axis — is judged against the baseline's *return* stage instead of
  against its fresh start.

- **BREAKING (protocol): v3 is no longer served.** A v0.10.0 server refuses a
  client that still speaks protocol v3 — the connection it happens on is
  closed with no answer, exactly the way a *newer* client against an older
  server has always been refused — and the server keeps its listener. The v3
  path (one service per control connection, a registration carrying a
  requested channel count) is gone with it, together with the two-key service
  registry that indexed it. Both ends of a molehill deployment are the same
  binary, so a wire break is a fact to announce rather than a state to serve:
  upgrade both ends together, in either order, and the refused connection
  names the version it expected. The interop matrix's new-server/old-client
  case now pins the refusal, and
  `tests/integration_test.rs::a_v3_hello_is_refused_on_its_own_connection`
  pins it on this tree without the old binary.

- **BREAKING (configuration): a removed key no longer starts.** The keys the
  0.10 configuration surface removed — `[client.data].default_count`, a
  service's `count`, `pool_size` and `heartbeat_timeout`,
  `[server].max_pool_size`, a service's `health_check` — were stripped with
  a warning for one release. They are refused now, in one message naming
  every key found and what to write instead, because a key that secretes a
  compatibility path for one release is a key that never gets removed. The
  upgrade instruction in `docs/configuration.md`, "Migrating to 0.10" is the
  - **The Soak method was re-derived: the path is shaped once, the cells say how
  they were measured, and the release artifact carries the load axis.** Four
  changes to what a published benchmark number means, each with its A/B in
  HANDOFF.md ("Shaping scope, the rate cells, and the shaped-cell rule"):

  *The stage classes apply to the visitor's leg only.* Until now one HTB class
  shaped both the visitor leg and the tool's backend leg, so every injected
  delay was paid twice (a `rtt100` stage's interactive floor read 802 ms) and a
  `rate100` class carried 100 Mbit *in total* — around 42 % of nominal end to
  end on every arm. Measured on one binary, one method, only the scope changed:
  floors halve (802 -> 401 ms, 162 -> 82 ms), the rate classes read 91-100 % of
  nominal, the worst stage transition drops from 25-28 s to 7 s, and a saturated
  20 Mbit link's real queueing becomes visible (p99 7.4-8.3 s against the 2.0-3.8
  s the old scope hid by halving the offered load). `SOAK_SHAPE_LEGS=both`
  reproduces the old scope; the value used is in `meta.shape_legs` and in the
  gate's method keys, so runs across the change are refused, not compared.

  *A stage's bulk cell is the load over its whole window, and the cell names the
  side that measured it.* The peak interval was a property of the shaper's
  schedule; the reading is now span-weighted over the stage, and on a rate class
  — where the client's socket buffer defeats the sender's accounting by
  construction — it is the receiver's own window, marked `*` in the tables. A
  rate stage whose dial produced no receiver summary carries no reading and says
  why (`rate20`, whose client is still blocked 30 s past the boundary, is such a
  cell) instead of publishing the defeated side's zero.

  *A shaped stage's interactive cell is reported as context, not judged.* Three
  runs of one unchanged method move a shaped p99 cell by 5-24 % against the 25 %
  limit the gate applies per stage, so the gate now fails only a blow-up (3x)
  there and the README marks those columns and picks no winner in them.

  *The host key is a measurement as well as a name.* Every run records
  `host_calibration` (SHA-256 over a fixed 192 MiB buffer, median of three,
  1-2 % repeatable), and the gate refuses to compare two runs whose calibrations
  differ by more than 25 % — closing the hole that on a host without
  `/etc/machine-id` the identity key is only `cpu_model | nproc`.

- **The rate-shaped cells are measured with a bounded sender.** A rate shaper
  made the bulk client's socket buffer absorb the whole stage: its measured
  intervals then read zero bytes while the path kept carrying them, `rate20`
  never produced a summary at all, and the number it left was not merely
  unreadable but wrong — 0.0334 Gbit/s on a 20 Mbit path, 70 % above nominal,
  because the transfer outlived the stage it was measured in. The client's
  window on a **rate** class is now bounded (`SOAK_RATE_SOCKET_WINDOW`, default
  `256K`; `off` reproduces a run measured before it): the zero-byte share falls
  from 72-79 % to 13-14 %, `rate20` reads 0.0196 (98 % of nominal) and
  `rate100` 0.1000 against 0.0935-0.1081. The reading rule follows the
  measurement rather than the class, so a cell whose sender can speak is read
  from the sender again, and the receiver's window is left for the cells that
  still need it. `just soak-check --screen` also reports one verdict per metric
  now (throughput *and* interactive p99) instead of choosing one for the run —
  which is how the two A/Bs below were read.

- **The pool-size and shared-pool questions are answered.** Measured
  interleaved, one binary, matched load: raising `max_tunnels` from 4 to 8 does
  not help a mixed workload (at 15 or more streams the shipped cap wins all six
  steps on interactive p99, by 42-61 %), and a **shared** pool
  (`[client.data].shared_pool = true`) costs interactive latency against the
  shipped per-service default at nineteen of twenty steps (median p99 8.15 ms
  against 5.00) while the two are indistinguishable on throughput. The defaults
  do not change; the numbers behind them are in HANDOFF.md.

- **UDP's capacity ceiling is documented and measured.** A UDP service's
  datagram throughput is bounded **per pool**, not per worker: measured on one
  host, `udp_workers` at 1, 2 and 4 carried 1.14, 1.00 and 0.98 Gbit/s of
  1400-byte datagrams, unchanged by 16 or 64 visitors, and datagrams beyond it
  are dropped (the design's deliberate choice over head-of-line blocking other
  visitors, counted under `MOLEHILL_UDP_STATS` as `queue_full`). The
  configuration page says so beside the knob, and the measurement also settles
  the milestone row that asked for a shortest-queue assignment rule: there is no
  imbalance to correct (visitors spread evenly in every configuration, and the
  drop equals the excess over the ceiling to within 0.07 %).

- **A configuration that cannot serve is refused out loud.** The instance was
  spawned and its result only looked at when a later configuration change
  arrived, so a failure at startup left the process alive, silent and serving
  nothing — measured with the control port already held by another process:
  four `INFO` lines, "Running as a server" among them, an empty log afterwards
  and no listener. The failure now ends the process with the cause and a
  non-zero exit (`the instance stopped: Failed to listen at
  \`server.control.bind_addr\`: Address already in use`). A failure that arrives
  with a reload is reported the same way.

- **The comparability key measures the path, not just the CPU.** Every run
  records two tool-free calibrations now: the existing CPU workload (state) and
  a loopback-path probe (512 MiB through one socket pair, median of five,
  pinned to one CPU *in a child process of its own*, Gbit/s). They answer different questions — two container
  instances of one `host_id` measured 25-39 % apart on the clean cells of the
  two arms that reach the loopback ceiling while the CPU probe read 1.7 % apart
  — so a comparison is refused when either moves (the path within 15 %), and a
  file that predates a probe is reported as unverifiable for it rather than read
  as agreement. Unpinned, the probe read *bimodally* across processes (29.4-29.7
  against 34.2-34.4 Gbit/s on an idle host), which no tolerance can carry;
  pinned it repeats to ~1.5 % and drops ~5 % under four busy loops.

- **The release sweep publishes the load axis too.** `--test` takes a comma
  list and the ritual runs `--test=rrul,capacity`, so
  `results-soak-vX.Y.Z.json` carries "how many bulk streams it sustains before
  the SLO breaks" beside the staged schedule, measured on the same host at the
  same revision. The two are separate instruments and are never cross-checked.
  See `docs/release.md`, "Benchmarks", and `docs/benchmarks.md`, "Test types".

### Removed

- **`health_check` (the per-service health probe) is gone, and a service is no
  longer withdrawn from the server when its local backend goes down.** This is
  the transparent-visibility model: registration is the only thing that decides
  whether a service is visible, so a dead backend is a failed request for the
  one visitor who asked (connection closed or reset, like any reverse proxy in
  front of a dead upstream) instead of a service that silently disappears for
  everyone. The old behaviour could not tell "the backend is down" from "the
  process that accepts connections is up but broken", and its deregister/
  re-register cycle was itself a source of control-plane churn; the cause of a
  failed request now goes to the client's log, where an operator can see it.
  Operationally nothing needs to be done to recover a backend: it is enough to
  start it, and the service that was never deregistered forwards again. A config
  that still carries `health_check` is refused, in the one message that names
  every removed key and what to write instead (see "BREAKING
  (configuration)" above). See `docs/configuration.md`, "A local service
  that is down".

### Fixed

- **One visitor's unanswerable channel request no longer parks the whole
  service.** The server's accept loop paired visitors one at a time, so a
  request the client could not answer — a pool at its placement ceiling, which
  the client reports to nobody — held the accept loop for the visitor's whole
  25-second budget, every visitor behind it queued unanswered, and the k-th
  one was shed a full budget after the first. Pairing is per visitor now, with
  the number of pairings in flight bounded (128): a shed visitor costs that
  visitor and the service keeps accepting and forwarding. A stripe group's
  gather stays atomic (all-or-none under a lock), and a data-channel open the
  pool refuses is reported once per process on the client — INFO the first
  time, then DEBUG — so an out-of-capacity pool is visible instead of a stream
  of indistinguishable per-connection failures. Pinned by
  `one_unanswerable_visitor_does_not_park_the_service`, which fails with the
  serial loop (the second visitor shed 49 s after the first); it also caught a
  latent test defect — a read whose result was never checked, so a shed
  visitor's closed socket read as four zero bytes of "garbage".

- **A burst of new streams no longer stacks on one tunnel.** Growth used to be
  sampled only on the pool's 50 ms maintenance tick, so a burst that opened its
  streams back to back — a 20-stream `iperf3` bulk test — finished placing every
  one of them on the same tunnel before the first tick could see it, and that
  tunnel's interactive and control streams then queued behind the bulk. An open
  whose chosen tunnel is already at the growth threshold now grows first and
  places second, so the burst spreads over the tunnels it will use. The in-path
  growth skips itself when growing is wrong (a growth in flight, a refused
  growth holding the pool back, the pool at `max_tunnels`, a cold pool), and the
  maintenance tick keeps its role. Pinned by a test that fails with the whole
  burst on one tunnel when the in-path growth is removed.

- **A KCP data channel on an IPv6 path no longer fragments either.** The
  path-MTU clamp shipped in v0.9.0 read the kernel's MTU through `IP_MTU`, which
  answers for IPv4 only, so an IPv6 KCP session kept its 1400-byte datagram and
  the kernel split it on any smaller path — the same failure the v4 fix removed,
  where one lost fragment costs the whole datagram. The probe now asks
  `IPV6_MTU` for an IPv6 peer too (declared in-crate on top of `nix`'s socket
  option macros, so the crate still adds no `unsafe`), and the session shrinks to
  the path minus the 40-byte IPv6 header and the UDP header. The arithmetic and
  the shrink-only contract are unchanged.

- **A graceful server shutdown now ends the connections it was serving.** The
  listeners stopped, but a live control session — and every service listener it
  had bound, and every multiplex tunnel it owned — kept running until the
  process exited; the lazily-bound KCP listener was not stopped either, so its
  UDP port stayed bound for the life of the process. On a restart the old
  session could therefore keep answering on the ports the new server was about
  to bind (most visibly in a deployment that restarts in place), a restarted
  server could not bind a KCP port its predecessor still held, and a public
  port stayed held for as long as the shutdown took. Dropping the registrations
  ends each session, its services and its tunnels together, and the KCP
  listener is signalled with the same shutdown; the client sees the closed
  control channel and reconnects, which is what a restart is supposed to look
  like from its side.

- **A client that reconnects starts with fresh tunnel pools.** The pools used
  to survive a reconnect, but their tunnels carried the *previous* session's
  nonce: the new server refuses them as stale, so every open on a reused pool
  failed and the pool's own growth was refused too — forwarding stayed down
  until something else rebuilt the session. The pools are dropped with the
  connection now (the reconnect path re-registers and re-activates every
  service, each of which builds the pool it needs).

- **One stream past a tunnel's cap no longer takes the tunnel down.** A
  multiplex tunnel carries at most 64 concurrent streams, and the 65th used to
  be answered with a protocol error that terminated the whole connection: a
  burst that crossed the cap destroyed every service sharing that tunnel —
  every visitor on it failed at once — and the pool rebuilt from nothing. The
  one stream is refused now (the engine queues a reset for it, at `ERROR` once
  and then quietly) while the tunnel keeps serving the streams it already had.
  The refusal is a failure for the visitor that arrived at the wrong moment and
  nothing for anyone else. The placement ceiling — 56 of those 64 streams —
  keeps the case rare; this is what makes it survivable when it happens.

- **A service can no longer be parked for good by one unanswerable request.**
  The visitor accept loop pairs one connection at a time: it takes a visitor,
  asks the client for a data channel, waits, forwards, and only then accepts
  the next one. That wait had no bound, and a request the client cannot answer
  never comes back at all — a pool at its placement ceiling refuses the open
  and reports the refusal to nobody. One such visitor therefore stopped the
  service from accepting anything, permanently, even after every stream had
  been released and the pool was empty. It is reproducible without a
  benchmark: saturate a pool, drain it completely, then connect — the fresh
  visitor hung. The wait is a budget now (5 s) that asks again on expiry,
  because capacity usually comes back and at most one visitor is waiting, and
  only a visitor the client refuses five times in a row is dropped.

- **The pool grows before it has to refuse.** Growth needed 80 % of a tunnel's
  stream capacity — 51 of 64 — which a workload has to be shaped deliberately
  to reach: the benchmark's own peak is 20-21 concurrent streams per service,
  so the pool stayed at a single tunnel and everything queued behind it. It
  grows at 12 % instead, once a tunnel carries seven streams, which is where
  head-of-line blocking starts to show in the interactive stream.

- **A forwarded connection that moves nothing in either direction is closed.**
  A visitor whose stream wedged — the path recovered, the connection never did
  — held that stream for the life of the session, because nothing released it,
  and the pool had one less place to put a working visitor. A forward that
  moves no bytes either way for 300 s is closed now, which releases the stream.

- **The multiplex receive window is 32 MiB, not 64.** At 64 MiB a saturating
  bulk transfer starved the engine's own window accounting, so `iperf3` ended
  a completed transfer with `error - idle timeout for receiving data` and exit
  1 — a finished measurement reported as a failed one. The boundary is
  measured: 8, 16 and 32 MiB all finish with exit 0 on the same path, 64 MiB
  does not, and the smaller window costs no throughput.

- **A stalled tunnel writer is woken again.** A stream's reader and writer
  both park on the connection's per-stream command channel, and both stored
  their waker in the same slot. A reader that parked last — queueing a window
  update, which is what returns send credit to the other side — erased the
  writer's waker, so the credit that came back afterwards woke nobody: the
  stream's send direction slept until some unrelated resize happened to
  notify, and every visitor on that tunnel stalled for the rest of the
  session. The reader now parks in its own slot and the connection wakes both
  when a command leaves the channel (`src/mux/connection.rs`,
  `a_readers_channel_park_keeps_the_writers_waker` pins the slot discipline;
  the stripe livelock it produced is recorded in HANDOFF.md).

- **A tunnel whose connection died leaves the client's pool.** The pool had
  exactly one removal path — the idle shrink — and it requires the *whole*
  pool to be quiet, so a single stream that outlived its connection kept the
  dead tunnel, and its slot against `max_tunnels`, placeable for the rest of
  the session: every later open that landed on it failed with `Closed` while
  the pool kept reporting capacity. The pool now reaps a tunnel whose driver
  has ended on the next maintenance tick, without any idle/warm/cooldown gate,
  and dials a replacement when the dead tunnel was carrying something
  (`ShrinkReason::Dead`; `a_dead_tunnel_is_reaped_and_replaced` fails without
  the reap).
- **A window update or a stream close no longer queues behind bulk data.** A
  tunnel's frames all left through one FIFO, so the bodyless bookkeeping that
  the peer needs to make progress — the credit a window update grants, the FIN
  that ends a stream — sat behind however much payload the other streams had
  queued. Those frames now leave through a priority queue ahead of any data
  frame, the receiver scan round-robins instead of always serving the
  lowest-numbered ready stream, and the queue is bounded in bytes so one
  tunnel cannot hold an unbounded amount of payload ahead of a socket that is
  not draining (`src/mux/connection.rs`; the ordering, the FIFO discipline of
  payload and the bound are unit-tested).
- **A visitor whose data channel dies before the forward command is
  re-paired, not dropped.** The client dials the local service only *after* it
  receives `StartForwardTcp` on the channel, so a channel whose backend leg is
  already gone fails in exactly that window — and the server used to drop the
  visitor on the spot, closing its socket. Measured on the
  `rate100:120,rate20:120` reproducer: an iperf3 control connection paired with
  such a channel died as `control socket has closed unexpectedly`, and because
  every later dial of the stage inherited the failure the bulk spine recorded
  nothing at all. The visitor now asks for another channel, under the same
  `PAIR_ATTEMPTS` allowance a *missing* channel gets
  (`src/core/server.rs::serve_tcp_visitor`).
- **The Soak schedule drains a stage before reshaping the next one.** The
  method's `rate20` cell published "spine produced no intervals" for every
  tool, and the cause was in the harness: a stage boundary killed the bulk
  client and immediately changed the qdisc, so the old stage's kernel-side
  drain and FIN retransmissions shared the new, slower queue with the next
  stage's handshake. Measured with no tool in the path at all, a fresh connect
  timed out after 10.5 s and the next round trip took 3.5-6.7 s. The harness
  now waits at the *old* shaper until the netem queue is empty and the
  throughput port has no established connection (bounded by
  `SOAK_DRAIN_BUDGET`; both halves of that predicate were later widened, see
  the two bullets below), restarts the single-test `iperf3` backend
  before every stage's bulk attempt, and records the client's own failure text
  instead of a bare exit code. See `docs/benchmarks.md`, "The stage schedule".

- **The Soak drain reads the queue it claims to drain.** That wait had a hole
  in it: `tc` renders a `backlog` with a unit suffix (`b`, `Kb`, `Mb`, `Gb`)
  and the drain's pattern accepted only the bare `b`, so any queue large enough
  to print as `Kb` — which is every backlog at a rate-shaped transition — was
  read as *no qdisc at all*, and the drain returned without waiting for
  anything. It was a silent no-op at exactly the transitions whose queued bulk
  it exists to absorb, which is where every empty bulk cell of this cycle's
  sweeps was (`molehill`'s `rate20`, `nps`'s `jitter`): reproduced
  deterministically on the `rate100:20,rate20:20` transition, 3 of 3 runs dead
  before the change and 3 of 3 carrying their spine after it, with the suffix
  scale (KiB) checked against `tc -s -j` on the same instant. The drain and the
  transition now also record what they left behind, per stage
  (`drain_s`, `drain_expired`, `drain_final_backlog`, `drain_busy_sockets`,
  `spine_sockets`, `backend_restart`, `spine_attempts`,
  `spine_first_interval_s`), so a stage that starts on a busy path says so
  rather than leaving it to be guessed. No schedule, port set, load fraction or
  SLO changed. See `docs/benchmarks.md`, "The stage schedule".

- **A stage transition ends when the path is quiet, not when a timer runs
  out, and the bulk spine is redialed if it has to be.** Three things were
  wrong with the wait between stages, and all three are fixed together because
  they are one behaviour:

  *The wait could not be satisfied.* Its predicate was "the netem queue is
  exactly empty **and** the throughput port has no established connection", and
  neither half was decidable: the interactive, churn and UDP probes share the
  tool's class and leave ~1.2 KB queued permanently (measured with a 400 s
  budget — the queue settled at 1.2 KB and never reached zero), and
  `established` alone reads **0** from ~t+20 s while the killed client's
  `FIN-WAIT-1` sockets are still retransmitting megabytes. So the wait could
  only ever end by expiring, which made every transition timer-driven: whether
  the next stage's dial survived depended on whether the clock happened to
  allow enough time. It now waits for a queue *tolerance* (no more than one
  `lo` frame, 64 KiB) **and** for no socket in a state that can still send —
  `ESTAB`, `FIN-WAIT-1`, `CLOSE-WAIT`, `SYN-SENT`, `SYN-RECV`, and not the
  teardown states, which linger for minutes carrying nothing.

  *`SOAK_DRAIN_BUDGET` is a safety net, not the mechanism*, sized above the
  measured worst case (180 s against ~159 s: that is how long a `rate20`
  stage's killed 20-stream client takes to retransmit what its kernel holds at
  that stage's own 20 Mbit/s). A net that fires is recorded per stage, and both
  the tolerance and the state set travel in `meta`.

  *The spine is dialed at `SOAK_SPINE_RETRY_S` seconds into the stage* (default
  `0,25,50,80`) and stops at the first dial that carries intervals; a dial that
  has carried nothing is abandoned at the next offset, so a stuck one cannot
  eat the retry it exists to leave room for. A healthy stage never notices —
  its first dial runs the stage out — and a recovered stage is visible rather
  than silent: `spine_attempts` counts the dials and `spine_first_interval_s`
  records when bulk started.

  The result is a measurement whose start state is controlled rather than
  fitted: in the release sweep every one of the 32 tool-stages dialed on its
  first attempt, and the transitions cost less than the fixed budget they
  replaced (197 s per tool in total, against 840 s of budget expiry). See
  `docs/benchmarks.md`, "The stage schedule".

- **`just soak-check` compares the method, and refuses a cell the method
  cannot measure.** Two things the release gate stated too weakly:

  *Comparability was checked as one integer.* `workload_version` is a single
  number for the whole model, and this cycle changed five method keys while it
  stayed `1` — the stage transition, the drain's predicate and budget, the
  spine's retry schedule — so the gate would have called two different
  instruments comparable and printed verdicts from the comparison. It now
  compares the runs' method records (`soak_check.METHOD_KEYS`: the schedule,
  the shaper classes, the load, the SLO, the probe rates and the transition
  settings) and refuses, naming every key that differs **and** every key one
  file does not record at all — an absent key is an instrument that file cannot
  describe, not a default to be assumed. Every blocking reason is reported, not
  just the first, because a baseline can fail more than one and naming only one
  would suggest that clearing it makes the pair comparable.

  *A degenerate cell was still published as a number.* At or over half of a
  stage's bulk intervals reading zero bytes — which is every rate-shaped stage,
  because netem holds each interval's bytes past that interval's own accounting
  window — the peak that remains is not a throughput measurement, and both the
  plot's tables and the README now print that share instead of a figure. The
  per-stage bulk table is generated by `just soak-plot` for the first time, so
  the README's bulk numbers and the tool cannot drift apart.

  *And the run now states its own noise.* The schedule measures `clean` at both
  ends of every timeline, so every run contains a replicate of one condition
  about an hour apart. `just soak-check` reports that spread per tool — 18.83 to
  20.48 Gbit/s for molehill, 0.4 % for frp — and it is the scale a between-tool
  difference has to clear. It is reported, never judged: variance is data, and
  a threshold on it would be invented. Applied to this release's numbers, the
  clean path reads molehill 18.8-20.5 Gbit/s against rathole's 17.6-18.0:
  ranges that do not overlap, but a gap inside molehill's own replicate spread,
  so this run does not separate them either; frp is 3.1-3.4x behind, and
  molehill's clean latency (7.6-8.4 ms) is an order of magnitude better than
  rathole's (77.4-77.7 ms). See `docs/benchmarks.md`, "Comparability".

- **A `client`-only build compiles again, and so does `client,kcp`.** Two
  `#[cfg]` gates were left behind when the v3 path was deleted, both by
  removing the line *under* an attribute and leaving the attribute to attach
  itself to the next item: `read_register_result` became gated on `server` as
  well as `client`, so a `client`-only build could not find the reader its own
  session loop calls, and `common::owned_write` — which the noise and the KCP
  transports both use on either side — became `server`-gated, so `client,kcp`
  could not compile. CI's feature-powerset job and the `minimal` profile build
  have been red since the v4-only commit for exactly these two; `just powerset`
  (all 251 combinations) and `cargo build --profile minimal
  --no-default-features --features client` both pass now.

- **A datagram over `udp_buffer_size` no longer ends the UDP service on
  Windows.** The contract is that such a datagram arrives truncated to the
  limit, and POSIX does that in the kernel and reports the buffer's length;
  Windows fills the same buffer with the same prefix but reports
  `WSAEMSGSIZE`, and that read error was taken for a dead socket — so one
  oversized datagram ended the service's whole UDP pool, told the client the
  service was no longer exposed, and, on the way back, broke the forwarder that
  read the local service's oversized reply. The server now reads a
  whole-datagram buffer and applies `udp_buffer_size` to what it read, because
  a failed `recv_from` is also where the visitor's address is lost and the
  datagram could then be neither truncated nor routed; the client's connected
  socket reads the error as the full buffer it stands for.
  `udp_buffer_size_bounds_a_datagram_without_breaking_the_channel` pins both
  halves — it is the branch's Windows build job that caught this.

## [0.9.0] - 2026-09-25

> The published benchmark numbers were measured on `710186c`. This release
> differs from it by the platform-build fix CI forced, which is a no-op on the
> measured platform; the reasoning is in HANDOFF.md, "Provenance of the
> published v0.9.0 numbers".

### Changed

- **The benchmark model was replaced: the measurement matrix is retired and
  the Soak model ships in its place** (`benches/scripts/soak/`). A *cell* —
  one average per tool per network condition, cold-started per cell and
  reported as a median over reps — is replaced by a *workload over time*:
  every tool is driven through one identical workload (an interactive
  stream, N bulk streams, C short connections per second, a UDP session)
  while the path follows a scripted stage schedule that is changed in place,
  so the tool's session is never rebuilt and adaptation/recovery is part of
  the measurement. Test types: `capacity` (ramp the load until the
  interactive stream breaks the SLO), `rrul` (saturate and watch the
  interactive stream's RTT distribution over time), `soak` (the drift/leak
  axis), `cost` (CPU-seconds per carried Gbit at a fixed operating point)
  and `screen` (a fast development A/B, the two builds interleaved inside
  every load step with a sequential decision). Everything measured is
  externally observable, so the peers (frp, rathole, nps) are driven by the
  same workload and charted in the same panels; tools in a batch never share
  a shaper (one HTB class plus independent netem each) and the interactive
  probes run in their own processes so the harness is never inside the
  measured path. The retired matrix's runner, charts, gate and results
  (`benches/scripts/bench/`, `assets/benchmark-*.png`,
  `results-v*.json`) are removed; its numbers remain in git history and in
  the v0.8.x release notes and are never a regression signal against this
  model. Recipes: `just soak` / `soak-plot` / `soak-check` / `soak-peers`;
  the release review now requires `results-soak-vX.Y.Z.json` and
  `assets/soak-vX.Y.Z.png`.

> **Measurement note.** Several A/B figures quoted below were first taken
> with the benchmark harness's broken `--ab` mode, which spawned the default
> binary on both sides of an interleave and therefore compared one binary
> against itself (fixed in `064c55a`; post-mortem in HANDOFF.md, "The `--ab`
> harness bug"). Those figures are withdrawn; the ones that stand were
> re-taken with the fixed harness and say so in their entry. A/Bs taken as
> two separate runs (the noise-stream pairs, the KCP experiments) and the
> in-process probes (µs, allocations, faults) were never affected. The
> branch-vs-`main` cumulative comparison and the v0.9.0 release matrix were
> run entirely with the fixed harness.

### Fixed

- **The KCP carrier builds on macOS and Windows again.** Its send batching
  (a reusable staging buffer plus one span per datagram) lived entirely inside
  the Linux-only `recvmmsg`/`sendmmsg` module, while the batch *shape* is what
  both the Linux and the non-Linux send arms consume — so the non-Linux arm
  referred to types that did not exist there and four CI targets failed to
  compile. The portable half now lives in `transport::dgram`, the Linux module
  keeps only the syscall machinery, and the non-Linux path (reassemble a split
  datagram, one `send_to` each) is what it always was.

- **A KCP data channel no longer fragments on a path smaller than its
  datagram.** UDP does not negotiate a path MTU: Linux fragments an oversized
  datagram by default, so on a 1280-byte path every 1400-byte KCP datagram
  became two fragments and one lost fragment cost the whole datagram — enough
  to take the KCP carrier from 0.37 Gbit/s to **zero** on a 1 %-loss path while
  the TCP arms were unaffected. Each session now reads the kernel's path MTU
  and shrinks its datagram to fit (IPv4; shrink-only; re-checked once a second
  because a session outlives the path it started on). The TCP carriers already
  had this from the kernel.

### Added

- **The benchmark can measure cold start** (`--test=reconnect`): how long from
  a client start until every registered service answers, per service, five
  repetitions per build, interleaved when two builds are compared. It is the
  first instrument for a cost every other probe is blind to — they all dial a
  running tool — and it reports ~154 ms on a clean loopback path on this host.

- **The UDP visitor path has drop counters** (`MOLEHILL_UDP_STATS=1`): the
  server's datagram reader distinguishes a full worker queue (the loss the
  design accepts instead of head-of-line blocking every visitor) from "no data
  channel was ready yet" (the registration/reconnect window), counts each, and
  logs both once a second under the same opt-in convention as the KCP and mux
  counters. Routing now returns its outcome instead of only counting, so the
  decision is testable without racing on process-global statics.

- **Noise session resume** (`[transport.noise] resume = true`, default
  off): a reconnect proves possession of the previous session's
  handshake hash with a MAC instead of repeating the handshake's key
  exchanges. The server issues a ticket (sealed with a key derived from
  its Noise static private key) after a full handshake; the client caches
  it and, on the next connect, sends it with a fresh nonce and MAC
  (transport selector `0x02`). Both sides then derive fresh record keys
  from the cached hash plus two nonces (HKDF-SHA256 over
  ChaCha20-Poly1305). Tickets expire after 24 h, a repeated
  `(ticket, nonce)` pair is rejected, and any failure declines cleanly so
  the client falls back to a full handshake. Measured on this host
  (release build, in-process pair over a duplex, the default pattern):
  **442.7 -> 38.5 us per connection pair** — the handshake's key
  exchanges are ~97% of the setup CPU and resume removes them. The
  tradeoff is forward secrecy on resumed sessions (their keys derive
  without a fresh DH), which is why it is opt-in. See docs/transport.md,
  "Noise session resume".
- **Data-channel striping** (`[server.data] stripe_count = K`): a visitor
  connection can be spread over `K` parallel data channels — a *stripe
  group* — instead of one. Each direction numbers its 32 KiB chunks and
  spreads them round-robin over the group; the receiver reassembles by
  sequence number, so the connection behaves like one stream whose ceiling
  and in-flight window are the sum of its channels'. The framing lives on
  the data channel (`StartForwardStripedTcp` command, `[seq][len]` frames,
  `src/stripe.rs`), not on yamux, so the engine's wire format — and 0.8.x
  peer interoperability — is untouched, and channels that do not carry the
  new command are byte-identical to before. Default `1` (off); TCP services
  only. An interleaved A/B against the parent revision with the fixed
  harness (loopback cell, 3 rounds, 8 s tests, both binaries' commit SHAs
  verified) measures **+48.7%** on 1-stream with non-overlapping rep ranges
  (10.73 -> 15.96 Gbit/s), 8-stream inside its spread (+5.1%), and a
  median-only cost side (churn -7.7%, CPU +40.8%, RSS +8.7%, sub-ms
  latency +5%); cpu-per-frame halves. Details and the inertness check at
  K=1: HANDOFF.md, "Stripe A/B (K=4)". See docs/internals.md,
  "Data-channel striping".
- The KCP data path (carrier `kcp`, `kcp` feature) carries opt-in
  attribution counters — the same instrument the mux engine's framing
  counters are for that engine. With `MOLEHILL_KCP_STATS=1` every
  molehill process logs a per-second `kcp-stats` line: datagrams
  in/out, retransmissions, acks and SACK gap notifications sent, pump
  rounds, and coarse per-phase milliseconds split across input, delivery,
  writer drain, wire drain and the ARQ update. Relaxed atomics at the
  existing sites; no wire-format or behaviour effect, default off.

### Changed

- The multiplexing engine (`multiplex` feature) is now maintained in-repo:
  rust-yamux 0.14 was vendored into `src/mux/` — wire-identical with the
  yamux specification, so 0.8.x peers keep interoperating — and the
  `yamux` crate dependency is gone. Vendoring is a move, not a rewrite;
  the deviations are mechanical (logging through `tracing`, `std`
  instead of `web-time`/`static_assertions`, upstream property tests
  dropped, the unused graceful-close subsystem removed). It puts the
  framing path under molehill's own rules and unlocks changes that
  call-site tuning cannot reach. The per-change record of the migration
  (what landed, what was measured and closed) is HANDOFF.md, "What
  landed".
- The mux engine is now tokio-native: it speaks tokio's `AsyncRead` /
  `AsyncWrite` directly instead of the futures-io traits behind a
  tokio-util `Compat` shim, so the `futures` and `tokio-util`
  dependencies are gone. The per-stream command channel is unbounded —
  the yamux send window is the real backpressure (a frame is only queued
  after its bytes consumed window credit), so the queue stays bounded by
  the credit the peer granted. No wire change.
- `poll_read` on a mux stream now delivers buffered bytes before
  attempting the window update (the update needs command-channel
  capacity, the delivery does not), and the connection's poll loop
  drains its queued frames within one poll instead of sleeping with
  frames still queued. Both close deadlock windows the futures-based
  engine did not have: a reader parked on a full command channel while
  holding buffered data starved the peer's sender of credit, and an
  idle writer plus drained receivers plus a quiet socket left the
  connection with no waker at all — the queued frames (window updates
  included) never went out until an unrelated timeout broke the cycle.
  The command channel stays bounded at 10 frames: the depth is pacing,
  not backpressure — an unbounded queue lets writers run whole windows
  ahead, the receiver's buffer grows with them, and the window update
  shrinks with the buffer, throttling the very sender it is meant to
  supply.
- `MultiMap` — the two-key map behind the server's control-channel
  registry — is now plain safe Rust: the second key is stored in both maps
  instead of sharing one heap item between them through raw pointers.
- The per-tunnel mux stream cap is raised from 32 to 64
  (`DEFAULT_MUX_MAX_STREAMS`), doubling the per-client concurrent
  data-channel ceiling at the default `count = 4` (128 -> 256). The
  yamux credit reservation grows from 8 MiB to 16 MiB of the 64 MiB
  connection receive window, leaving 48 MiB (75%) for the window
  auto-tuner; the pairing is guarded by a unit test that fails if the
  reservation ever swallows half the window (the configuration that
  measured a ~30x throughput drop). The per-tunnel data-channel ceiling
  was probed directly on one host with `count = 1`: 15 concurrent
  streams before (the cap minus the service's 16-stream pool and
  iperf3's control stream) versus 47 after, with the 16th and 48th
  opening failing in the respective runs. See HANDOFF.md, "What
  landed".
- `default_split_send_size` is now 32 KiB (the vendored yamux default was
  16 KiB). Re-measured on the fixed engine — the earlier 16 KiB preference
  came from a run polluted by the dead-receiver leak below and its numbers
  are not comparable: the 32 KiB split now measures +45.7% with
  non-overlapping reps on the single-tunnel 8-stream loopback cell and
  +5..9% on the shaped cells, everything else inside the spread.
  Re-measured again 2026-09-22 in the cumulative A/B against `main`
  (fixed-harness `--ab`, 3 rounds): the single-tunnel 8-stream loopback
  cell sits at **+0.3%** with the rounds alternating direction, so the
  +45.7% figure did not reproduce; the default stands on no-regression
  grounds, not as a proven throughput win.
- The Noise record stream (the `noise` transport) is leaner: reads
  accumulate the two-byte length header together with the ciphertext in one
  buffer — one `poll_read` sweep per record instead of a separate header
  read first — and a record that coalesces with its successor in a single
  wake is decrypted from the same buffer without an extra copy. The
  per-record `set_len` dance is gone, and with it the wrapper's `unsafe`
  code. Covered by new unit tests in `src/transport/noise_stream.rs`. An
  A/B against the parent revision on one host (3 reps, 8 s tests) measures
  **+9.0%** on the loopback 1-stream cell and **+8.0%** on the
  loss1_rtt10 8-stream cell — both with non-overlapping rep ranges; the
  remaining cells sit inside the run spread and claim nothing. Details and
  the full table: HANDOFF.md, "Leaner Noise record stream".
- A record whose plaintext fits the caller's read buffer is now decrypted
  **straight into that buffer** instead of being staged in the wrapper and
  copied out: the plaintext length is known from the ciphertext length, so
  tokio's `ReadBuf::initialize_unfilled_to` can size the output region
  exactly (a free slice view for the standard caller, a memset of just
  that region for uninit callers). No unsafe is added. The mux path
  benefits too, because yamux's writer splits a frame into a header write
  and a body write, so the body record aligns 1:1 with the frame reader's
  body ask. The +12.3% first measured on the noise loopback 8-stream cell
  did not reproduce under the fixed harness (that cell reads -7.5% in the
  cumulative A/B, `main` ahead in all three rounds), so no gain is claimed
  for it; the change stands on its mechanism. Details: HANDOFF.md,
  "Direct decrypt into the caller's buffer" and "Final cumulative A/B".
- Connection setup allocates almost nothing now. The handshake runs on
  stack buffers (its messages are bounded by the pattern's tokens, well
  under 300 bytes — the snowstorm original allocated two 64 KiB buffers
  per handshake turn), and the three 64 KiB record buffers come from a
  bounded pool (64 sets, ~12 MiB high-water): freed in one piece they
  exceed the allocator's trim threshold, so without a pool every
  connection re-faults and re-zeroes 48 pages. A counting-allocator +
  getrusage probe (release build) measures pair setup at **232 us, 28
  allocations and 4 KiB, zero minor faults — down from 312 us, 42
  allocations and 900 KiB**. The system-level churn gain first reported
  alongside it (+11.7% connects/s) did not reproduce under the fixed
  harness — the direct-mode loopback churn arm sits at parity in the
  cumulative A/B — so only the in-process setup probes are claimed.
  Details: HANDOFF.md, "Connection-setup allocation".
- Control frames on the mux data path (SYN/ACK/FIN/window update/ping) are
  now staged into one buffer with their 12-byte header and written in a
  single call instead of two. Larger frame bodies keep the two-phase write
  so a 16 KiB payload is never copied twice.
- The KCP data path amortizes its per-segment bookkeeping: the engine's
  outbound datagrams stage in a reusable ~46 KiB buffer and cross the pump
  channel as ONE message per batch (closed at 32 datagrams, the staging
  cap, or the engine's flush boundary — `Kcp::flush` now calls
  `Output::flush`), and the reader side coalesces consecutive segments
  into one channel message per ~16 KiB while the reader keeps up
  (per-segment granularity returns under reader backpressure). Channel
  messages and `Bytes` allocations per segment fall ~32x on the send side
  and ~8-11x on the receive side (measured by the new `blobs_out`
  counter); the wire datagrams, the byte stream and the ARQ semantics are
  unchanged — a pacer denial still drops only the denied datagram. An
  interleaved A/B against the parent revision with the fixed harness
  (3 rounds x 3 reps x 8 s, cells loopback / loss1_rtt10 / rtt100, kcp4
  arm + the loopback mux-off control, both binaries' SHAs verified)
  measures **+6.6%** on loopback 1-stream and **+7.6%** on loopback
  64-stream (both non-overlapping), **+9.3%** on loss1_rtt10 8-stream
  (non-overlapping), CPU -7.2% and cpu/kframe -20.7% (median-only); the
  loopback 8-stream cell's -39% is that cell's documented cold-start
  bimodality (0.4-3.2 Gbit/s modes), not a claim. One open cost: loopback
  RSS +73% median-only against its own parent (the coalesced-blob channel
  residency, bounded at ~32 MiB per session under a full reader stall);
  the cumulative figure against `main` — from the first valid
  branch-vs-main A/B — is **+194%** on that cell, the per-session staging
  and coalescing residency times the 64 concurrent sessions the cell
  holds. Details: HANDOFF.md, "Phase 1 A/B" and "Final cumulative A/B".
- Zero-copy receive on the KCP read path (link L1): the engine's own
  segment buffers cross to the reader channel **by ownership**
  (`Kcp::recv_owned` freezes each segment in place; the reader-channel
  message is a `ReadBatch` of parts), so both per-byte copies the receive
  path paid are gone — the Phase-1 receive batching is now free
  (reference-count moves, not memcpys). The engine's buffer-API `recv`
  became a thin wrapper over the same core and is test-gated; new engine
  tests lock the two read APIs byte-for-byte identical. Measured per
  delivered segment (interleaved A/B, 3 rounds x 3 reps, cells loopback /
  loss1_rtt10 / rtt100, kcp4 arm + the loopback mux-off control, both
  binaries' SHAs verified): the recv loop fell **0.72-0.79 -> 0.042-0.046
  us on loss1_rtt10 (-94%)** and 0.30-58 -> 0.045-0.051 us on loopback,
  with the segments-per-reader-message ratio held and the input/output
  phases unchanged; CPU / RSS / cpu-per-kframe moved favourably on every
  cell (median-only) and no attributable throughput regression. Details
  and the full table: HANDOFF.md, "Link L1 A/B".
- Zero-copy send for stream-mode PUSH datagrams (link L2): the engine
  emits a 24-byte header plus the segment's own payload buffer by
  reference (`DatagramSink::write_datagram`; the segment payload type
  moved from `BytesMut` to `Bytes`), and the adapter's batch carries the
  payload as a second iovec (`Span::Split`) that `sendmmsg` writes
  directly — the engine's staging buffer and the adapter's batch copy
  both disappear on the send path, so a sent byte is copied zero times
  between the app write and the syscall. The wire bytes are identical to
  the packed form (engine test locks them byte-for-byte, and the
  retransmit re-emits the same allocation); ack/probe datagrams keep the
  packed path. Measured (interleaved A/B, fixed harness, 3 rounds x 3
  reps, cells loopback / loss1_rtt10 / rtt100, kcp4 arm + the loopback
  mux-off control, both binaries' SHAs verified): no attributable
  throughput change — loopback 1-stream -0.2% and 64-stream -1.0%
  non-overlapping (the control arm's own noise on those cells is +/-9%),
  the 8-stream cells inside their spreads, and the rtt100 1-stream -40%
  tool claim is not attributable (that cell is bimodal on both binaries
  and the medians differ by rep count, inside its documented 7x spread).
  CPU moved favourably on loopback (-4.6%) and rtt100 (-11.3%),
  median-only; RSS +12.4%/+16.7% median-only on loopback/loss1 (a
  pacer-denied span now holds its payload `Bytes` instead of a staged
  copy, bounded by the 32-datagram batch). Details: HANDOFF.md,
  "Link L2 A/B".
- Zero-copy write on the kcp4+noise path (link L3): the Noise record is
  handed to the writer channel **by ownership** — the record buffer the
  AEAD encrypted into *is* the transport's buffer, gated by the
  `TAKES_OWNED` const so a plain-TCP transport keeps the pooled path —
  and the engine's `send_owned` shares it per segment with O(1) `Bytes`
  splits, so both remaining per-byte copies on that path are gone and a
  sent byte is copied zero times between the app write and the syscall,
  AEAD aside. The engine's slice `send` is now only the owned path's test
  oracle (byte-for-byte lock). Measured (interleaved A/B, fixed harness,
  3 rounds x 3 reps, cells loopback / loss1_rtt10, kcp4 arm + the mux-off
  control, both binaries' SHAs verified; the run was interrupted at the
  start of the rtt100 cell, so that cell is not part of the comparison):
  **+5.7%** on loopback 1-stream and **+24.4%** on loopback 64-stream
  (both non-overlapping, the latter exceeding the control arm's own
  +11.3% bias on that cell); the loss1_rtt10 -0.3%/-0.4% tool claims sit
  inside the cell's own ±7% rep spread with comparable retransmits, and
  the mux-off control's -10.3% on loopback 1-stream is that cell's
  ordering bias. CPU / RSS / cpu-per-kframe all median-only favourable or
  flat. Details: HANDOFF.md, "Link L3 A/B".
- Zero-copy send on the stripe path (link S1): the stripe send direction
  reads each chunk **directly into the payload region of its frame
  buffer** — behind the 10-byte header, written in front once the read
  length is known — and hands the whole frame to the stripe by ownership,
  so the send direction's staging copy is gone (the read buffer becomes
  the frame). The stripe write itself keeps the borrowed boundary, and the
  wire format and the round-robin/commit semantics are unchanged. An
  interleaved A/B against the parent revision with the fixed harness
  (3 rounds, 3 reps, 8 s tests, loopback cell, mux-stripe arm + the
  unstriped `mux` inertness control + the mux-off control, both binaries'
  commit SHAs verified) measures **+9.7%** on the stripe arm's 1-stream
  (17.098 -> 18.758 Gbit/s, the head ahead in all 3 rounds) — inside the
  cell's spread, so recorded as directional, not claimed — with 8-stream
  at parity and the unstriped control unchanged. The cost is the per-chunk
  frame buffer allocated per read instead of reused: RSS +10.7% and CPU
  +3.3% on the opt-in stripe arm (median-only). The same link's sibling
  on the default arms, M1 (owned mux frame bodies), was reverted after its
  A/B — it failed the gate on four cells (including both single-rep
  64-stream points) with RSS +25% on the default arms; the evidence and
  the identified cost mechanism are in HANDOFF.md, "Link M1 A/B".
  Details: HANDOFF.md, "Link S1 A/B".
- The KCP attribution instrument's delivery phase is split and a
  timer-scope bug fixed: the "deliver" timer's binding outlived its
  statement, so it booked the rest of the pump round (the SACK check, the
  ack flush, the wire drain, the liveness tail) into the delivery phase —
  the first per-segment table's "delivery 2.46 us" was mostly the wire
  drain. The phase is now block-scoped, `kcp-stats` gains
  `segments_delivered`, `recv_empty`, `ms_deliver_spill` and
  `ms_deliver_recv`, and the re-based loopback kcp4 table (1 rep x 8 s,
  both processes) reads: sender per segment — writer 0.60 us, wire drain
  2.55 us, input 0.09, update 0.01 (pump body ~3.26 us, pump rounds
  3002/s against 4842/s before send batching landed); receiver per
  segment — input 0.42, deliver 0.41 (recv loop 0.40, spill 0.01),
  output 0.42, with 8.0 segments per reader message. The original "sender
  delivery anomaly" is retired as an artifact; the re-aimed targets (the
  wire drain on the send side, the recv-loop copies on the receive side)
  are exactly what the zero-copy links above remove. Also recorded: the
  full-path copy map and the L1-L3/M1/N1/S1 link sequence in HANDOFF.md,
  "Zero-copy route".
- Lint-hygiene pass over the data path: in-code lint waivers fell from 49
  to 14 in production code (70 -> 25 including test modules) and the
  `unsafe` surface from 19 items to 8. No wire-format, protocol or
  scheduling change; the full suite and both clippy passes are green. The
  waivers were removed by fixing the code rather than by relaxing a level:
  `expect_used`/`unwrap_used` sites became real error paths, `cast_*`
  sites became `try_from` conversions whose unreachable fallbacks are
  benign, the vendored KCP engine's `input`/`flush` were split into the
  helpers their own `too_many_lines` reasons described, and `pump_tail`'s
  per-round state became one `PumpTailCtx` instead of twelve parameters.
  The dead `Config::set_split_send_size` setter was deleted.
- `src/transport/udp_batch.rs` — still the single audited unsafe site —
  now keeps `unsafe` only at the FFI boundary: the two zeroed `msghdr` /
  `sockaddr_storage` templates (musl's private padding fields rule out a
  struct literal), the kernel-ABI cast that reads a received address back,
  the `recvmmsg`/`sendmmsg` calls, and the `Send`/`Sync` impls the
  reusable descriptor arrays need. Everything else is safe: iovec pointers
  come from `ptr::from_mut` on bounds-checked indices, staged spans from
  range-sliced `Bytes`, and the send address from
  `socket2::SockAddr::from(peer)` instead of a hand-built storage — which
  also means a **scoped IPv6 peer now carries its flowinfo and scope id**
  onto the wire, where the hand-built storage dropped both.

### Fixed

- Multiplexed connections no longer accumulate dead stream receivers.
  The engine used futures' `SelectAll` for the per-stream command
  receivers, which drops a sub-stream once it reports its end; the
  tokio-native conversion replaced it with a `Vec` and never removed the
  finished receivers. Every stream ever opened stayed in the vector, and
  the connection's poll loop is O(receivers) — a client serving many
  short-lived connections (connection churn) ended up polling thousands
  of dead receivers on every poll: the single-tunnel churn rate measured
  -29% and the first-byte p50 3.1 -> 7.1 ms against the pre-conversion
  engine. Fixed by retaining only the live receivers.
- `molehill --genkey` on a binary built without the `noise` feature names
  the feature correctly now ("noise", previously "nosie"). The
  `feature_not_compile` helper is cfg-gated to exist exactly when one of
  its `#[cfg(not(feature = ...))]` callers does, instead of being
  `allow(dead_code)`d away in full-feature builds.
- The benchmark harness's `--ab` mode spawned the default binary on both
  sides of an interleave (every spawn site read a frozen global instead of
  the per-arm knob), so every interleaved A/B taken with it compared one
  binary against itself. The global is gone — the binary path lives only
  in `knobs`, which the interleave swaps — and a label collision between
  same-named worktree binaries (the sides silently overwrote each other)
  plus the unrecorded label-to-path mapping that let one verdict be read
  backwards are fixed too. Two more bench bugs found while running the v0.9.0
  matrix are fixed with it: the `--ab` work had wrapped the arm set in a
  per-round loop, so a plain `just bench` ran every arm — molehill's and every
  peer's — three times over (the matrix took 3x its documented wall time and
  the extra passes bought nothing), and `setup_nps`'s `web` symlink was the one
  non-idempotent step of the peer's tree, so the nps arm failed on the second
  cell of any matrix (it had only ever been measured on loopback before). Every
  figure in this section quoted from an affected run has been re-taken or
  explicitly withdrawn above.

- **The Soak harness had three defects that made parts of a run wrong or
  unreadable, all of them silent.** (1) The Noise keypair helper raised
  `NameError` on first use: the memo it read was never defined, so every
  `noise` / `kcp4` / `noise-direct` variant died before it measured
  anything. (2) The single-run lock and the stale-process sweep both
  identified a live run by the string `bench.py` — the runner the matrix
  retired — so concurrent runs stopped being refused and the sweep could
  kill a live run's processes. (3) The SLO, the soak/cost load fractions and
  the per-stage statistics were partly dead: `SOAK_SLO_RTT_P99_MS` and
  `SOAK_SOAK_LOAD_FRACTION` were accepted and ignored, the capacity verdict
  ignored the interactive error rate that its own SLO documents, and the
  charts computed every per-stage statistic against a time base that
  selected no samples at all (so no stage percentile was ever drawn). The
  runner now records the revision, the binary version, the endpoints each
  probe dialed and every knob it used, and `soak_check.py` re-checks the
  endpoint invariant, the series completeness and the absolute SLO against
  that record before it compares anything to a baseline.


### Changed

- **The Soak charts were rebuilt around one question per figure.** The first
  release's set mixed a linear RTT axis (a single 7000 ms outlier flattened
  every stage below 100 ms), a missing legend entry for the throughput line
  and a "loss events" panel that plotted a constant `1` for every loss. The
  set is now: the master (per tool, log RTT with per-stage p50/p99 and the
  bulk throughput over the shared stage schedule, wedges marked on the
  bottom edge), `-stages.png` (small multiples — one panel per stage, a
  lollipop per tool, so tool-vs-tool per condition reads at a glance),
  `-capacity.png`, `-udp.png` (RTT plus a sliding loss *rate* derived from
  the probe's own attempt stream), `-drift.png` (with every fitted slope
  printed) and `-cost.png`. Each figure carries its method constants and
  revision in the footer, and a tool keeps its colour across the whole set.
- **Unsafe is now denied crate-wide** (`unsafe_code = "deny"`, was `warn`),
  so an unexpected `unsafe` fails a plain `cargo build` instead of only the
  `-D warnings` gate. The one module that needs it — the `recvmmsg`/`sendmmsg`
  batching FFI — keeps its per-item `#[expect(unsafe_code, reason = ...)]`
  with a `SAFETY` comment; the alternatives were evaluated against their
  sources and are recorded in docs/lint-policy.md ("Unsafe"): `nix` cannot
  drop the `Send`/`Sync` proofs (its `MultiHeaders` is itself `!Send`) and
  `quinn-udp` would change the KCP send path's GSO semantics. Two clippy
  waivers were removed outright on the way: the KCP stats clock now converts
  nanoseconds with `Duration::as_secs_f64()` and masks the 32-bit protocol
  wrap instead of casting, so both cast waivers are gone.
- The python bench scripts are format-checked as well as lint-checked: the
  pre-commit chain now runs `uvx ruff format --check benches/scripts/`
  beside `uvx ruff check`, `just py-lint` runs both and `just py-fmt`
  auto-fixes. The ruff waiver list shrank to the one entry that is true of
  every script here (they measure PATH binaries), with the rest waived
  inline at their own sites and a named reason each.
- The docs-only path CI already had (`ci.yml` / `docs.yml`) is now mirrored by
  the hooks: `githooks/docs-only` classifies the staged paths (commit) or the
  pushed range (push), and a change limited to markdown, `docs/**` or
  `assets/**` runs only the two gates it can move — the secret scan and the
  docs-alignment check — instead of the whole Rust chain. Anything ambiguous
  (empty change set, new branch, tag push, or a commit mixing docs with code)
  falls back to the full chain, and deleting the classifier restores it.
- **HANDOFF.md was restructured and condensed** (1762 → ~770 lines): it now
  opens with the branch's state and the theme planned for the next update
  (measure the three unmeasured data-path axes — establishment, fragmentation,
  reconnect — then fix what they show), keeps the landed-work index, the
  gated candidate table and the open items, and compresses the historical
  measurement record to the verdicts, numbers and commits that carry it. An
  Everything measured before the Soak model is now bannered as **historical
  context, not evidence** — the matrix harness was proven wrong in ways that
  were invisible at the time (`--ab` spawned one binary on both sides, a
  verdict could be read with the sides swapped, the memory axis read a key
  that never existed, several headline figures never reproduced), so those
  numbers may not be quoted, compared or gated on. One verdict was corrected
  in place first (a throughput claim that did not exceed its own ordering
  floor); the rest was left as written and demoted.
- **The documentation set was given an ownership contract.** Every page now
  owns one topic and links to the others (the routing table is in AGENTS.md
  §3, the audience-and-scope table in docs/structure.md): the landing pages
  describe what is true now, `docs/benchmarks.md` (+ its Chinese mirror) is
  the home of the benchmark method — what is measured, how to read the
  charts, the stage schedule, the SLO, the test types, the per-decision
  measurements and how to reproduce a run — `docs/configuration.md` owns what
  each setting does and hands the measured costs to that page, and
  `docs/release.md` owns the ritual instead of re-explaining the method.
  Migration and deprecation narrative ("what replaced the old tables") is out
  of the user-facing pages; it lives in this changelog, the release notes and
  HANDOFF.md. `githooks/check-docs` now enforces the parts that can be
  grepped: every page has an owner, user-facing pages have a mirror with the
  same structure, and the READMEs link each page in their own language.
- **The configuration, transport and contributor documentation was aligned
  with the code it describes.** Five documented behaviours were wrong or
  unenforced and are now stated as they are: privileged ports inside an
  `allow_ports` range are *not* treated specially (the OS decides), the
  tunnel-count ceiling is 64 streams per tunnel, `retry_interval` is the
  backoff cap rather than a fixed interval (after three tries the client
  falls back to 1 s), `nodelay` only reaches the sockets the client owns,
  and a `psk` is silently unused unless the configured noise `pattern`
  carries a PSK modifier. The `x448` keygen promise was dropped (the
  shipped `snow` backend has no X448). The `count × 64` arithmetic, the
  `[client.data]` behaviour in a multiplex-less build and the
  `MOLEHILL_STRIPE_COUNT` override are stated where they belong, and the
  Chinese configuration page regained the facts it had dropped for
  `resume`. The landing pages, the release ritual and the contributor docs
  were re-grouped by audience (users vs contributors) and stripped of the
  retired measurement matrix's vocabulary.
- A release now fails fast on a stale ritual: `release.yml` refuses an
  undated changelog section and a missing `results-soak-vX.Y.Z.json` /
  `assets/soak-vX.Y.Z.png`, and both it and the test-build workflow take a
  concurrency group so two runs cannot race on the same release or cache.
  The `githooks/check-docs` gate now checks every command a hook runs (not
  only `cargo` ones), checks the reverse direction (a documented gate that
  no hook runs), and pins the docs index on both READMEs. `--version` now
  reports the commit SHA it always printed a line for, and marks a dirty
  tree.

## [0.8.1] - 2026-09-11

### Fixed

- A service whose control channel ended on its own — client shutdown, a
  connection reset, a heartbeat write failure — kept its public port bound on
  the server. The connection pool owns the bound listener, and it only ever
  stopped when a *new* registration took the service over, so a service
  nobody was driving any more still held its port: visitors reached nothing
  and the next registration of that service was rejected with `Port N is
  already in use` until the server was restarted. The pool now also watches
  its control channel's task and stops with it, releasing the listener (and
  the UDP socket) on both the TCP and the UDP path. Covered by
  `finished_control_channel_releases_its_ports`.

### Changed

- **The benchmark baseline is carried forward from v0.8.0.** This patch
  changes no forwarding path and no measurement code, so re-running the
  matrix would only re-measure the same build on a different day:
  `results-v0.8.1.json` and the charts are the v0.8.0 matrix unchanged, with
  the provenance recorded in the results meta. `just bench-check` compares
  them against `results-v0.8.0.json` and reports no regression.

- CI skips the code chain for a change limited to markdown, `docs/` or
  `assets/`: those paths cannot move the Rust gates, so `ci.yml` filters them
  out and a new `docs.yml` runs the docs-alignment check instead — the one
  gate such a change can break. It needs no toolchain and finishes in
  seconds. A commit mixing docs and code paths runs both workflows, and the
  tag-driven release workflow is unaffected.

## [0.8.0] - 2026-09-11

> **The benchmark figures quoted per entry were measured when that change
> landed**, on the host then in use. The 2026-09-10/11 method revision
> (rate-cell shaping, throughput window convention, UDP capacity ladder)
> then re-measured the whole matrix against the committed defaults, so the
> figures are not comparable across entries. The authoritative, mutually
> comparable set is the README Benchmarks chapter, regenerated from
> `benches/scripts/bench/results-v0.8.0.json`; HANDOFF.md records that
> baseline's scope and its regression-gate waiver.

### Added

- The KCP adapter's SACK gap notification tightens from 50 ms / 32
  segments to 10 ms / 16: the cooldown now sits below the nodelay RTO
  floor, so a SACK beats the RTO backoff instead of firing after it
  (measured +9% on the loss1_rtt10 cell, flat elsewhere, no jitter-cell
  regression).

- The KCP data plane batches UDP datagram IO on Linux (`recvmmsg` /
  `sendmmsg`, up to 32 datagrams per syscall — the same amortization
  QUIC stacks get from UDP GSO): loopback throughput improves +32%
  (1-stream) and +8% (8-stream) on the kcp4 arm, with the weak cells
  flat; the wire format is untouched (datagrams are byte-identical).

- `just test-fast` and `just bench-fast`: a quick verification loop (lib +
  core integration subset) and a ~2-minute molehill-only smoke matrix into
  `results-dev.json`; the bench runner's default `--out` derives from
  `Cargo.toml`'s version, so dev runs never merge into a release baseline.

- v0.8.0 benchmark re-baseline: `results-v0.8.0.json` and the README
  chapter/charts now describe the merged defaults (count=4, ring-
  accelerated noise) with three new cells (rate-limited r100/20 and
  r20/40, jittery j20/10) and six new metrics (CPU%, connection churn,
  sustained UDP capacity, a 64-stream scale point, a mixed bulk +
  interactive workload, per-rep min/max). The matrix self-throttles
  (nice 10, load-aware cooldown) and every probe socket is bounded.
  The v0.7.2 baseline ran a different container and single-tunnel
  default, so the old regression gate does not carry over;
  `results-v0.8.0.json` is the new baseline.

- Parallel multiplex tunnels by default (`[client.data].default_count = N`,
  default 4): data channels are spread round-robin over N tunnel connections
  per service, isolating head-of-line blocking (rtt10 HoL max 101 -> 81 ms)
  and aggregating beyond a single TCP flow (loopback 8-stream 4.3 -> 12 Gbps,
  1% loss 3.7 -> 7.1 Gbps in the bench matrix); a dead tunnel is skipped
  transparently. `1` reproduces the single-tunnel behavior.
- Per-service servers and auth: `[client.services.<name>].remote_addr`
  overrides `[client.control].default_remote_addr` for that service — its
  control channel and, by default, its data plane dial the given server —
  and `token` overrides `[client].default_token`, so one client can spread
  services across several molehill servers (region replicas, per-tenant
  servers, rolling migrations) even when the servers do not share a token;
  each server covers the registered ports in its own `allow_ports`.
- Per-service data-plane and control overrides: `[client.services.<name>]`
  accepts `mode`/`count`/`carrier` (defaults in `[client.data]`),
  `remote_addr`/`heartbeat_timeout`/`retry_interval`/`token` (defaults in
  `[client.control]` / `[client]`), so one client can mix data paths per
  service (e.g. a multiplexed interactive service next to a `direct` bulk
  service) and per-service heartbeat timeouts against servers with
  different heartbeat intervals. The pools were already per service; the
  server needs no changes — it adapts per connection and lazily opens its
  KCP listener on the first `kcp` registration.
- Optional KCP data tunnels (`[client.data].default_carrier = "kcp"`,
  feature `kcp`):
  multiplexed tunnels ride KCP-over-UDP sessions (fixed, recorded protocol
  parameters) behind a thin in-repo tokio adapter, with Noise kept on top
  when the control transport is `noise`; the server lazily binds its UDP
  listener on the first registration that declares the `kcp` carrier (see
  HANDOFF.md); idle-tunnel keepalive is a known limitation.
- Explicit data-plane endpoints: `[client.data].default_data_addr` (defaults to
  the service's control endpoint) and `[server.data].bind_addr` (defaults
  to the control listener) let operators move data channels onto a separate
  port or interface. The server binds a dedicated listener only when the
  two addresses differ; otherwise the control listener keeps accepting data
  connections exactly as before.
- Release-grade benchmark matrix (`just bench`): peer comparison against frp,
  rathole (upstream) and bore across loopback and weak-network cells
  (rtt/loss via netem, with a userspace delay-proxy fallback when
  `CAP_NET_ADMIN` is unavailable), chart + table rendering (`just bench-plot`)
  and a per-tag regression gate (`just bench-check`) required before tagging.

### Changed

- Benchmark measurement method revision (2026-09-10). Rate cells shape `lo`
  with a `limit 2000`-packet queue (recorded as `netem_rate_limit`): the old
  shallow queue tail-dropped GSO segments and cost ~80% of the shaped rate,
  so a rate cell measured the shaper rather than the tool. Throughput
  sampling is isolated per repetition (fresh iperf3 server after a stall or a
  `server is busy` answer, a client bound never tighter than the historical
  `secs + 20`), keeps raw per-rep JSON under `iperf-raw/<arm> <cell>/`, uses
  one stated window convention (bytes over the measured window, with the
  receiver's own window beside it, and the receiver's count used when the
  sender's accounting is provably degenerate), and records a typed reason for
  every `null`. `audit_results.py` gates completeness (holes, nested fields,
  sampler output) and the endpoint invariant below; `docs/release.md` and
  AGENTS.md §10 state the comparability rules.

- The UDP capacity probe is now a ladder rather than a fixed two-point
  sample: 500 pps to the configured burst rate in 2000-datagram bursts with a
  1.5 s drain, reporting the highest step delivered within
  `max(2%, cell loss + 2pp)` plus the knee. It finds real knees (10 ms cell
  12 000 pps / 10.9 Mbit/s, 100 ms cell 1 000-2 000 pps / 0.7-1.4 Mbit/s) and
  a lower bound on unshaped loopback (27.2 Mbit/s, the ladder top), where the
  previous design only reported its own pacing back.

- The v0.8.0 benchmark baseline was re-measured on 2026-09-10/11 against the
  revised method (52 arms on host `0b073ddbf222`; the audit reports zero
  holes, zero arm errors and no endpoint violation), and the README
  chapter/charts were regenerated from it. Because the revision changed the
  shaping model, the throughput window/accounting and the host, **every
  number in the chapter is a fresh same-host measurement — the previous
  v0.8.0 rows and the v0.7.2 regression baseline are not comparable.** The
  re-derived conclusions from this run: Noise retains ~58%/76% of plain
  throughput, `count = 4` aggregates at 8 streams (19.5 vs 9.2 Gbit/s on
  loopback, 12.3 vs 4.5 at 1% loss), and the KCP carrier stays far behind TCP
  wherever the path is not the bottleneck (1.1 vs 14.9 Gbit/s at 8 streams,
  ~3x RSS) with UDP-only paths and rtt100 session quality as its uses. A
  UDP-under-load weakness seen in two earlier runs (100% paced-pinger loss in
  the 10 ms cell) did **not** reproduce in this one (2%), so it is recorded as
  variance rather than a finding.

- Fixed a measurement regression that invalidated an intermediate baseline:
  the throughput sampler dialed the iperf3 backend instead of the tool's
  exposed port, so it reported the loopback ceiling with the tunnel bypassed.
  Entries now record the endpoint, the sampler raises when the two ports are
  equal, and the audit fails such a run. With the endpoint fixed, the
  weak-cell limits are visible again and honest: the 20 Mbit/s cell measures
  1-stream (0.009 Gbit/s) with its 8-stream slot `null` and a recorded
  timeout reason, and every `null` in the file carries its reason.


- The logo was replaced with the mole + volcano + data-stream design
  (`assets/molehill.svg`), and per-run bench artifacts
  (`results-arms-*.json`) are no longer tracked — only the release
  snapshots (`results-vX.Y.Z.json`) and charts stay committed.

- Docs now present molehill as an independent project: the rathole fork is
  kept as provenance (README / README.zh provenance note, AGENTS.md §7,
  `docs/release.md` versioning, the crate doc comment and the Cargo
  description) instead of as the project's identity. The fork point and the
  continued version line stay documented and unchanged.

- Benchmark peer binaries are cached under `~/tmp/bench-peers` instead of
  `/tmp/bench-peers`, which session cleanup wipes (forcing a re-download
  through the rate-limited GitHub API); `PEER_DIR` still overrides it.

- **BREAKING (protocol v3)**: every connection between client and server
  starts with a one-byte transport selector (`0x00` plain / `0x01` noise,
  on TCP connections and KCP sessions alike), and the service registration
  carries the data-plane carrier the client will use (`tcp`/`kcp`). The
  server accepts whatever the client speaks on one listener — its
  `[server.transport]` block is keys-only — and opens its KCP UDP listener
  lazily on the first `kcp` registration, rejecting with a precise reason
  when the bind fails or the binary lacks the `kcp` feature. Per-service
  encryption: `[client.services.<name>].transport` accepts `type`
  (`"noise"` / `"plain"`, unset = follow `[client.transport].type`)
  plus per-service `noise` keys — one client can run plain and encrypted
  services side by side (e.g. a service dialing a different server with its
  own public key), and the client is no longer generic over one transport
  (`ClientStream`/`ClientTransport` enums). Both ends
  must upgrade together (protocol version mismatch is a hard error).
- `snow` is upgraded to 0.10 (MSRV 1.85, edition 2024): the u16-framed
  record stream wrapper is vendored in-repo (`src/transport/noise_stream.rs`,
  ported from snowstorm 0.4.0 — unmaintained and pinned to snow 0.9, so a
  real upgrade requires owning the wrapper; Apache-2.0 attribution in the
  file header) and `pin-project` joins the `noise` feature. No behavior or
  wire change (measured throughput identical to the snow 0.9.6 build); the
  wrapper is now ours, which unblocks the leaner-record-stream work
  (HANDOFF). `Builder::local_private_key`/`remote_public_key`/`psk` now
  return `Result` and validate key/PSK lengths at build time — a wrong-size
  key surfaces at handshake setup instead of mid-handshake.
- KCP data tunnels: the ARQ send/receive windows are doubled (2048/4096
  segments, ~2.8/5.7 MiB in flight at MTU 1400 — the old 1024/2048 capped
  throughput at BDP/RTT, e.g. ~0.28 Gbps at 40 ms tunnel RTT). Measured
  with the bench matrix: +29% single-stream on the 1%-loss/10 ms cell
  (0.29 -> 0.38 Gbps) and +20% on the 5%-loss/100 ms cell, loopback
  unchanged. A 5 ms flush interval was tried and rejected: it regressed
  loopback 8-stream ~3x (busier update timer under saturation). The
  adapter-level PING/PONG keepalive (2 s, NAT warm + RTT probe) is now
  documented as present; dead peers are still only confirmed on the next
  write (~20 RTOs). KCP's measured role: UDP-only paths (TCP blocked by
  firewall/NAT) and latency-first interactive traffic on high-loss,
  high-RTT links (5%-loss/100 ms cell: udp p50 601 vs 813 ms and hol max
  gap 1444 vs 2963 ms, kcp4 vs the noise-TCP arm).
- The Noise data path now runs ring's hardware-dispatched
  ChaCha20-Poly1305 (`snow`'s ring-accelerated resolver, part of the
  default feature set; `snow` is now a direct dependency, `snowstorm`
  keeps the stream wrapper): measured +29% loopback single-stream
  (3.81 -> 4.92 Gbps) and +25% 8-stream (11.80 -> 14.75 Gbps) end-to-end
  on x86-64, and +7%/+16% on the 1%-loss/10 ms cell. No wire change —
  every pattern gets the accelerated cipher; the pattern hash (BLAKE2s
  default) only runs once per connection in the handshake.
- **BREAKING (transport)**: the `tls` and `websocket` transports are removed.
  `[client.transport]` / `[server.transport]` accept only `"plain"` (default)
  and `"noise"`, and the `[client.transport.tls]`,
  `[client.transport.websocket]`, `[server.transport.tls]` and
  `[server.transport.websocket]` blocks, the `TlsConfig`/`WebsocketConfig`
  types, and the `native-tls` / `rustls` / `websocket-*` cargo features with
  their dependencies are gone. Migrate to `noise` (docs/transport.md); if TLS
  termination is still required, run nginx or a CDN in front of molehill.
  Old configs naming the removed values or blocks fail loudly at startup.
- **BREAKING (configuration)**: the client/server layout is now three blocks
  per side. `[client]` keeps `default_token`; the client-level `prefer_ipv6`
  was removed (it had no effect in the running code — only the per-service
  `prefer_ipv6`, which steers the UDP forwarder's bind choice, is live); the
  control channel moved to `[client.control]` and the data plane to
  `[client.data]`. The client-wide defaults are `default_`-prefixed so they
  read distinctly from the per-service overlay keys:
  `[client.control]` (`default_remote_addr`, `default_heartbeat_timeout`,
  `default_retry_interval`) and `[client.data]` (`default_mode`,
  `default_count`, `default_carrier`, `default_data_addr` — replacing `mux`,
  `mux_tunnels`, `tunnel`, `tunnel_addr`), while `[client.services.<name>]`
  carries the overlays (`remote_addr`, `heartbeat_timeout`,
  `retry_interval`, `token`, `mode`, `count`, `carrier`, `protocol`,
  `transport`, `udp_forwarder_ipv6`, `udp_send_queue_size`). The service
  key `type` was renamed to `protocol` (it selects tcp/udp and collided
  with the transport `type`), `prefer_ipv6` to `udp_forwarder_ipv6` (it
  only steers the UDP forwarder's bind choice), `udp_sendq_size` to
  `udp_send_queue_size`, and the per-service transport toggle is
  `transport.type` (`"noise"`/`"plain"`). On the server,
  `bind_addr`/`heartbeat_interval` moved to `[server.control]`,
  `kcp_bind_addr` became `[server.data].bind_addr`, and
  `[server.data].carriers` is **gone** — the registration carries the
  client-declared carrier (v3) and the server opens the listener on first
  use. `[server.transport]` lost its `type` key: it holds only the Noise
  keys, and the server accepts both plain and Noise connections (v3
  transport selector byte); a client that decides to encrypt (noise-vs-
  plain benchmark in the README is the price list) needs no server-side
  `type` agreement. The transport value `"tcp"` is now `"plain"`,
  `[client.transport.tcp].proxy` moved to `[client.transport].proxy`, and the
  dead transport-level `nodelay`/`keepalive_secs`/`keepalive_interval` keys
  were removed (fixed internal defaults; the per-service `nodelay` remains).
  Old keys are rejected by `deny_unknown_fields`, never silently ignored;
  the full migration table is in docs/configuration.md.
- Bounded per-tunnel yamux buffering: the receive window is fixed at 64 MiB
  per tunnel with 32 streams (was: yamux's own 1 GiB / 512, and configurable).
  With the old defaults a lossy link accumulated unbounded backlog — measured
  211 MiB avg / 620 MiB peak RSS in the client process at 1% loss (10 ms
  RTT); the new defaults keep every cell of the bench matrix at its previous
  throughput (the auto-tuner needs allocatable credit headroom,
  ~window/2RTT per stream; 16 MiB / 32 streams measured 30-65% slower on
  delayed links) while bounding the loss backlog at ~35 MiB avg / ~69 MiB
  peak. The values are internal constants, no longer config keys — yamux
  couples them (`window >= 256 KiB * streams`), so exposing both invited
  misconfiguration. Applies to TCP and KCP tunnels alike.
- The client tunnel driver is now event-driven: announcing a freshly opened
  stream is a polled future instead of an awaited socket flush inside the
  driver loop, so a backpressured tunnel can no longer stall inbound frame
  processing (window updates, data) while an open is in flight.
- The transport-arm feature `kcp` is part of the default feature set, so the
  standard `cargo build --release` binary covers the tcp/kcp arms. Slim
  builds (`embedded`, the `minimal` profile, the container recipe) keep
  their explicit feature lists and are unaffected.
- Benchmark entries are PEP 723 python scripts run via `uv run` (no shell test
  entries); peer tools (frp, rathole, bore) are fetched as the latest
  GitHub release binaries — never built from source. The matrix measures per
  tool and cell: through-tunnel TCP
  throughput (1/8 streams), connection-path RTT, data-path RTT, UDP session
  quality (RTT/loss/jitter/max gap), a head-of-line probe and RSS (schema v3).
  Runs are resumable and continue on error: each completed arm is printed and
  checkpointed to the results file immediately, `--tools/--cells/--variants`
  select subsets, and results merge unless `--fresh` is given. Molehill runs
  mux / mux-off / mux1 / noise / kcp4 variants (mux-off on the loopback cell
  only).
- Benchmark charts split into a plain-TCP peers chart (molehill default mux
  vs frp / rathole / bore) and a molehill-family chart isolating the costs of
  multiplexing (mux vs mux-off) and encryption (mux vs noise); the
  encrypted chisel peer was removed — its SSH tunnel is not comparable on the
  plain-TCP axis.
- The python bench/test entries are now linted by ruff in the pre-commit gate
  (`ruff.toml`, waivers documented there), and the uv/PEP 723 convention is
  part of AGENTS.md.
- The multiplexing-cost comparison covers the loopback cell only (mux-off
  runs loopback by design; the weak-cell multiplexing behavior is told by
  the tunnel-count axis — count = 4 vs count = 1 — in the count chart). The
  README benchmark section is reorganized around a configuration-selection
  guide (when to keep or turn off mux, plain vs noise) and a methodology
  section documenting the test discipline and known limits.
- Documentation policy: governance and contributor docs are English-only —
  the four `*.zh.md` governance mirrors were dropped; user-facing docs
  (README, configuration, transport) keep Chinese mirrors, and the Chinese
  configuration (`docs/configuration.zh.md`) and transport
  (`docs/transport.zh.md`) pages were added. AGENTS.md §2/§3 and the
  docs-alignment check were updated to match.
- Releases are published **directly** — the GitHub Release is created public
  from `CHANGELOG.md` notes as soon as the build matrix finishes; the draft
  stage is gone and the human checkpoint is the tag push itself.
- New `githooks/pre-tag` release review (light, seconds): tag↔version match
  (incl. `Cargo.lock`), dated non-empty changelog section, committed bench
  results/chart, container-job greps, plus an advisory audit checklist
  (CHANGELOG & docs, container build, benchmark gate, deliberate-release
  confirm). git has no native tag hook, so it fires via `just tag` and again
  inside pre-push on every `v*` tag push (before the heavy gates); the
  pre-commit / pre-push / pre-tag responsibilities are now separated in
  docs/checks.md.
- Benchmark data hygiene: the missing/failed cells of the v0.8.0 baseline
  are filled from targeted same-host re-runs — the peer tunnels at the
  rate-limited cells (frp/rathole/bore 1-stream now measured, previously
  null; the 8-stream slots stay `null` by design — see the known
  limitation below), the loss2b25 steady-RTT probes, churn at rate20,
  and every HoL probe that previously recorded fake zero RTTs now records
  `null` with its reason. The benchmark backends now run **per arm** (a
  wedged iperf3 server can no longer poison every later arm of a cell;
  EADDRINUSE retries + SIGKILL cleanup), the 8-stream test runs after the
  cheap probes so a wedge cannot contaminate them, and every throughput
  failure carries the iperf3 stderr in `partial_metrics` instead of a bare
  null. The charts mark missing data with an 'x' and the README tables are
  emitted mechanically from the raw results (full per-cell count/carrier
  tables, adaptive decimals). Known limitation, recorded in the
  methodology: on the low-rate rate20 cell, eight parallel iperf3 streams
  wedge iperf3's single-test server on the shaped loopback path
  (GSO-sized segments × the netem packet limit buffer seconds of data), so
  its 8-stream slot is `null` with the timeout reason in `partial_metrics`
  — consistently across all tools (rate100 measures both).
- Configuration docs restructured around the measured data: a decision
  tree (mermaid) with the v0.8.0 benchmark costs now guides the
  `mode`/`count`/`carrier`/transport choices before the specification, in
  both languages. The `examples/` directory is gone — every example config
  (minimal, full, noise, udp, unified, proxy, iperf3) and the
  systemd/container deployment files now live as code blocks in
  docs/configuration.md, and the config test suite parses those blocks
  directly (it previously read the `examples/` files), so the shipped
  examples keep being validated.

### Fixed

- The container image (and the musl release binaries it is built from) now
  include the `kcp` feature: the explicit feature lists in `release.yml`
  and `just container` omitted it while the default set — and the docs —
  include KCP, so `default_carrier = "kcp"` failed with a config error on
  the GHCR image. Verified with a `x86_64-unknown-linux-musl` check of the
  exact feature set.

- The KCP protocol engine is now maintained in-repo as molehill's own
  module (`src/kcp/`), re-implemented from the reference C implementation
  by skywind3000 instead of the `third_party/kcp` path dependency: cargo
  strips path dependencies when publishing, so `cargo publish` used to
  compile against the unpatched registry version and fail verification.
  `cargo package` now verifies cleanly. Aligned with the reference: fast
  retransmit is ack-timestamp-gated (`IKCP_FASTACK_CONSERVE` semantics —
  the old dep feature of the same name was inverted and shipped the
  aggressive variant), the timeout ssthresh halves the flush-entry cwnd,
  window probing starts after 5 s, and stream-mode `send` reports partial
  progress; the unused accessors, the `tokio`-feature code and the
  conv-adoption hook are trimmed.

- The committed charts are regenerated with the canonical cell order and
  the canonical footer (targeted re-runs no longer shuffle the axes or the
  footer cell list).

- Benchmark chart values: a measured zero is labelled `0` (a zero-height
  bar was indistinguishable from an absent slot, e.g. the UDP-loss panel
  showed empty-looking cells), and sub-1 cost metrics keep three significant
  digits (`kcp4`'s mixed-bulk 0.029 Gbit/s used to print as `0.0`).

- Benchmark charts only draw rows and cells that actually take part in a
  comparison: the molehill-vs-peers per-cell panels skip the cells the
  plain-TCP peers do not run (those molehill-only stories live in the
  count/carrier charts), the mux chart is a single loopback row (mux-off is
  loopback-only by design), and the cost chart's 64-stream panel omits
  `mux1` instead of drawing an empty slot. The HoL probe now records a
  connected pinger that completes zero or one round trip as a stall (the
  elapsed wait) instead of `null`, which used to vanish from the chart
  (frp/rathole at rate20_rtt40).

- Benchmark charts and runner: an absent measurement now renders as a grey
  `x` instead of a fake `0.0` bar (the cost chart drew `mux1`'s unmeasured
  64-stream and mixed-bulk slots as zero), the runner skips by design the
  probe it would otherwise wedge — the 64-stream scale point above an
  arm's `count × 32` yamux ceiling —
  recording the reason in `partial_metrics`, and a failed iperf3 run
  reports iperf3's own error/exit status instead of a bare
  `KeyError: 'sum_received'`. `mux1`'s mixed-bulk probe is re-measured at
  9.0 Gbit/s now that the over-ceiling scale test no longer leaves the
  shared iperf3 server wedged.

- Benchmark probes: the steady-ping probe reconnects on stall or EOF (no
  more probe-wide `TimeoutError` on burst loss; the EOF recv-spin is gone),
  the churn probe no longer `IndexError`s when every connection fails
  (reports zeros with `success_pct`), the HoL pinger records `null` instead
  of fake 0 ms when it gets no samples (single-sample gaps are `null` too),
  and a failed mixed-bulk transfer records its reason alongside the echo
  half of the metric.

- Benchmark throughput now measures **through the tunnel** (iperf3 dials the
  tool's exposed port); previously it dialed the backend directly, so every
  tool reported the loopback iperf3 ceiling (~46 Gbit/s) regardless of tool or
  network cell. Historical results files and charts are not comparable to the
  new schema.
- Benchmark runner hardening: a global lock refuses concurrent runs (they
  used to reap each other's live processes through the stale-pid sweep, which
  now also never touches processes owned by a running runner); SIGTERM takes
  the Ctrl-C cleanup path (arms killed, netem qdisc removed, full meta
  checkpointed); a killed `--fresh` run can no longer destroy the previous
  results file (it is backed up first); mid-run checkpoints carry the full
  meta instead of only the schema; per-metric failures record `null` plus a
  `partial_metrics` list instead of faking `0.0` or discarding the arm; a
  backend bind failure records that cell's arms as errors instead of aborting
  the whole matrix; retransmit counts belong to the median throughput rep;
  leaked iperf3 backends are now reaped by the sweep (workdir in cmdline).

## [0.7.2] - 2026-09-05

### Changed

- Adopted the rust-agents-template lint set in full: `pedantic` (deny) and
  `missing_docs` (warn) are declared in `Cargo.toml` and the whole codebase
  passes them; the public API and config structs are now documented. The
  unused `lazy_static` dependency was replaced by `std::sync::LazyLock` and
  dropped.

### Fixed

- UDP forwarding keeps **session affinity** for every remote peer: the
  server routes all datagrams of one peer address through a single data
  channel (previously the per-datagram pool load balancing split a burst
  across channels), and the proxy client keeps one local outbound socket per
  peer for its whole session, surviving channel re-sharding and channel
  loss. Stateful UDP sessions — e.g. Minecraft Bedrock (RakNet), QUIC,
  WireGuard — no longer tear in half with `pool_size > 1`; the pool now
  shards distinct peers, not packets. The server also replaces dead UDP
  data channels to keep the pool at its configured size.

## [0.7.1] - 2026-09-05

Project base rebuilt on [rust-agents-template](https://github.com/NIyueeE/rust-agents-template).
No forwarding-behavior changes; the git history was also rebuilt in this
release (upstream rathole history intact; the fork's development commits
squashed into one release commit per version).

### Added

- Layered git hooks under `githooks/`: pre-commit fast gates (fmt, staged-
  changes secret scan, unused-dependency check, docs↔code alignment, strict
  clippy) and pre-push heavy gates (audit, deny, outdated, tests); activate
  with `just setup`
- `githooks/check-docs`: verifies the governance docs still describe the code
  (hook commands, lint tables, edition, toolchain channel, README doc index,
  CI/release wiring)
- `githooks/check-secrets`: blocks commits that stage credential-shaped
  lines (`security-scan:allow` marker to waive a documented line)
- Modular bilingual governance docs: `docs/checks.md`, `docs/lint-policy.md`,
  `docs/release.md`, `docs/structure.md` with `*.zh.md` counterparts
- Dependency policy via `cargo-deny` (`deny.toml`: licenses, bans,
  advisories, yanked crates) wired into pre-push and CI
- `just` recipes: `setup`, `fmt`, `test`, `check`, `powerset` (cargo-hack
  feature powerset), `container` (scratch image)
- `.github/workflows/test-build.yml`: manual per-commit, per-platform CD
  test builds that never publish
- `CONTRIBUTING.md`, `SECURITY.md` (private vulnerability reporting with
  molehill-specific scope notes), `.editorconfig`, Dependabot config for
  cargo and GitHub Actions, PR template, issue-template config
- `rust-toolchain.toml` now declares `clippy` and `rustfmt` components
- All GitHub Actions pinned to commit SHAs (Dependabot tracks the `# vX`
  comments)

### Changed

- CI restructured: the full check chain runs via `just check`; the
  feature-powerset, per-feature test matrix, minimal-size check, and
  cross-platform builds remain dedicated jobs
- Release workflow enforces the changelog-driven policy: a missing
  `## [x.y.z]` section in `CHANGELOG.md` fails the release before anything
  builds; notes are only ever extracted from the changelog; releases are
  created as drafts
- `AGENTS.md` rewritten as repository rules (self-check, waiver discipline,
  docs↔code alignment, commit convention, tag-push policy, provenance from
  rathole and the upstream-anchored versioning); architecture details moved
  to `docs/structure.md` and `docs/internals.md`
- READMEs gained a Development section (hooks activation, `just setup` /
  `just check`) and a split documentation index; the stale `rust-1.95.0+`
  badge now reflects the `stable` channel

### Removed

- Legacy `.githooks/pre-commit` (superseded by the layered `githooks/` chain)

## [0.7.0] - 2026-08-26

Major release: the server no longer needs per-service configuration — clients
declare what to expose and the server enforces policy. **Protocol bumped to
v2: upgrade both ends together** (mismatched peers fail loudly with a
"please update" message; no compatibility with 0.6.x, by design).

### ⚠ Breaking changes & migration

1. **`[server.services.*]` removed.** The client is now authoritative.
   - Move each service's server-side `bind_addr` into the client's
     `remote_bind_addr`.
   - Delete all per-service `token = ...` lines; both sides now use one
     required `default_token`.
   - The server now requires `allow_ports` (see below).
2. **Protocol v2**: hello/auth/registration framing changed; the fixed-size
   ack gained framed variants (`RegisterRejected(reason)` replaces
   `ServiceNotExist`).
3. Auth is anchored to `default_token` on each side instead of per-service
   token tables.

### Added

- **Dynamic service registration**: clients send a framed `RegisterService`
  (name, type, `remote_bind_addr`, pool size) right after authentication;
  the server validates against its policy, binds the endpoint eagerly, and
  acknowledges — port conflicts surface as precise rejections delivered to
  the client.
- **Server-side policy knobs**: mandatory-for-registration `allow_ports`
  whitelist (empty/missing rejects *all* registrations), explicit listing
  required for privileged ports (<1024), optional `max_pool_size` clamp.
  Rejections are terminal for that service run: the client logs the server's
  reason once and stops retrying until config/restart.
- **Per-service tuning**: `pool_size` (default 8 TCP / 2 UDP),
  `udp_buffer_size` (default 2048, up to 65535 — wire-compatible),
  `udp_idle_timeout` (default 60s), `udp_sendq_size` (default 1024).
- **Multiplexing**: one tunnel connection per service carries every data
  channel as a yamux stream. The `multiplex` feature is part of the default
  feature set and `mux = true` is the default. rust-yamux auto-tunes stream
  receive windows towards the bandwidth-delay product; tunable via
  `mux_receive_window` / `mux_max_streams`. The initial end-to-end stall was
  traced to yamux's lazy outbound-stream SYN (a read-only pooled stream never
  emitted its first frame); the client driver now sends a zero-length SYN
  kick, with a read-first regression test and a full
  `{tcp,tls,noise,websocket} × {mux±}` integration matrix. `mux = false`
  keeps the one-connection-per-channel path available.
- **Container image parity**: the release musl builds and the scratch image
  now include multiplexing, and the publish workflow smoke-tests the pushed
  image (`--help` plus manifest inspection).
- **Colored, span-aware logging**: level-coded colors (ERROR red, WARN
  yellow, INFO green, DEBUG cyan, TRACE purple), visible span context
  (`handle{service=ssh}:`) on every line, ANSI only on TTYs (`NO_COLOR`
  respected), source target appended at debug/trace.

### Performance

- Loopback benchmark vs v0.6.4 and frp 0.71.0 (plain TCP, same machine):
  throughput is loopback-saturated and statistically identical across all
  tools (~64 Gbit/s aggregate); the differentiator is connection-path
  latency — echo RTT p50 **0.249 ms** with mux enabled vs **0.380 ms** for
  frp (~35% lower), p99 **0.319 ms** vs **0.594 ms** (~46% lower). Chart in
  the README; raw data and reproducible scripts in `benches/scripts/bench/`.
- With `multiplex` enabled (default), concurrent visitors no longer pay a
  TCP(+TLS/Noise) handshake per data channel, and steady-state file
  descriptors drop from one-connection-per-channel to a single tunnel.
- Memory comparison added: average RSS (server + client) sampled under
  loopback iperf3 load — molehill 0.7.0 mux **16.9 MiB** (35.8% of frp),
  mux=off 16.8 MiB (35.7%), v0.6.4 16.5 MiB (35.0%), frp 47.1 MiB.
  `run_bench.sh` now records RSS and the chart includes the memory panel.

### Changed

- Service endpoints bind eagerly at registration time so conflicts surface
  immediately as rejections instead of pool retry loops.
- Hot reload of client services registers/unregisters over existing control
  channels instead of tearing down physical connections.
- UDP oversized-datagram drop threshold follows the receiver's configured
  `udp_buffer_size` instead of a compile-time constant.
- CI now also checks the no-hot-reload `server,client`-only feature
  combination (clippy + lib tests) and runs integration tests serially to
  avoid timing flake between the TCP and UDP suites.
- CI minimal-size job now looks for `target/minimal/molehill` (the custom
  Cargo profile's actual output path), fixing the size step failure.

## [0.6.4] - 2026-08-25

### Added

- Client-side health check (`health_check`) for TCP services: the client probes the local service and, after `max_failed` consecutive failures, drops the service's control channel so the server stops serving it and visitors fail fast; the service is re-registered automatically once it recovers. Supports `tcp` and `http` probe types with configurable `interval`, `timeout`, `max_failed`, and `http_path`
- `HANDOFF.md` now holds all planned work and future design documents (data-channel multiplexing, configurable pool sizes, QUIC transport, buffer pooling, single control channel, and the former README planning items); the README planning section was removed in favor of it

### Changed

- TCP_NODELAY is now enabled by default on every leg of a forwarded service (both ends of data channels, visitor-facing sockets, and the client's connection to the local service) instead of only on the outer transport connections; the per-service `nodelay` option still allows opting out
- TCP keepalive (20s/8s) is enabled by default on data channels and visitor sockets, so pooled idle channels that were silently dropped by NATs/middleboxes are detected instead of being handed to visitors
- The bidirectional TCP copy buffer is raised from tokio's 8 KiB default to 32 KiB per direction (`copy_bidirectional_with_sizes`)
- UDP datagrams are now framed into a reused buffer and emitted with a single write (one TLS/Noise record per packet) instead of three writes with per-packet heap allocations; the server's receive path no longer allocates per packet
- UDP packets larger than the 2048-byte buffer are now dropped in-stream (the channel stays usable) instead of tearing down the whole data channel

### Fixed

- The UDP connection pool logged "Failed to run TCP connection pool" as its error context
- Upgraded `h2` to 0.4.16 to fix RUSTSEC-2026-0258 (unbounded empty DATA frames; `h2` is pulled in by the optional console feature's hyper/tonic chain)

## [0.6.3] - 2026-08-05

### Added

- Strict lint rules in `Cargo.toml`: `unwrap_used`, `expect_used`, `panic`, `dbg_macro`, and `undocumented_unsafe_blocks` are denied; `unsafe_code` is warned
- Complete configuration example covering every option in `examples/full/`
- Container deployment examples (Docker/Podman Compose and Podman Quadlet) in `examples/container/`
- Startup log banner with version, git describe, and target triple; timestamps on log lines
- Detailed usage guide, usage notes, and troubleshooting section in `docs/configuration.md`

### Changed

- Container image is now assembled from the release build's static musl binaries on a `scratch` base (~8 MiB) instead of compiling inside the builder stage
- x86_64/aarch64 musl release artifacts now build with the full rustls feature set (static, OpenSSL-free)
- CI updated to `actions/checkout@v7` and the `stable` toolchain
- Client logs show the service name and remote address instead of a hex digest
- `docs/transport.md` and `docs/internals.md` rewritten to match the current implementation (Noise PSK, connection pooling, heartbeat, hot reload)
- README restructured with a Deployment section and a documentation index; `README.zh.md` synced to the new structure
- Updated dependencies (anyhow, vergen, tokio, clap, openssl, etc.)

### Fixed

- Config validation now rejects proxies without host/port and `remote_addr` without a port at startup instead of panicking at runtime
- UDP packet headers declaring a length above the receive buffer are rejected instead of causing oversized allocations
- TCP and UDP integration tests no longer share exposed ports, eliminating flaky parallel test failures
- Release packages now include the Chinese README (`README.zh.md` instead of the non-existent `README-zh.md`)
- Expired TLS test certificates regenerated
- Removed regenerable TLS key files from the repository (kept in `.gitignore`)
- Log message typos (`Shutting down gracefully`, `identity`)

## [0.6.2] - 2026-05-02

### Added

- Added Noise PSK (pre-shared key) support with configurable `psk` and `psk_location` fields
- Added UDP pool load balancing: each data channel gets its own worker task sharing the same socket via `JoinSet`
- Added unit tests for protocol message serialization/deserialization (`Hello`, `Auth`, `Ack`, `ControlChannelCmd`, `DataChannelCmd`, `UdpTraffic`)
- Added cargo test step for non-macOS ARM targets in CI pipeline
- Added `prefer_ipv6` config option at both client-level and per-service
- Added benchmark documentation section in `CLAUDE.md`
- Added `MaskedString` and Proxy Support documentation in `CLAUDE.md`

### Changed

- Pinned CI Rust toolchain to `1.95.0` via `dtolnay/rust-toolchain@master`
- Updated `README.md` and `README.zh.md` with Noise PSK, `prefer_ipv6`, and WebSocket transport documentation
- Expanded `CLAUDE.md` with source structure details, connection pooling, and build profile descriptions
- Fixed typo in TLS config example: `pkcs12 = "identify.pfx"` → `pkcs12 = "identity.pfx"`

### Fixed

- Resolved deferred TODO items: Noise PSK support and UDP pool load balancing are now implemented

## [0.6.1] - 2026-05-01

### Added

- Added `src/common/`, `src/config/`, `src/core/` submodule structure with module re-exports
- Added embedded (noise-only) test run and binary smoke test (`--help`) to CI pipeline
- Added `molehills.service` (server systemd unit file)
- Added `should_retry_accept()` helper for transient resource exhaustion errors (EMFILE, ENFILE, ENOMEM, ENOBUFS)
- Added safety documentation for `MultiMap` unsafe code blocks

### Changed

- Reorganized source tree into `src/common/`, `src/config/`, `src/core/` submodules
- Upgraded `toml` from 0.5 to 1.0, `sha2` from 0.10 to 0.11, `rand` from 0.8 to 0.10, `async-socks5` from 0.5.1 to 0.6.0
- Upgraded `vergen` from 8 to 10.0.0-beta.8 with separate `vergen-gitcl` crate
- Migrated `base64` API from top-level functions to engine-based API (`base64::engine::general_purpose::STANDARD`)
- Updated `build.rs` for vergen 10 API
- Upgraded GitHub Actions from node20 to node24 (`actions/checkout@v6`, `upload-artifact@v7`, `download-artifact@v8`)
- Moved `panic = "abort"` from release profile to dev profile
- Switched nonce generation from `rand::thread_rng().fill_bytes()` to `rand::rngs::SysRng::try_fill_bytes()`
- Renamed systemd example files from `rathole*` to `molehill*` with corrected service descriptions and mode flags
- Updated `CLAUDE.md` with improved command examples, source structure documentation, and build profiles

### Fixed

- Fixed CI toolchain alignment with project and multi-arch Docker build (removed `--locked` flag)
- Fixed `cargo publish` with `--allow-dirty` for Cargo.lock drift in CI
- Fixed systemd service flag assignments (server/client mode flags were inverted)
- Fixed `fdlimit::raise_fd_limit()` ignored return value warning

### Removed

- Removed stale FIXME comment in UDP data channel code
- Removed `async-trait` dependency (resolved upstream in `async-socks5` 0.6.0)
- Removed old `ratholes@.service` systemd file (replaced by `molehills.service`)


## [0.6.0] - 2026-04-23

### Added

- New project branding: renamed from `rathole` to `molehill` with new logo and updated documentation
- Added `justfile` with `just check` command chain (cargo check, clippy, fmt, audit, machete)
- Added `CHANGELOG.md`
- Added `rust-toolchain.toml` pinning Rust 1.95.0
- Added `CLAUDE.md` project documentation for contributors
- New CI workflow `ci.yml` with cargo audit, cargo machete, and cargo-hack feature-powerset checks
- Enhanced `release.yml` with GHCR Docker publishing and automatic changelog extraction

### Changed

- Upgraded Rust edition from 2021 to 2024
- Upgraded Rust toolchain from 1.71.0 to 1.95.0
- Upgraded `clap` from 3.x to 4.x with derive features and `ValueEnum`
- Replaced `backoff` with `backon` 1.6 for retry logic
- Replaced `bincode` with `postcard` for serialization
- Upgraded `tokio-rustls` from 0.24 to 0.26
- Upgraded `rustls-native-certs` to 0.8
- Upgraded `vergen` from 7 to 8 with `gitcl` backend
- Updated `build.rs` for vergen 8 API
- Updated author and description metadata in `Cargo.toml`
- Reformatted `README.md` and `README-zh.md` with centered layout, badges, and fork attribution
- Updated all internal references from `rathole` to `molehill`

### Removed

- Removed `.rustfmt.toml` (nightly-only `imports_granularity` incompatible with stable)
- Removed outdated documentation: `docs/benchmark.md`, `docs/out-of-scope.md`, and `docs/img/` directory
- Removed old `rust-toolchain` file (replaced by `rust-toolchain.toml`)
- Removed old CI workflow `.github/workflows/rust.yml` (replaced by `ci.yml`)
- Removed `atty` dependency (unmaintained)
- Removed `rustls-pemfile` dependency (functionality merged into rustls)
