# HANDOFF: Working State & Future Work

> **State as of 2026-10-01.** **v0.10.0 is released.** PR #4 merged into
> `main` (merge commit `dffd7d8`), the tag `v0.10.0` published a GitHub
> Release, the GHCR image and crates.io, and the branch that carried the work
> (`feat/session-and-pool`) is deleted — `main` is the only line now. The theme
> is **M1** (one control session per endpoint, protocol v4), **M2a** (one
> shared elastic pool per carrier, plus the S1 observation), **M6** (the
> configuration surface) and **M7** (`direct`'s role, measured).
>
> What is left is not code: the repository-settings items a human has to make,
> and the three open threads at the bottom of this page.
>
> **This file owns the working state**: what is open, what was decided and why,
> and the release checklist. Per AGENTS.md §3 it is a contributor page —
> user-facing facts belong in the docs pages, and anything released belongs in
> CHANGELOG.md. The cycle's closed records (incidents, superseded sweeps,
> measurement records, retracted diagnoses) live in
> [HANDOFF-archive.md](HANDOFF-archive.md), which this page indexes; the
> archive was split out on 2026-09-30 because the live state had become a third
> of a 1,600-line file.
>
> Shipped work: [CHANGELOG.md](CHANGELOG.md). Design:
> [docs/internals.md](docs/internals.md). Method and how to read the numbers:
> [docs/benchmarks.md](docs/benchmarks.md).

## Where this stands

- **An unreleased cycle is in flight** on `feat/transparent-l3`: transparent
  (L3) services, wire **v5**, based on `main` at `cf01091`. Nothing of it is in
  `v0.10.0` (verified: the tag's tree has no transparent code at all), so its
  configuration surface is still being shaped in place. The decision record,
  the model it is being reshaped into, and the header-compression question are
  in "The transparent-L3 cycle" below.

- **v0.10.0 shipped on 2026-10-01**: the release run is green on all thirteen
  jobs (nine platform builds, GitHub Release, GHCR, crates.io), and the
  released artifact is `benches/records/results-soak-v0.10.0.json` — measured
  at `96bda00` with the model the bench model replaced, and gated by the
  sweep's own check with no waiver. That schema is history now: the next
  release publishes `results-bench-vX.Y.Z.json` and gates it with
  `just bench-gate`.
- **The release cost four tag pushes, and the three failures are worth
  remembering**: the emulated cross legs (`native_target` guard missing on a
  new spawn-based test), the musl leg (an assertion pinned to glibc's wording),
  and the `aarch64` cross leg (the release matrix's cache shared across legs
  with different glibc). The first two are in `tests/` — which forced the sweep
  to be re-run on the commit that carries them — and the third is in
  `release.yml`. All three are fixed; the open thread at the bottom of this
  page proposes the check that would catch the next one mechanically.
- **The repository state**: `version = "0.10.0"`, `CHANGELOG.md` carries
  `## [0.10.0] - 2026-10-01` with `[Unreleased]` empty, and the release tag
  `v0.10.0` names `104626f`.
- **Only docs and assets may follow a sweep.** `githooks/pre-tag` reads the
  results file's recorded revision and fails the tag if `src/`, `tests/`,
  `Cargo.*`, `build.rs` or the bench model's own code changed since — the
  changelog date and the README numbers may, code may not (AGENTS.md §10,
  "prove provenance").
- **The repository-settings items are a human's to make** — branch protection
  for `main` and the release secrets. They are tracked as an open thread below;
  nothing in this repository can enable them.

## The transparent-L3 cycle (unreleased, branch `feat/transparent-l3`)

**State as of 2026-10-09.** The feature is committed and green
(`45adb6e` the feature, `edffa53` never-encrypted, `b1ce03b` the striping
note): the transparent (L3) client makes itself the owner of a public
`ip:port`, carried as whole IP packets over a TUN device on both ends, with the
wire at **v5** and `just l3-accept` (root-only, outside the check chain)
proving the transparency end to end. Because none of it is released, the
configuration surface is free to change — and this cycle changes it.

### Decisions

| # | Decision |
|---|---|
| L1 | L3 is a **run mode**, not a service type: touching a NIC is a process-level capability, so an entry-level key must not be what opens it |
| L2 | The mode is chosen by its own top-level table (`[transparent]`), exactly as `[server]`/`[client]` are; a CLI flag only overrides |
| L3 | `[client]` and `[transparent]` in one file is `Undetermine`: two roles are two processes (capability and restart isolation) |
| L4 | The server gets **no** mode. It is a policy host: one config serves encrypted L4 clients and plaintext L3 ones |
| L5 | `[server.transparent]` **is** the server's switch: absent, a registration is refused by policy *before* any device is looked at, so a client can never be what makes the server reach for `/dev/net/tun` (shipped first in this cycle) |
| L6 | **No** `allow_addresses`: one shared `default_token` is one trust domain, so a global address list cannot discriminate between clients; the address universe is already expressed by the operator's own routing |
| L7 | Noise and L3 are **not** mutually exclusive — the Noise keys simply have no effect on an L3 client. The documented consequence: enabling L3 means that server accepts plaintext connections from it |
| L8 | `allow_ports` keeps gating L3 too: it is documented as the master switch for dynamic registration, and an L3 claim is a registration |
| L9 | A claim's address key stays `remote_bind_addr`, and `proxy` stays in `[transparent.transport]` (the only key there — no encryption key has a home in that table) |
| L10 | Header compression is an **intermediate state**: keep it when the measurement shows benefit, remove it when it does not, against criteria written down *before* the run |
| L11 | No startup INFO for the switch; the plaintext consequence lives in the docs instead |

### Next, in order

1. ~~**C — the `[transparent]` run mode and its config model.**~~ **Done**: the
   `[transparent]` block, `[transparent.claims.<name>]`, `RunMode::Transparent`
   with `--transparent`, the `[client.transparent]` and
   `protocol = "transparent"` redirects, and the nine per-key refusals deleted
   (those keys have no home in the schema). The lowering is a front end: the L3
   block becomes the same `ClientConfig` the forwarding path already runs on, so
   `core/client.rs`, the data path and the wire were not touched. Validation
   takes the model as an argument (`ClientModel`) rather than being forked, and
   its messages follow it — a claim is a *claim*, and the block named is
   `[transparent.control]`, not `[client.control]`.
2. ~~**M — measure the L3 path before writing a compressor.**~~ **Done**
   (2026-10-09, `9507a7a`): the harness samples `/proc/net/dev` per arm and
   `benches/scripts/l3/wire_report.py` reports carried packet sizes and a
   header-compression ceiling. Campaign in `docs/benchmarks.md`, "The
   transparent-L3 wire question".
3. ~~**Z — only if M passes.**~~ **Not built: the measurement says no.** The
   ceiling on the workload compression exists for (2000 round trips of 64 B) is
   **17.9 % of the wire** — under the 20 % bar written before the run, and an
   upper bound before exemptions. The same table says why: 196 B of wire per
   116 B carried packet, i.e. ~80 B of tunnel transport per packet (our 2-byte
   length, the multiplexer's frame, the carrier's TCP/IP header and its ACKs)
   against ~35 B of compressible header. **Framing is the lever, not headers**;
   batching several packets per frame is the next candidate and needs its own
   measurement.
4. ~~**V — the verdict, before any `v0.11` tag.**~~ **Recorded above, and it is
   a no**: nothing was added to the wire, so there is nothing to remove and no
   v6 risk.
5. **Done after M: the L3 data path was optimised where the measurement pointed.**
   Batching (drain the device, frame each packet where it is read, hand a run of
   frames over as one write) took 12 % off the bulk arm's wire bytes; the L3
   model's `[transparent.data].default_mode` is now `direct` (a claim has one
   channel, so the multiplexer was pure per-packet cost), worth another 6 % of
   the wire, a third of the CPU per packet and 65 % more small round trips per
   second. Numbers and method in `docs/benchmarks.md`, "The transparent-L3 wire
   question". What the same instrument now says is that **CPU, not the wire, is
   the next lever** (~135 µs of daemon time per carried packet on the small
   arm), and that it **cannot yet answer the many-flow case**: every arm is one
   connection, so parallel TUN queues (`IFF_MULTI_QUEUE`, one reader per queue)
   are a candidate that needs a concurrent-flow arm before it can be claimed
   either way. That is the next measurement, not the next feature.

   **Both of those are now answered, and neither is the next thing to build.**
   The harness gained a multi-flow arm *and a control beside it* (the same flows
   with the tunnel out of the path): at 16 connections the tunnel carries
   19 900 round trips/s against the control's 23 750 (84 % of a ceiling that is
   the probe's own), at 64 connections 27 700 against 30 900 (90 %). So parallel
   TUN queues have no measured need — the device reader is not what limits a
   busy host, and a control is what makes that statement checkable instead of
   plausible. Bulk is the arm where the path *is* the limit (338 Mbit/s against
   the control's 602), and it moved a long way by batching the read side as well
   as the write side (+27 % throughput, −56 % CPU per packet).

   **The remaining levers were then attributed, and none of them is a code
   change.** Two instrument errors had to be fixed first, and both are worth
   remembering: the bulk arm sent one blob and read afterwards, so any run past
   the buffers measured a *deadlock* (two runs of 20 MB and 100 MB were reported
   as throughput when they were five-second stalls), and the harness built
   **debug** while the product ships release — the same arm measures 783 Mbit/s
   in debug and 1986 Mbit/s in release. Both are fixed (`BULK_CHUNK`,
   `PROFILE_BUILD=release`), and the numbers below are release.

   Release, one host, with the no-tunnel control beside each arm: bulk 200 MB in
   65 KB chunks **1986 Mbit/s** against the control's 12 024; a paced single flow
   **10 362 round trips/s**; 16 flows **22 543** against the control's 23 388 —
   **96 % of a ceiling that is python's, not the path's**.

   A `perf` profile of the release build during the bulk arm puts **84 % of the
   CPU in the kernel**, 10 % in molehill, 5 % in libc, with no symbol above 11 %:
   the cost is four syscalls per packet (TUN read, socket write, socket read, TUN
   write), spread thin. That retires the synchronous data path as a plan — it
   would chase the userspace tenth, not the kernel's five sixths — and it names
   the lever that does work: **packet size**. The same bulk bytes in 8000-byte
   packets instead of 1400 measured **3579 Mbit/s** at half the CPU per byte, so
   the TUN MTU (an operator setting, with a link MTU to match) is worth more than
   any remaining code change, and the deployment recipes now say so.
6. **One model, and it owns the sweep too (2026-10-10, unreleased).**
   `benches/scripts/bench/` is now the only runner: the soak sweep, the
   L3-versus-L4 comparison, the memory sampler, the HTTP latency script and the
   peer adapters are all *this* model — its profiles, its scenarios, its arms.
   What it grew in the migration:

   - **peer arms** (`peers.py`): frp, rathole and nps are arms like any other,
     configured per topology, with their release binaries fetched by
     `just bench-peers` and their versions recorded in the results meta. The
     first end-to-end peer run (one condition, `cost`, 60 s) read frp at
     6.08 Gbit/s and molehill at 13.58 Gbit/s, with the p99 and CPU-cost
     metrics beside them — the four-tool comparison, on the model's terms.
   - **conditions and timelines** (`model.CONDITIONS`, `model.TIMELINES`):
     `clean`, `rtt100`, `loss1`, `loss5`, `rate100`, `rate20`, `jitter` and the
     two MTU classes, imposed *in place* on a veth leg (`tc qdisc replace`), so
     the tool keeps running while the path changes. `--scale` shortens a
     timeline's holds without changing its shape, and the scale is part of the
     fingerprint.
   - **the staged scenario** (`timeline`, `soak`, `cost`): a bulk spine
     re-dialed per stage after a drain predicate (queue below a tolerance and
     no send-capable socket, held for two polls, with a budget that is recorded
     when it fires), three continuous series probes (interactive, datagram,
     churn) sliced per stage, a drift sampler (RSS/fds/threads) across the
     whole run, and wedge detection (>5 s of silence).
   - **the capacity ramp** and **reconnect**: one cell per stream level with a
     fresh-connection ping beside it and the SLO verdict per level, and five
     timed cold starts.
   - **`plot.py`**: the six figures (timeline, stages, capacity, UDP, drift,
     cost) and five markdown tables, rendered from the new schema, each figure
     carrying its method in the footer.
   - **`bench.py gate`**: coverage of every declared cell, the endpoint
     invariant, the SLO on the clean stages (the product's arms only — a peer
     that misses it is reported, never blocked), the drift and wedge axes (a
     slope is only judged past five minutes), the capacity ramp, and the
     regression half against a baseline.

   The retired tree (`benches/scripts/soak/`) is deleted; its published records
   moved to `benches/records/` as history, and the release ritual now names
   `results-bench-vX.Y.Z.json` + `assets/bench-vX.Y.Z*.png` and gates with
   `just bench-gate`. The model is self-contained: the iperf3 conventions
   (`iperf.py`), the provenance probes (`hostinfo.py`) and the peer adapters
   all live in it, with no import from the retired sweep.

   Not migrated, deliberately, with the reason: the **slow visitor** probe
   (opt-in head-of-line instrument; the model's UDP and TCP shapes cover the
   load shapes, and a HOL question would come back as a scenario), and
   `udp_stress.py` (it reads molehill's *internal* UDP queue counters to answer
   a molehill-only design question, which the model's externally-observable
   rule forbids in a comparison — it stays as a diagnostic, outside the model).

   What is still open: the sweep has not been run end to end on the migrated
   engine (the pieces are validated: a staged timeline over three arms, the
   capacity ramp with the SLO per level, a peer arm, the drift and wedge
   metrics, the figures from a real staged file), so the first release sweep is
   also its acceptance test. The 5 Gbit/s UDP ladder cell and the loopback
   ceiling remain the two places where a single run cannot order the arms.

7. **The carrier axis and the L3 data path, measured (2026-10-10, unreleased).**
   The hypothesis on the table was "L3 + KCP is better overall, and the
   multiplexer should not cost much". The model answered both, and the answers
   are *no* and *it costs 28 %* — with one exception each way, and with the
   real L3 lever showing up in the same campaign.

   **The carrier is now independent of the mode** (`08e72aa`): a `direct`
   service or claim can ride KCP — one session per channel, no yamux above it —
   where the config used to refuse the pair. The server's KCP listener reads the
   data-channel hello beside the tunnel hello, the client dials the carrier its
   service declared, and the model gained the axis to measure it (`fd4c9aa`):
   arms `l3` / `l3-mux` / `l3-kcp` / `l3-mux-kcp` / `l4-kcp`, the carrier in the
   generated config and in the evidence, and the condition `loss1_rate100` (a
   100 Mbit/s, 20 ms, 1 %-loss leg — the lossy WAN the choice is about).

   What the model measured (`--profile smoke`, two rounds, A/A twin in every
   run, conditions on the **tunnel** leg, tables in
   [docs/benchmarks.md](docs/benchmarks.md#the-carrier-axis-tcp-versus-kcp-2026-10-10-this-model)):

   | Condition | control | tcp carrier | kcp carrier |
   |---|---|---|---|
   | clean, `l3` (direct) | 41.8 | 4.17 Gbit/s, 0.855 s/Gbit | 2.19 Gbit/s, 2.554 s/Gbit |
   | clean, `l3-mux` | — | 2.99 Gbit/s, 1.435 | 2.51 Gbit/s, 2.310 |
   | `loss1`, `l4` / `l4-kcp` | 4.43 | 2.51 | 0.40 |
   | `loss1`, `l3` / `l3-kcp` | 4.43 | 2.03 | 0.41 |
   | `loss1_rate100`, bulk | 0.096 | 0.093 (`l4`), 0.089 (`l3`) | 0.059 (`l3-kcp`), **failed 3/3** (`l4-kcp`) |
   | `loss1_rate100`, `rr-16` | 323.2/s | 328.0 (`l4`), 335.3 (`l3`) | **337.8 (`l4-kcp`)**, **338.6 (`l3-kcp`)** |

   Read: the multiplexer costs **28 %** of the bulk throughput and 68 % more CPU
   per byte; KCP costs **48 %** and 3× the CPU per byte *on a clean path*; under
   loss KCP is **five to six times behind TCP** in both architectures; and on a
   rate-limited lossy leg the multiplexed KCP arm does not finish its test at
   all — `KCP session dead link`, which kills the visitor's connection with it.
   The mechanism came out of the carrier's own counters
   (`MOLEHILL_KCP_STATS=1`): **22 % of the datagrams it sent were
   retransmissions**, because with `nc=1` there is no congestion control and the
   pacer's only signal is a PONG that does not arrive within 2.5 s — a
   window-sized burst goes into whatever queue the path has, the standing queue
   delays the acks past the escalating RTO, and the retransmissions enlarge the
   queue until the ARQ declares a live-but-rate-limited peer dead.

   **Both obvious fixes were tried and both are wrong.** `nc=0` (KCP's own
   congestion control) as an A/B against the shipped build on `loss1_rate100`
   measured **0.002 Gbit/s against 0.043** on bulk and 256/s against 344/s on
   `rr-16` — KCP's built-in control collapses the window on a lossy path and
   never recovers. And capping the send window to the path's BDP (2048 → 256
   segments, `set_wndsize`) left bulk where it was (**0.039 against 0.044**) and
   the multiplexed arm still died. The counters say why a knob cannot do it: on
   that leg the sender retransmits **24 % of its output** against a 1 % loss
   rate, so the recovery loop itself is the defect — it needs a controller
   driven by what the path delivers (delivery rate and/or RTT inflation), in the
   adapter, with the `loss1_rate100` A/B as its gate. The engine exposes
   `set_wndsize`/`wait_snd` but not `snd_una`/`rx_srtt`, so such a controller
   starts with two accessors. The one place KCP won is worth keeping in view: on the same leg,
   many short interactions (`rr-16`) were fastest on both KCP arms with the best
   p99 — fast retransmit pays, and it is the bulk path where the missing control
   costs more.

   **The larger finding is about L3 itself.** The same campaigns measured the
   data path against the forwarding path on one host
   ([table](docs/benchmarks.md#the-l3-data-path-what-limits-it-2026-10-10-this-model)):

   | Arm | `bulk-1`, TUN MTU 1400 | `bulk-n` (8 flows) | `bulk-1`, TUN MTU 8000 |
   |---|---|---|---|
   | `l3` | 4.34 Gbit/s | 3.93 | **8.00** |
   | `l4` | 8.99 | 22.33 | 8.00 |

   Two conclusions, both actionable. **L3 does not scale with flows** (eight
   flows move less than one) — the ceiling is a serialized per-packet path, not
   a window, so more connections through a claim cannot raise it. And **packet
   size is the lever**: an 8000-byte TUN MTU (link MTU to match) took the same
   flow to 8.00 Gbit/s (+84 %) at a **third** of the CPU per byte, matching the
   forwarding path at one flow. That is an operator setting, and
   [deployment.md](docs/deployment.md#transparent-services) now carries the
   number.

   At that packet size a second measurement says *what* the remaining ceiling
   is — the forwarding path with its pool capped, so that one arm prices one
   carrier connection:

   | Arm (TUN MTU 8000) | `bulk-1` | `bulk-n` (8 flows) |
   |---|---|---|
   | control | 38.672 | 44.805 |
   | `l3` | 7.136 | 5.847 |
   | `l4` (pool ≤ 4) | 8.286 | 21.604 |
   | `l4-mux1` (one tunnel) | 9.819 | 12.646 |
   | `l4-mux8` | 9.058 | 21.955 |

   One carrier connection tops out near 8–10 Gbit/s here (the one-tunnel arm
   says so at one flow *and* at eight streams over that tunnel), and the
   forwarding path passes it only by spreading streams over several tunnels. A
   claim has exactly one channel, so it sits at the per-connection ceiling by
   construction — that is why its `bulk-n` is below its `bulk-1`. Going past it
   is a **design** decision (per-flow sharding: several channels per claim, one
   per inner flow), not a tuning one, and the model cannot price it before it
   exists.

   Open, in the order the measurements argue for them:

   - **A device path that carries more than one packet per syscall** — chosen
     as the next lever (2026-10-10), and **the kernel half is now verified**
     (spike outside the repo, same host):
     - attaching to an operator-created device with `IFF_VNET_HDR` works, and
       so do `TUNSETVNETHDRSZ` (its argument goes **by pointer**) and
       `TUNSETOFFLOAD` (its argument goes **by value** — mixing the two
       conventions returns `EINVAL` for every mask, which cost an hour of the
       spike);
     - `TUN_F_CSUM|TUN_F_TSO4|TUN_F_TSO6` is accepted, so the device advertises
       TSO/GSO;
     - one 54 918-byte **USO** write (`gso_type = UDP_L4`) was delivered as
       **40 datagrams of 1372 B** to a local socket: one syscall, forty packets,
       segmented by the kernel;
     - one 54 440-byte **TSO** super-packet was accepted and crossed a veth as
       *one* 54 KB skb (the virtual link preserves GSO), so segmentation is
       deferred to wherever it is really needed.

     Two more things the same day's experiments settled, both of them reasons
     the header does **not** land on its own:

     - **The offload flag is two-way, and enabling it alone breaks every
       claim.** With `TUN_F_CSUM` on and the virtio header dropped on the floor,
       no visitor could reach a claim at all (measured: both the framing run and
       the readiness probe failed for every round). The reason is the topology's
       own veth: it hands over packets whose transport checksum is still
       partial, the device advertises that it may do so, and a daemon that
       forwards the packet without its flags forwards something the far end can
       only drop. The header's `flags`/`csum_*` fields have to travel with the
       packet — which is the wire change, not a local one.
     - **The header without the offloads is a pure cost.** Attach with
       `IFF_VNET_HDR` only (offloads off, packets complete, everything still
       working) measured **4–5 % below** the released build on `bulk-1`: three
       rounds, the A/A twin beside it, every round below both old arms
       (4.041 against 4.188/4.255 Gbit/s). The header costs a 10-byte copy on
       read and a two-segment `writev` on write, and buys nothing until the
       writes coalesce. So it lands *with* the GSO write, as one change whose
       A/B has a win to show.

     **What is still unverified** is the half the win depends on: whether the
     kernel *coalesces* on the read side. The spike could not answer it — a UDP
     stream arriving on a veth is fragmented, not GRO-able, and the reads came
     back one packet each with the header all zeroes (10 169 reads of 1406 B,
     no `gso_type`) — so the TCP case has to be measured where TCP flows exist:
     in the bench, with the L3 arm at 1400 and at 8000, watching
     `mean_carried_packet_b` (a frame above the MTU is a coalesced read) and
     `syscalls_per_s`. If the read side never coalesces, the win is only the
     inject side's, and the design has to coalesce a run in userspace — which is
     why the two slices below are ordered that way.

     The design that follows, in two slices:

     * **Slice 1, no wire change.** Both TUNs attach with `IFF_VNET_HDR` and
       enable the offloads; the *inject* side groups consecutive packets of one
       flow (same 5-tuple and direction, consecutive TCP sequence numbers or
       equal-size UDP payloads, no SYN/FIN/RST inside the run, DF set) and
       writes the run as one GSO packet, falling back to one write per packet
       whenever the run does not qualify or the device has no offloads. The
       frames on the wire stay one packet each, so nothing needs a version bump
       and an old peer keeps working.
     * **Slice 2, with a wire change.** The virtio metadata travels in the L3
       frame, so a super-packet read from one TUN is carried as one unit and
       injected as one write at the far end (and the reverse direction too).
       This is a protocol change (v6) and is only worth it after slice 1 has
       been measured.

     Measurement plan, all of it on the model as it stands: the L3 arms
     (`l3`, `l3-deep`) at the default 1400-byte TUN MTU and at 8000, on
     `bulk-1`/`bulk-n`/`rr-16` and on `udp-pace` (`bytes_per_syscall` and
     `syscalls_per_s` are the readings that must move — they are device-I/O
     metrics, so an L3 arm is exactly where they are valid), with `l3-deep`'s
     deeper queue as the control for "is it syscalls or queueing". Success is a
     throughput gain the run's own A/A floor can see, at no cost to `rr-16`'s
     latency or the wire ratio.
   - **The datagram size follows the path — landed, measured, kept.** The
     carrier had been pinned at 1400 bytes with a shrink-only adaptation, so
     every jumbo path paid the per-datagram cost 5.7× more often than it had
     to. `Kcp::set_mtu` (was `shrink_mtu`) now follows the caller in both
     directions, the adapter computes the size from the kernel's path-MTU
     answer up to an 8 KiB ceiling (`datagram_for_path`), re-reads it once a
     second, and every inbound buffer is sized from the *ceiling* rather than
     from the engine's default — the spike's first jumbo attempt arrived as a
     retransmit storm because `DGRAM_BUF` was 2048 against 8000-byte segments.
     A/B against the previous build, two rounds an arm, `l3-kcp`, one bulk
     flow: clean jumbo **2.170 → 3.026 Gbit/s (+39 %)** at −28 % CPU per Gbit
     and a better wire ratio; `loss1` jumbo **0.407 → 1.713 (+4.2×)** at −40 %
     CPU; `loss1` on a 1500-byte link **neutral** (0.406 → 0.434), which is the
     safety property — the probe keeps a normal path at the size it carries;
     `rr-16` on `loss1_rate100` +3.8 %.
   - **The dead link was the rule, and it is fixed.** The engine inherited the
     reference's dead-link rule — twenty retransmissions of one segment ends the
     session — which conflates a peer that is gone with one that is slow. On a
     rate-limited path the acknowledgements queue behind the shaper, so a
     segment collects twenty retransmissions while the peer answers everything
     else, and a live session is closed. The rule now needs the count *and* a
     send window that has not moved for five seconds (`KCP_DEAD_GRACE_MS`, with
     `last_progress` set wherever `snd_una` advances). Measured on
     `loss1_rate100` + a jumbo link: the multiplexed arm failed `bulk-1` in
     **every** round before and completes **both** after (0.058–0.059 Gbit/s,
     17 % fewer wire bytes than the direct arm), and the direct arm's CPU per
     byte fell by a fifth. The one cost: a genuinely vanished peer is now
     detected up to five seconds later, which the session-level watchdogs
     (`FORWARD_IDLE_TIMEOUT`) bound anyway.
   - **The collapse was the pacer, and it is fixed.** The trigger turned out to
     be blunt: in a session's lifetime, the *first* cell of a turn runs at full
     speed and *every later* cell runs at a fraction, whatever the scenario
     (`bulk-n,bulk-n` 6.18 → 1.00; `bulk-n,bulk-1` 5.86 → 1.08; `bulk-1,bulk-n`
     7.1 → 2.0). The state was the adaptive pacer's allowance, visible once a
     gauge for it was added: 12 → 9 → 6.75 → 5.06 → 3.8 Gbit/s across the first
     cell's PONG timeouts, held for the rest of the session because the recovery
     is 5 % per four clean PONGs. A PONG that is late because the peer is busy
     sending is not congestion, so `PaceState::on_ping_timeout` now cuts only
     when the send window has not moved since the PING (`Kcp::snd_una`, sampled
     at PING time). Measured after the fix: `bulk-n,bulk-n` **7.79 then 7.67**,
     `bulk-1,bulk-n` **7.19 then 7.44** — no collapse — while `loss1` jumbo
     stays at 1.67–1.71, `rtt100` is 0.169 → 0.182 and `loss5` is unchanged.
     Two detours are recorded rather than hidden: a **window byte budget** (to
     stop the buffers growing 5.7× at a jumbo MTU) fixed nothing and cost the
     lossy leg two thirds of its throughput (0.45 against 1.67 Gbit/s on
     `loss1`), so it was reverted — a lossy path wants the window *wider* than
     the BDP, not closer to it; and a **1 ms ack-batching window** was −6 % to
     −12 % on the clean path and was reverted. What remains open from the
     original finding: nothing — the collapse is gone, and the numbers that
     replaced it are on the benchmarks page.
   - **L3+KCP matches the TCP carrier on a jumbo single flow and now leads it on
     eight (historical note; see the entry above for the fix).** With the segment size following
     the path and the dead link fixed, one bulk flow through an L3 claim at a
     jumbo TUN MTU (8000) and a jumbo link (9000) measures **7.265 Gbit/s
     against the TCP carrier's 7.086** in the same run (A/A twin 6.948, so both
     are inside the noise of each other) at 2.3× the CPU per byte — the
     user-facing claim "L3+KCP can be as fast as L3+TCP" now has a measurement.
     What does *not* hold is the eight-flow cell, and the trigger is
     reproducible and narrow:
     - `--arms l3-kcp --scenarios bulk-n` (alone): **5.9–6.2 Gbit/s** across
       four runs;
     - `--arms l3-kcp --scenarios bulk-1,bulk-n` (a bulk cell first): **1.99 and
       3.54 Gbit/s** in two runs — and in a 4-arm, 3-round campaign, 2.06 in
       every measured round while the TCP carrier beside it read 5.6–5.9;
     - the engine's own counters are *identical* in both cases (server
       retransmits ~15–18 % of its datagrams either way, client `ms_input`
       1.5–1.9 s, `ms_output` per datagram 7.5 µs), and the difference is the
       **bytes per datagram**: 6.2 KB when it is fast, 3.4 KB when it collapses
       — the sender is producing smaller datagrams, not more work per datagram.
     So the next step is to find why a preceding bulk cell makes the carrier
     emit half-full datagrams: the candidates are the hub's batching (it flushes
     "the moment the device runs dry", and eight flows drain the device
     differently from one) and the KCP writer's chunking downstream of it.
     Reproduce with `sudo -n uv run benches/scripts/bench/bench.py run --profile
     smoke --arms l3-kcp --scenarios bulk-1,bulk-n --condition clean
     --link-mtu 9000 --tun-mtu 8000 --rounds 1 --warmup-rounds 0`.
   - **The whole-workload sweep (`dev`, jumbo path, A/A twin) says where this
     stands**: level with the TCP carrier on one bulk flow (7.136 against 7.554
     and a twin at 7.496) and on churn (6 779 against 7 139, twin 7 209),
     **19 % ahead on eight flows** (7.418 against 6.023, twin 6.243), **behind on
     short round trips** (rr-1 10 177/s against 14 167/s, rr-16 30 264/s against
     32 984/s, p99 equal) and **about twice the inner-UDP loss** at every rung of
     the ladder (1.05 % against 0.60 % at 2 Gbit/s). The UDP loss is the one to
     chase next if quality matters more than throughput: it is the hub's
     endpoint channel dropping when the carrier's writer is slower than the
     ingress, and the loss is unrecoverable for a UDP flow.
   - **The long-haul gap is the next real target, and its mechanism is now
     measured.** `rtt100` (100 ms each way, no configured loss): TCP carrier
     0.461 Gbit/s, `l3-kcp` 0.178 — the widest gap left in the model. The
     carrier's counters put 21 % of the sender's datagrams somewhere on the
     floor and 28 % of its output into recovery, split 4 239 RTO against 2 163
     fast-retransmit (so mostly spurious timeouts). **Kept:** RTO headroom of
     `srtt/8` (`KCP_RTO_HEADROOM_DIVISOR`), worth −42 % RTO resends and a wire
     ratio of 1.20 → 1.09 with throughput flat. **Falsified:** pacing the burst
     to the BDP (`window / srtt`) is 35 % *slower* (0.114 against 0.175 Gbit/s,
     three rounds each, ranges disjoint) and does not change the wire ratio —
     the pacer's bucket-and-rate gate does not remove the losses, it only slows
     the sender. So the next attempt has to find where those datagrams actually
     die (the receiving socket's buffer, the veth queue between the namespaces,
     or the ARQ's own flush cadence) rather than throttle the sender: the
     obvious throttles have all been measured now. The diagnostics this needed
     are on the `MOLEHILL_KCP_STATS` line for good: `resends_rto` vs
     `resends_fast`, `rto_ms`/`srtt_ms`, the window state, the reader/spill
     residency and the pacer's allowance.
     Two more results from chasing it, both landed: **the drain parks what the
     pacer or the kernel refuses** instead of dropping it (worth +12 % on
     `loss1`, 1.824 against 1.627 Gbit/s with disjoint ranges, at an identical
     wire ratio) and the park is bounded by half the RTO so the ARQ does not
     duplicate what the pacer is holding. And one **trade left on the table**:
     with parking in place, capping the send rate at `window / srtt` is +21 % on
     `rtt100` (0.204 against 0.169 Gbit/s) but **+14 % wire bytes** there; it is
     measured, documented on the benchmarks page, and deliberately not carried
     until someone decides that trade. A quick follow-up worth one run: whether
     a *larger* pacer bucket (more burst allowed, less queueing delay) keeps the
     throughput and loses the wire cost.
   - **A rate-aware pacer for the KCP carrier** (above) — **attempted
     2026-10-10, measured, reverted.** The design: sample the peer's
     acknowledged progress every 20 ms, convert it to segments per second, and
     set the send window to `rate × srtt × 1.5`, never below 64 segments. Its
     A/B against the shipped build, same arm, two rounds each:
     clean **neutral** (2.174 against 2.188 Gbit/s), `loss1_rate100` **slightly
     ahead** (0.059 against 0.053 Gbit/s, 9 % fewer wire bytes), and `loss1`
     **twelve times worse** (0.033 against 0.411). The reason is the signal, not
     the intent: `snd_una` measures *contiguous* acknowledgement, so it stalls
     on every gap and the window shrinks exactly when loss needs the opposite —
     and a window too small starves fast retransmit, which is driven by the acks
     of *later* segments; with nothing in flight the sender waits out a 30 ms+
     RTO instead. Two things follow for the next attempt: the rate signal must
     come from acks (or from `KCP_SACKS_SENT`/retransmit counters) rather than
     from contiguous progress, and the window needs a loss-aware reserve with a
     hard floor under fast retransmit's needs. The shipped constant window stays
     until that exists.
   - **The churn scenario reports zeros, and only for L4 arms.** Three runs on
     2026-10-10 (`churn-16`, 2/5/10 rounds) produced four rounds with no samples
     at all and three rounds where the readiness probe could not reach the
     exposed port for its full 30 s — every one of them on an L4 arm or its
     twin, never on L3, and the probe's own log for a zero round *does* contain
     latency samples. Two candidate explanations, neither established: the
     architecture (a fresh visitor connection costs L4 a data channel and a
     pair of ephemeral ports that L3 does not spend) or the probe (which reports
     a rate without saying why connections failed). Until the probe records the
     failure reason, no churn number may be quoted — including the tempting
     reading that L3 beat L4 in both runs' means (6366/7131 against 5638/4932).
     The instrument is the thing to fix first; if the architecture is the cause,
     it is a finding worth having.
   - **What the multiplexer's 28 % actually is.** The earlier per-cell model
     measured 6 % of wire and 33 % of CPU per packet for `direct` over
     `multiplex`; this model measures 28 % of throughput and 68 % of CPU per
     Gbit. The two are not the same reading and the difference is worth
     attributing (yamux frame copies vs the pool's stream machinery) before
     anything is changed for it.

**Pre-registered criteria (kept as written, for the record).** Small packets
(≤128 B payload): wire bytes down ≥ 20 %. Mid (512 B–1 KB): ≥ 5 %. Bulk
(1400 B): throughput down ≤ 2 %. The small-packet and the bulk conditions must
both hold; a difference inside the variance counts as *no difference*, and
either failure removes the feature. The small-packet arm failed on the ceiling
alone, so the other arms were not run for the verdict.

### Open threads

- **One instrument, and the boundary is a profile (closed 2026-10-10).**
  The earlier decision — a sibling instrument for L3, the sweep left alone —
  was a consequence of the model not existing. There is one runner now: the
  sweep is the `sweep` profile, the staged schedule is the `timeline`
  scenario, the drift axis is the `soak` profile, the screen is `--profile
  screen --ab-arm`, and a peer is an arm. What stays distinct is *evidence*:
  the model writes results outside the tree, and only a run the ritual
  publishes lands in `benches/records/`.
- The benchmark comparison against v0.10.0 must build that state (worktree at
  the tag): there is no released asset that speaks v5, and no two tags are
  compatible, so each side runs as a complete pair.

## The v0.10.0 theme

One control session per endpoint, one shared elastic pool per carrier,
transparent visibility and a quiet log. Four of the milestones were independent
and are already merged on `main` (unreleased, so they are part of this release);
the rest ship as **v0.10.0**, because the wire protocol and the configuration
surface both change.

### Decisions

| # | Decision |
|---|---|
| D1 | Session per `(client, remote_addr)` — in practice per `(remote_addr, effective transport)`: the selector byte is per-connection, so a plain and a Noise service on one endpoint cannot share a connection without downgrading one |
| D2 | Per-service auth inside one session; one denied registration never kills the session |
| D3 | Control channel direct and independent (dependency direction, liveness isolation, repair ability) |
| D4 | Pool per carrier; `carrier` stays a per-service selector |
| D5 | Growth needs no new control command (client-local, client-initiated — NAT-friendly) |
| D6 | `min_tunnels` deleted; warmth is derived (warm for `idle_timeout` after activity); a pool starts cold |
| D7 | UDP-derived floor = `ceil(channels / streams-per-tunnel)`, capped by `max_tunnels`; the worker set is maintained |
| D8 | UDP channels are never recycled (peer affinity + the local source port the backend sees) |
| D9 | `[server].max_pool_size` deleted → `[server.data].max_tunnels_per_client`, default `0` = unlimited |
| D10 | `health_check` deleted (probe, state change and key) |
| D11 | Heartbeat kept, but the server **declares** its interval in the session ack; the client derives `max(10 s, 2 × interval + 5 s)`; `interval = 0` ⇒ no client timeout |
| D12 | `pool_size` split: TCP warmth is client-local; UDP parallelism is `udp_workers` |
| D13 | Bounds live at the connection layer; the server does not arbitrate channel counts |
| D14 | Over-cap ⇒ typed, non-fatal refusal; growth stops, and a retry needs a tunnel's death or a shrink |
| D15 | Growth thresholds and hysteresis stay internal constants until measured (they still are) |
| D16 | `direct` mode kept (sparse visitors + measurement control arm); M7 measured its role |
| D17 | IPv6 path MTU is covered by an in-crate `Ipv6Mtu` declared with nix's exported `sockopt_impl!`/`getsockopt_impl!` — no `unsafe` |
| D18 | Log model: level contract + aggregation + a log-budget test |
| D19 | Config shape: client-level policy, service-level intent; no knob without evidence |
| D20 | One commit per milestone with its own verification; docs in the same commit |
| D21 | Scheduling is two layers: pool placement (client) and pairing/assignment (server); the server does not choose tunnels |
| D22 | Instrument before policy (falsifiable): S1 adds only read-only accessors + telemetry; if the state spread is inside the noise, S2 does not land — **it was zero, so it does not** |
| D23 | Placement policy = eligibility + rotation + hysteresis (never a weighted score) |
| D24 | A stripe group's K streams must land on K distinct tunnels — **structural since the group request landed**: the server names the group before its channels are opened and the client reserves one tunnel per stripe (v4, extended in place) |
| D25 | No RTT sampling; the algorithm may use stream count, pending opens, send credit, worker queue depth — nothing else (send credit is not exposed by the engine, so it is not used) |
| D26 | Growth/shrink is a hysteretic, rate-limited state machine (≤ 1 tunnel per maintenance tick) |
| D27 | UDP assigns a *new* peer to the shortest worker queue — **falsified by measurement** (2026-09-30): a single visitor never fills a queue (its own socket buffer throttles it), many visitors fill them evenly, and the drop equals the excess over a per-pool ceiling that `udp_workers` does not raise |
| D28 | Waiting visitors stay FIFO; **spare** streams would be picked from the least-loaded tunnel — not landed (S1) |
| D29 | Stripe pairing is atomic: K spares from K distinct tunnels, all or none — server-side today (a broken channel discards the whole group and re-requests it) |
| D30 | Shrink requires `pinned_peers == 0`: a channel with pinned-but-idle peers is not idle |
| D31 | New sources never create channels (invariant); floods cost affinity entries only; table size, evictions and per-tunnel `pinned_peers` are in `MOLEHILL_UDP_STATS`; no hard cap until measurement asks for one |

### Rejected, with the reason (so it is not re-litigated)

Configurable `min_tunnels` (warmth is derivable); keeping `[server].max_pool_size`
(the bound belongs at the connection layer); `health_check = false` as a boolean
axis; server-side inference of backend health (indistinguishable from a visitor
that hangs up); the "health check avoids control-plane churn" argument;
per-service health numbers; reversing the heartbeat direction (still a
follow-up); cross-carrier striping (the slowest carrier gates the group);
`carrier = "auto"`; the control channel inside the pool; a non-zero default for
the server's tunnel cap; negotiating channel counts; weighted placement scores;
an RTT sampler; stateful prioritisation of waiting visitors; treating "zero
streams" as sufficient for shrinking; a whole-group `open_streams(n)` entry
point in the client (it had no wire command to trigger it — the open thread
above is what would give it one).

## Release (v0.10.0)

1. ~~Freeze~~ **done (2026-09-28)** — `version = "0.10.0"`, the `[Unreleased]`
   content moved under a dated `## [0.10.0]` section (now dated 2026-10-01, the
   tag day), `[Unreleased]` left empty, the withdrawn v0.9.1 artifacts deleted.
2. ~~Sweep~~ **done (2026-10-01, at `96bda00`)** — the artifact, the charts and
   both READMEs describe this commit: four tools, 8/8 stages each plus the
   capacity ramp, `--test=rrul,capacity`, `shape_legs=visitor`, the bounded
   rate-class window, `tree_clean: true`, and **`just soak-check`: `OK: no gate
   violation`**, no waiver. The gate's own self-check is the verdict: the
   v0.9.0 baseline is not a gate input (another host, and a method record
   missing six keys), which is the documented behaviour for a fresh sweep.
   Two earlier sweeps are superseded — `bbe9664` and `f34788d` — both because a
   `tests/` change landed after them, which is exactly what the pre-tag rule
   treats as invalidating the artifact.
3. Before the tag: `just tag-check` must be green on the release commit (it is,
   and its provenance line reads `results measured at v0.10.0-1-g96bda00; no
   code changed since`). **Only docs and assets may follow the sweep** —
   anything under `src/`, `tests/`, `Cargo.*` or `benches/scripts/soak/*.py`
   invalidates it, and that cost this release two extra sweeps.
4. ~~`just check`, `just interop`~~ **both green (2026-09-30)** — the full
   chain, and the interop matrix's three cases (both cross-version directions
   refuse, the refusing process keeps serving its own version).
5. ~~CI green, merge, changelog date, `just tag`, push the tag~~ **the first two
   tag pushes failed, and nothing was published either time** (2026-10-01):
   - **The emulated legs.** The release workflow tests every target it
     publishes, and the cycle's new `tests/noise_keys_test.rs` spawns the
     freshly built binary — which cannot be exec'd from inside an emulated test
     binary, so `arm-unknown-linux-musleabi` died with `Exec format error` and
     cancelled the matrix. Second time this class has failed a release; the fix
     is the guard the first incident introduced (`native_target`, file-level on
     `noise_keys_test.rs`, site-level on `pool_test.rs`'s one spawning
     scenario), verified by falsification rather than assumed.
   - **The musl leg.** With the cross legs green, `x86_64-unknown-linux-musl`
     stopped on `startup_failure_test`'s `Address already in use` — musl spells
     EADDRINUSE `Address in use`. The binary was right in both; the assertion
     was pinned to glibc's phrasing, and only the release matrix ever runs
     these tests under musl (the native legs are glibc, the cross legs are the
     guarded ones). Reproduced locally (`rustup target add
     x86_64-unknown-linux-musl` + `musl-tools`, then the matrix's own command),
     fixed to match what every spelling shares, and verified by running the
     whole suite on both targets.
   - **The `aarch64` cross leg, on the cache this cycle added.** With both
     fixes in, `aarch64-unknown-linux-musl` died before compiling anything:
     `libc`'s and `generic-array`'s build scripts, restored from a 347 MB cache
     entry, could not run in the `cross` container (`GLIBC_2.28 not found`).
     `Swatinem/rust-cache` keys on the job, not on the matrix value, so every
     leg shared one entry — and the legs do not build in the same environment:
     the native legs compile host binaries with the runner's glibc, the `cross`
     legs execute them inside a container with an older one. `ci.yml` already
     keys its matrices per target; `release.yml` was the outlier and now does
     the same (`key: ${{ matrix.target }}-release`). This one is a workflow
     change, so it does not touch the sweep's provenance.
   Each failure was fixed, the sweep re-run where the rule demanded it, and the
   tag deleted and re-pushed — which is what AGENTS.md §5 allows, and the
   reason the rule exists.
6. **Next**: push the tag → the release workflow publishes (GitHub Release,
   GHCR, crates.io).
7. **Open, for the next cycle**: nothing mechanically stops the next
   spawn-based test from missing the `native_target` guard, or the next
   assertion from pinning one libc's wording — these two failures were the
   second and third of their kind. A cheap check would close both (fail when a
   `tests/*.rs` contains `CARGO_BIN_EXE` without `native_target`, and when a
   test asserts a bare libc phrase); it needs a home in the check chain and the
   docs that go with it, so it was not added mid-release.

### Release sweep (2026-10-01, `96bda00`)

`v0.10.0-1-g96bda00`, tree clean, `stale: false`, binary sha256
`228cb43cc2c1e493` (4 181 840 bytes), host `99919695eec2` / host_id
`d764f9da9c7e5b2a`, calibration 417.8 MiB/s, loopback probe 21.59 Gbit/s,
`shape_legs=visitor`, `rate_socket_window=256K`, `batch=2`, four tools, 8/8
stages each plus the capacity ramp, `--test=rrul,capacity`. **`just soak-check`:
`OK: no gate violation`**, no waiver. Charts re-rendered and both READMEs
refilled from the plot's own tables. The peers are the ones re-downloaded for
this release (frp 0.71.0, rathole 0.5.0, nps 0.26.10 — all still the latest).

| tool | clean bulk (Gbit/s) | replicate | clean p99 (ms) | loss1 | rate100 | rate20 | ramp |
|---|---|---|---|---|---|---|---|
| molehill | 15.921-16.830 | 5.4 % | 8.4-9.4 | 9.727 | 0.100 | 0.019 | 8/8, never broke |
| frp | 6.181-6.191 | 0.2 % | 2.9-3.0 | 5.862 | 0.099 | 0.019 | 8/8, never broke |
| rathole | 12.822-12.867 | 0.3 % | 102.3-104.5 | 9.679 | 0.100 | 0.020 | 8/8, never broke |
| nps | 0.134-0.135 | 0.5 % | 64.4-66.9 | 0.142 | 0.100 | 0.020 | 0/8, broke at 1 (p99 204.84) |

What this run is worth reading for:

- **Every cell is a measurement this time**, including all four ramps: three
  arms carry the ramp's full 8 streams (the subject and both TCP peers), and
  only `nps` breaks, at its first load level — the same shape the `bbe9664`
  sweep had, and unlike the `f34788d` attempt whose `rathole` ramp stopped on a
  dead iperf3 backend.
- **The subject's SLO is met on both clean visits** (8.4 and 9.4 ms against the
  50 ms limit, zero errors), and its completeness, endpoint invariant and drift
  checks all pass.
- **It is the third sample of the top pair of arms**, and the one that settled
  what a run can say about them: across three sweeps of identical code their
  clean readings span 15.9-22.2 and 12.8-20.4 Gbit/s — swings of 39 % and 59 %
  that reverse their order — while `frp` (6.04-6.19) and `nps` (0.133-0.136),
  an order of magnitude below this host's loopback ceiling, moved by under 3 %,
  and the host's own loopback probe stayed inside 21.6-21.9 Gbit/s throughout.
  The READMEs publish no ordering for those two rows and `docs/benchmarks.md`
  states the limit with all three samples behind it.

**Environment incident on the way here.** One attempt at this sweep produced a
complete-looking artifact in 14 seconds: `iperf3` was no longer installed (a
container restart had reset the filesystem and taken the runtime-installed
package with it), so every test recorded a typed
`No such file or directory: 'iperf3'` failure. The gate refused it, the
superseded artifact was restored from git rather than published, `iperf3` was
reinstalled, and the run repeated. The environment notes below now name the
mechanism; `command -v iperf3` before a long run is the cheap guard.

## Open threads for the next cycle

- **Replacing the `udp_batch` FFI waivers (`quinn-udp`) is an A/B, not a
  cleanup.** This is the decision record behind the one place where
  `docs/lint-policy.md`'s "fix the code first" rule runs into "no equally
  reasonable alternative" — the eight `unsafe_code` expectations in
  `src/transport/udp_batch.rs` (the `recvmmsg`/`sendmmsg` batching FFI):
  - the raw syscalls have no `std` equivalent; `socket2` (already a dependency)
    covers everything *except* the batching calls;
  - **`nix` does not remove them.** Its `MultiHeaders<S>` holds
    `Box<[libc::mmsghdr]>` (raw pointers inside) and is therefore itself
    `!Send`/`!Sync`, so the three `unsafe impl Send`/`Sync` proofs — the part
    that carries the real soundness argument — would still be required, at the
    cost of a new dependency and a per-call `Vec<IoSliceMut>` allocation in the
    hot path. Verified against `nix` 0.31.3's source, not assumed;
  - **`quinn-udp` could remove all eight** (`UdpSocketState` is a plain
    `Send + Sync` struct whose `recv`/`send` take caller-owned slice buffers),
    but it also brings GSO/GRO segmentation, i.e. it changes the send path's
    syscall shape. That is a measurable change to the KCP data path, so it
    belongs in its own `just soak --test=screen` run against the current
    batching rather than in a lint cleanup.
- **Privileged ports** are documented as *not* implemented (the whitelist admits
  any port it contains; the OS decides whether the bind succeeds). Enforcing
  `<1024` would be a behaviour decision for the human.
- **Carried over**: an HTTP API for configuration (hot reload is files-only),
  replacing the python bench/test entries with `cargo-script` once it is stable,
  and QUIC (implemented and measured, parked in the `archive/transport-test`
  tag; revisit only for a UDP-only path or multi-stream loss isolation).

## Environment notes (this host, re-checked 2026-10-01)

- **Verify `iperf3` before a long run** (`command -v iperf3`). It is installed
  at runtime rather than baked into the image, so a container restart drops it:
  measured on 2026-10-01, when a restart (new hostname, uptime reset) left the
  binary gone and a sweep produced an artifact of typed failures in 14 seconds.
  The bench fails cleanly — every test records `No such file or directory:
  'iperf3'` and the gate refuses the file — but it looks like a finished
  artifact until someone reads it.
- **`/tmp` is periodically wiped.** Keep `--out` and logs under `~/tmp` or the
  repo. The Soak harness's own work directories (`/tmp/molehill-bench.*`) are
  normal residue and are never deleted by the harness.
- **`timeout N` orphans the run.** The wrapper signals `uv run`, not the python
  child, which keeps executing and holds the bench lock.
- **Peers are cached** in `~/tmp/bench-peers` and the interop binary in
  `~/tmp/interop`, so `just soak` and `just interop` normally need no network.
  `just soak-peers` re-resolves each peer to the **latest GitHub release** and
  re-downloads when the cached version is behind; the resolved versions are
  recorded in the results meta beside the numbers.
- **`sudo` works without a password** and `tc`/`ip` are present, so the shaped
  stages and the MTU classes run here.
- **This host is `3f8b4508ab91`**: 20 cores, 23 GB RAM, kernel
  `6.12.0-160000.38-default`. `/etc/machine-id` is **absent**, so the Soak
  harness's `host_id` hashes `cpu model + core count` only
  (`host_id_basis.machine_id: false`) — *not* the hostname. The container's
  hostname changes between sessions without moving `host_id`, which is the
  point: the comparable runs in this cycle's files share `host_id
  d764f9da9c7e5b2a`, and a file *without* a `host_id`
  (`results-soak-v0.9.0.json`, `98c48ea3fa68`) falls back to the hostname, so
  the gate refuses it.
- **A baseline-less run is the norm here**: the gate's own self-check
  (completeness, endpoint invariant, absolute SLO) is what a fresh sweep is
  gated on, because every stored baseline either has no `host_id` or predates
  the current method. The v0.9.0 file is still compared where the method keys
  agree, and the gate names what it could not verify rather than assuming it.

## Archive

Everything this cycle closed is in [HANDOFF-archive.md](HANDOFF-archive.md),
in the order it happened, with an index at the top of that page: the incidents,
the superseded sweeps, the measurement records, the retracted diagnoses, and
the closed threads of this cycle. The records that predate the 2026-09-28
freeze were archived into git history earlier and are indexed there too
(`git show f2156de^:HANDOFF.md`).

The rule that keeps the split honest: **a record in the archive says what the
branch's authors believed at the time, and no number in it may be quoted as a
measurement of the current code, compared against a Soak result, or used to
gate anything.** The live numbers are the sweep record under "Release
(v0.10.0)" on this page.
