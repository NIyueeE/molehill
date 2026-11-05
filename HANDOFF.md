# HANDOFF: current state and the plan

> **2026-11-03.** This page is the execution document: where the tree stands,
> what is measured, what is open, and what happens next. It is deliberately
> short — the running narrative of every cycle, with its refutations and its
> numbers, is in [HANDOFF-archive.md](HANDOFF-archive.md), which this page
> indexes. Per AGENTS.md §3 this is an agent page: user-facing facts belong on
> the docs pages, shipped behaviour in [CHANGELOG.md](CHANGELOG.md).

## Where the tree stands

| | |
|---|---|
| Branch | `feat/v1.0.0` |
| Version | `1.0.0` in `Cargo.toml` **and** `Cargo.lock` |
| Changelog | `## [Unreleased]` empty; one dated `## [1.0.0] - 2026-11-01` |
| `just check` | green end to end (both hook layers) |
| `just interop` | 3 of 3 — 6.0 and v0.11.2 refuse each other on the connection it happens on, and each keeps serving its own dialect |
| Bench peers | frp 0.71.1, rathole 0.5.0, nps 0.26.10, cached |
| `benches/records/results-bench-v1.0.0.json` | **absent on purpose** — no record is committed that its own gate refuses |
| Wire dialect | 6.0, unreleased; its commands may still be extended in place |

**No tag has been created and none will be by an agent.** Publishing is a human
act (AGENTS.md §5); what is missing is a gate that passes every time, and the
numbers that say why are on this page.

### mhd's last clean stage has never carried bulk, so the pair never existed (2026-11-05)

Every full-scale timeline record of this cycle, for mhd, has **no bulk throughput in its
last clean stage**:

| run | `clean` (first) | `clean#2` (last) |
|---|---|---|
| m88 | p99 285.99, **7.31 Gbit/s** | p99 23.51, **none** |
| m90 (two builds) | 146.03 / 25.79, 6.54 / 6.61 Gbit/s | 23.65 / 23.60, **none** |
| m95 | 302.13, 4.10 Gbit/s | 22.54, **none** |
| m102 (**TCP**) | 120.00, 13.54 Gbit/s | 19.54, **12.20 Gbit/s** |

So the sentence this file has been repeating — "the first stage is the SLO failure and
the last stage is fine" — was comparing a stage with four bulk flows against a stage
whose bulk instrument **had died**, every time. The `iperf3` cell reports
`control socket has closed unexpectedly` in the original and in the retry, while the
boundary echo probe answers in 5.5–6.0 ms, and my round-two lenient path kept the
stage's *latency* while recording the loss as a typed absence — which is why the number
looked fine and the row looked green.

TCP's pair is fair (13.54 against 12.20 Gbit/s) and its gap is real: 120 against 19.5 ms.
mhd's pair has to be re-measured before anything can be said about mhd's own first-versus-
last gap, and before condition 1 can be attributed to either.

What the wedged stage looks like from inside, on the server, during `clean#2` of m95:

```
mhd-stats: writes refused, by the bound that refused credit=16 window=269 aged=0 opening=0
```

— the response direction refused by the **peer's credit** sixteen bytes short of its
window, and by its own congestion window, with the age gate silent and no stall line at
all. On the client, in the same seconds, two or three datagrams a second are refused as
`malformed` and one or two as `malformed_decode`, which on loopback with no corruption
means a datagram was written that the decoder rejects. And the sink, for the whole 150
seconds, reports `0.00 Bytes` on all four streams before its idle timeout.

So the bulk flows are wedged end to end while a one-byte request through the same tool
answers in six milliseconds: the server has bytes to send and a window that refuses it,
the client has a stream that is not being read, and the backend has four connections with
nothing arriving. That is the next reproduction to build, and it is a *throughput*
defect in the carrier, not a harness one.

### FIXED: the L3 acceptance passes end to end (2026-11-05)

The prologue had been read twice. The fix is the small one from the entry below: the channel
queue **carries the key it was delivered with** (`ChannelQueue::rx` is now
`Receiver<(u32, C)>`, `push` takes the group), and the claim path uses that key as the lane id
instead of reading four more bytes off the wire. `read_lane_half` is gone — it existed only for
the doubled read.

`just l3-accept` now passes every check it makes, exit status 0:

```
VISITOR OK
REQUESTS n=3 size=65536 ok
VISITOR OK
REQUESTS n=2000 size=64 ok
PASS an idle claim lends a lane to the claim carrying more flows than lanes
PASS the busy claim stays byte-exact through the lane change
PASS with [transparent.data].lend = false no lane moves, and the claim serves byte-exactly
PASS the operator's valve refuses a claim's surplus lane, naming the cap
PASS one claim on the same four lanes carries the same load
PASS the claim serves byte-exactly on the lanes it got
```

So the transparent L3 path works end to end — the visitor reaches the claim, bulk and small
requests are byte-exact, the lane-lending allocator moves a lane between claims without a byte
lost, `lend = false` holds still, and the operator's valve refuses a surplus lane naming its
cap. That is the first of the objective's conditions to move from failing to passing, and it
moved by reading a key that was already in hand rather than by reading the wire again.

The lesson is worth keeping: the defect was invisible because both halves of the prologue are
**eight bytes and the same shape**, so the second read blocked rather than corrupting anything,
and the only line that said so ("named no lane: Failed to read the v4 lane prologue") had been
in the log for several rounds before it was read.

### The L3 deadlock's root cause: the prologue is read twice (2026-11-05)

The server said it itself, in a line this file had not grepped for:

```
Transparent channel for 10.99.0.1:8443 named no lane: Failed to read the v4 lane prologue
```

twice, sixteen seconds after the client's dial.

Both halves of the prologue are eight bytes — four of service id and four of either a stream's
group or a claim lane's id — and the two codecs double as each other:

- `read_stream_prologue` reads **all eight** bytes and returns `(ServiceId, group)`;
- `read_lane_half` reads the **second four** and returns the lane id.

And the server calls **both** on one channel: the `DataChannelHello` arm of the dispatcher
reads the whole prologue with `read_stream_prologue`, and then the claim path
(`run_transparent_service`) calls `read_lane_half` for four bytes the client never wrote a
second time. So the claim's channel blocks on that read, fails, is dropped with `continue`, and
`Routes::join` is never reached — which is the whole of the symptom: `member=0 live=false`, the
visitor's SYN unclaimed, no lane ever joined.

The comment above the dispatcher already describes the intended design and names a function that
does not exist: "a direct data channel names its service right after the hello; a transparent
lane names itself beside it (see `read_channel_prologue`)". That lookup — read the service id,
find out whether the service is a claim, then read the right second half — was never written,
so the server reads eight bytes and then four more.

Two ways to fix it, and the second is smaller:

- write `read_channel_prologue` as described: read four bytes, look the service up, then read
  the second half as a lane or a group. That is the design the comments promise;
- or hand the second half through: `do_v4_data_channel` already *has* it as `group`, and for a
  claim those four bytes **are** the lane id, so the claim path can be given
  `LaneId::new(group)` instead of reading anything. No wire change, no new lookup, and the
  prologue is read exactly once.

The second is what the next change does, and `just l3-accept` is the one command that decides it.

### The claim's lane reaches the right address over mhd, and the server never dispatches it (2026-11-05)

Two more lines — the handshake's start and finish, and the dial's resolved address — narrow it
again, one acceptance run each:

| step | evidence |
|---|---|
| the claim resolves its dial address | `data channel: resolving the dial address service=0 addr=10.10.0.254:2333` **twice** |
| the handshake completes | `data channel: handshake done service=0` **twice**, within a millisecond |
| the server accepts it | `Handshaking a control session` **once** — the control session, and nothing else |
| the server joins a lane | `lane joined` **never**, `member=0 live=false` |

So the address is right, the dial succeeds, and the server logs **no data channel at all**. The
resolution is not the bug and the client is not silent any more.

What that leaves is specific: the claim's lanes are **mhd lanes over UDP on the same port**, so
they never appear as a TCP accept — they arrive on the server's UDP dispatcher as a lane, whose
first bytes are the prologue naming the claim. The client's side of that handshake completes;
the server's side must then hand the lane to `forward_transparent` for the named claim, and it
does not — `Routes::join` is never reached, so the claim is never routable and the visitor's SYN
is dropped as unclaimed.

That is now one dispatch wide on the server, and it has no line of its own: the mhd lane path
logs its own progress (`mhd lane admitted`, window lines, stall lines) and none of it names the
claim the lane was opened for. The next change is a line where a claim's prologue is read on the
server — before and after the claim is looked up — and it will say whether the prologue is
refused (unknown claim, wrong service, a dialect mismatch) or accepted and then dropped between
the lookup and `forward_transparent`.

### Both ends of the lane dial now speak, and they say: the client dials, the server never joins (2026-11-05)

Two `debug!` lines, one at each end of the claim's data channel — `open_data_channel` on the
client (`service`, `claim`, whether it is a stripe) and `forward_transparent`'s join on the
server (`claim`, `slot`, `lane`, `carrier`) — answer in one acceptance run what five rounds of
reasoning could not:

| end | what it printed |
|---|---|
| client | `dialling a data channel service=0 claim=Some(10.99.0.1:8443) stripe=false` **twice** |
| server | `lane joined` — **never** |
| server counters | `claim=10.99.0.1:8443 member=0 live=false lane=-` |

So the client is not lazy: it dials the two lanes its `transparent_lanes: 2` asks for, at claim
setup, before any packet exists. The server never accepts one, and `Routes::join` is never
reached, which is why the routing table has no claim and the visitor's SYN is dropped as
unclaimed.

And the dial itself is silent in a way that is now specific: `spawn_data_channel` does log a
failure (`debug!("{:#}", e)`), and the client's log has **no line after the two dials** — no
error, no establishment, nothing. So the channel neither failed nor completed before the
acceptance gave up: it is stuck inside `run_data_channel`, between the spawn and whatever the
server would have seen.

That is the whole question now, and it is one function wide: `run_data_channel`'s dial —
the transport connect and the lane prologue write — needs the same two lines, one before each
step. Then the acceptance says whether the client never gets a connection to the server's data
listener, or gets one and never writes the prologue that names the claim.

### The eager first lane was tried and refuted; the instrument that would have said so (2026-11-05)

The smallest no-protocol fix for the L3 deadlock was to have a claim dial its first lane at
service registration instead of waiting to be asked, at the point where the service becomes
active (`open_channels(&active, slot.channels)` followed by `join_lane_pool`). It compiles, the
352 unit tests pass, and **the acceptance still fails with the same counters**:

```
l3-stats: role=server claim=10.99.0.1:8443 member=0 live=false lane=- carrier=- …
l3 device frame unclaimed proto=6 src=10.10.0.2:47886 dst=10.99.0.1:8443
```

The client's log for that run has eleven debug lines and **not one of them mentions a lane, a
channel or a prologue**, so the dial either never ran (that activation site is not on this
claim's path) or ran and was refused before anything reached the wire. Either way the change
was not verified, so it is reverted rather than kept on the strength of its reasoning.

The lesson is the same one this file keeps recording: the reasoning was sound — a routable claim
needs a lane, and nothing asks for it — and the *site* was wrong, and only an instrument would
have said which. `open_data_channel` writes no line of its own, so there is no way to tell a
dial that never happened from a dial that was refused.

So the next change is an instrument at each end of that dial, before another fix:

- `open_data_channel`: one `debug!` naming the service, the endpoint when it is a claim, and
  whether the request was queued — a dial that never happens and a dial that is refused look
  identical today;
- the server's accept path for a claim's data channel (`forward_transparent`): one line per
  join, naming the endpoint and the slot — that is the other end of the same question, and it
  is what `live=true` in the counters would mean.

With those two lines the acceptance says which end is silent, and the fix is then a fact rather
than a plausibility.

### Why L3 drops the first packet: a claim is routable only once a lane joins (2026-11-05)

The two switches that already exist (`MOLEHILL_L3_STATS=1`, and the `debug` line the unclaimed
drop writes) name the mechanism without any new code.

The server's own counters, one line per claim:

```
l3-stats: role=server claim=10.99.0.1:8443 member=0 live=false lane=- carrier=- flows=0
          forwarded=0 no_channel=0 offered=0 enqueued=0 …
```

and the packet it dropped:

```
l3 device frame unclaimed proto=6 src=10.10.0.2:55630 dst=10.99.0.1:8443
```

So the server reads the visitor's SYN from its TUN, `Routes::pick` asks
`EndpointTable::endpoint_for(info, Destination)` for `(10.99.0.1, 8443)`, and that lookup misses
— because **a claim enters the routing table only when a lane joins it**
(`Routes::join`, `hub.rs:715`), and no lane has joined (`live=false lane=-`).

And no lane joins because a lane is opened *lazily*: the client mints one when it has traffic of
its own to put into the tunnel, and this claim's first packet is **inbound**. So the packet that
would create the lane is the packet that needs the lane:

1. the visitor's SYN reaches the server's TUN and is read;
2. the claim is not in the routing table (no lane ever joined), so the packet is dropped as
   unclaimed and no LANE REQUEST is triggered;
3. the client has nothing to send, so it never asks for a lane either;
4. the counter stays at zero for the life of the topology.

That is a deadlock at the first packet, not a routing bug and not the container, and it is why
the acceptance reports only a timeout: every symptom downstream of step 2 is silence.

The fix is a design choice and belongs in the L3 path, not here: a claim must have a lane
**before** its first inbound packet — either the client opens one when the claim is registered,
or the server can ask for one when a packet arrives for a claim it knows about but cannot route.
The second is smaller and self-healing; the first is what the control plane already has the
vocabulary for.

### The L3 failure is in the product, and the counters say exactly where (2026-11-05)

The acceptance's failure was attributed by injecting one TCP connect from the visitor into a
running topology and differencing the two TUNs' counters. For a TUN, `tx_packets` is what the
kernel handed to userspace (the product read it) and `rx_packets` is what userspace wrote back
into the kernel (the product delivered it).

| counter | before | after |
|---|---|---|
| server `l3srv0` tx_packets | 22 | **30** |
| client `l3cli0` rx_packets | 0 | **0** |

So the visitor's packets **do** reach the server's TUN and **are** read by the product — eight
packets for one connect, which is the SYN and its retries — and **nothing is ever written into
the client's TUN**. Both devices work in the direction the product uses them (the client's own
`tx_packets` is 2, its stack wrote into the TUN and the product read it), the client has
`10.99.0.1/32` on `l3cli0` with the device UP, and both namespaces have `ip_forward=1`.

So this is not the container and not the routing: it is the **server → client relay inside the
product**. The visitor's packet is read on the server and never arrives at the claim's own
stack, and no log line names a drop — the L3 path is silent in both directions, which is why
the acceptance could only report a timeout.

The next places to look are the two ends of that relay: the server's L3 egress (does it frame
the packet it read and send it toward the claim's group) and the client's L3 ingress (does it
receive such a frame and write it to the TUN). Both are unlogged, so the first change is an
instrument at each end rather than another guess.

### L3 acceptance fails at the first thing it checks (2026-11-05)

`just l3-accept` builds the three-namespace topology with the real binaries and fails with:

```
FAIL the service never became reachable from the visitor
VISITOR FAIL timed out
```

The server and client both come up correctly, which is why this is not a startup problem: the
client logs `Running as a transparent (L3) client`, attaches to its TUN
(`Transparent data path attached to the TUN device tun=l3cli0 direction=Source`), establishes
the control session, and registers the claim — `Registered, exposed at 10.99.0.1:8443
service=web`. What fails is the only thing that matters: a visitor in the third namespace
cannot reach that address.

So one of two things is true, and this round did not distinguish them:

- **the claim's forwarding is broken** — the visitor's packets arrive at the client's TUN and
  are not carried, which is a product defect on the L3 path;
- **this container cannot run the topology's TUN devices** to the standard the acceptance
  needs — the devices are created (the client attached to one) but packets do not traverse
  them, which is an environment limitation that no code change fixes.

The distinction is one command away and is the next thing to run: `ping`/`tcpdump` on
`tun=l3cli0` inside the client namespace while the visitor sends, and a `sysctl` read of
`net.ipv4.ip_forward` in the client's namespace. A device that receives nothing points at the
environment; a device that receives and does not forward points at the claim.

This is the first of the objective's conditions to fail outright rather than read as unstable,
and it is recorded as failing rather than skipped.

### The gate items, measured: `check` and `interop` pass; capacity reads 1 to 8 (2026-11-05)

The objective's remaining conditions were untouched for twenty rounds while the first stage
was chased. This round measured three of them.

**`just check` passes** end to end on `71eef8f`: the heavy chain (audit, deny, outdated, the
serial test suite, both clippy passes, fmt, machete, the python gates, docs alignment)
reports `pre-push checks passed`.

**`just interop` passes**: three tests — a new server refuses an old client and keeps
serving, an old server refuses a new client and says so, and an old server rejects an unknown
dialect and survives.

**capacity does not yet read as a measurement.** Six ramps of the same scenario, three on an
early-cycle build and three on the current one:

| build | readings |
|---|---|
| pre-overdraft (round 12) | **3, 1, 1** |
| current (`71eef8f`) | **1, 1, 8** |

A single reading spans the whole ladder, so nothing can be concluded from one — including from
the `3` this round began with, and including a claim of regression against the published
`results-bench-v0.11.*`, whose `capacity_streams` is 8 for every arm. That baseline is not a
like-for-like comparison in any case: its method fingerprint is `257343fa3fd158d3` and this
cycle's runs carry `99a635024d8b174f`, because the probes, the workloads and the engine all
changed while the first stage was being chased. The gate refuses a baseline whose method
record does not match, which is the right behaviour and which this cycle has now triggered.

What the six ramps *do* say is worth as much as the number they fail to produce: **one of them
reached 8**, so eight concurrent bulk streams are attainable on this host, and the spread is
the same bimodality the first stage has. The capacity condition and the SLO condition are one
defect — an arrival's tail under bulk load, either ~23 ms or ~110 ms for a whole stage — and a
capacity number measured while the mode is undamped is measuring the mode.

### The whole first-stage ledger, and the ratchet hypothesis refuted (2026-11-05)

Every soak's first clean stage, by build, with the bulk rate beside it:

| build | p99 | bulk | |
|---|---|---|---|
| flat 2 MiB ceiling | 290.6 | 8.39 | fail |
| flat | 135.5 | 8.64 | fail |
| flat | 23.5 | 6.84 | pass |
| flat | 22.5 | 7.86 | pass |
| flat | 17.0 | 7.08 | pass |
| flat | 12.8 | 7.07 | pass |
| path-scaled ceiling (smoothed) | 23.3 | 8.23 | pass |
| path-scaled (smoothed) | 23.7 | 8.21 | pass |
| path-scaled (smoothed) | 109.8 | 8.01 | **fail** |
| path-scaled (smoothed) | 122.3 | 8.08 | **fail** |
| path-scaled (smoothed) | 23.8 | 8.12 | pass |
| path-scaled (**minimum** RTT) | 102.1 | 8.79 | **fail** |
| path-scaled (**minimum** RTT) | 38.3 | 7.49 | pass |

Three things this ledger says, and one it refuses to:

- the flat ceiling **never** passed above 7.9 Gbit/s (0 of 2) and always passed below it (4 of
  4). That separation is why the ceiling was changed;
- after the change the fast regime passes **more often than not** (4 of 7 in 8.0–8.8) and the
  good mode improved fivefold (23.5 against 135.5) — but the bad mode is still there, at 102
  and 110 and 122;
- **the minimum-RTT ceiling did not remove it** (102.1 at 8.79). The ratchet — a queue raising
  the estimate that admits a deeper queue — was a real risk and is now ruled out as the cause
  of the bimodality, by the intervention that would have removed it;
- what it refuses to say is *which* runs fail. Rate alone does not decide it: 8.01 and 8.08
  failed while 8.12, 8.21 and 8.23 passed. Something not yet instrumented sets the mode for a
  whole stage, and every candidate tested so far — refusals, loss, runtime saturation, the
  half-close grace, `FirstByte`, the accept queue, the queue's depth, the estimate's ratchet,
  and now the minimum — has been measured and excluded.

That is the state to hand over: the mechanism of the *good* mode is understood and the
mechanism of the *bad* mode is not, and the SLO's first stage on this host fails for TCP too
(120.0 ms in the same cell), which is why the remaining gate work moves ahead of it rather
than behind it.

### Four runs after the ceiling: better in the fast regime, still bimodal (2026-11-05)

Four `soak` campaigns on the path-scaled ceiling, all with a first-stage bulk rate between
8.0 and 8.2 Gbit/s — the regime that failed every time before the change:

| run | `clean` p99 | its bulk | `clean#2` p99 |
|---|---|---|---|
| m123 | **23.3 ms** | 8.23 Gbit/s | 13.3 |
| m131 | **23.7** | 8.21 | 6.3 |
| m132 | **109.8** | 8.01 | 4.1 |
| m133 | **122.3** | 8.08 | 23.2 |

So the change is **real but not sufficient**. What it bought is a frontier that moved: before
it, every run at or above 8.3 Gbit/s failed and the best failing reading was 135.5; now the
regime's *best* readings are 23.3 and 23.7 — a 5.7× improvement on the good side — while two
of four runs still fail at 110 and 122. The distribution is bimodal with the two modes at
about 23 and about 115, and the ceiling moved the good mode from "the slow side of the knee"
to "inside the SLO" without collapsing the bad one.

That is consistent with what the mechanism can and cannot do: shrinking the standing queue
from 3.85 MB to 0.06–0.75 MB cuts the wait in front of a small packet by the same factor, and
23 ms is what a 0.7 MB queue costs a three-round-trip handshake at eight gigabits. Cutting it
further needs one of:

- **fewer round trips in an open** — sealing, which deletes two of the three (and is still
  blocked by the striped deadlock);
- **a queue a handshake does not share**, i.e. small traffic on its own socket;
- **a smaller floor** — 256 KiB is already the estimator's granularity, so this road is
  nearly out of room at 8 Gbit/s.

The first is the one the whole cycle has been pointing at, and it now has a number: two of
three round trips are the difference between the passing mode and the failing one.

### The window ceiling was forty times the path, and now it is the path (2026-11-05)

The ceiling is now the floor scaled by the measured round trip instead of a flat two
mebibytes, and the change is verified *inside* the run that measured it rather than by the
number it hoped to move. The server's own window line, in the first clean stage:

```
lane=3  cwnd=262144  flight=59356   ceiling=262144  srtt_us=1000
lane=2  cwnd=262144  flight=356136  ceiling=262144  srtt_us=1000
lane=1  cwnd=786432  flight=747580  ceiling=786432  srtt_us=3591
```

- **the ceiling binds**: `cwnd` stops at 262 144 on two lanes, exactly the floor;
- **the flight is ten times smaller**: 59 KB to 747 KB, against the 3.85 MB that line used to
  read — the standing queue in front of every small packet is a tenth of what it was;
- **the feedback is bounded and visible**: when a lane's estimate rises to 3.6 ms its ceiling
  rises to 786 432, which is the same floor times the path. An inflated estimate raises its
  own ceiling, and that is now readable in the line that moves it.

And the stage it was measured on is the first one in the fast regime that passes:

| stage | p50 | p99 | bulk |
|---|---|---|---|
| `clean` | 2.57 ms | **23.32 ms** | **8.23 Gbit/s** |
| `clean#2` | 2.38 | **13.25** | 6.47 Gbit/s |

At 8.23 Gbit/s the six-run separation said this stage could not pass — every previous run at
or above 8.3 read 135 to 291 ms. One run is not a claim, and 8.23 sits just under the old
cohort's lowest failing rate; what is established is that the mechanism took effect exactly as
designed, and that the stage passed on a draw that used to be a failure.

The next run in the same regime decides whether that is the ceiling or the lottery.

### At 8.4 Gbit/s the runtime is idle and the handshake is ten times slow (2026-11-05)

The slow-open line answers what "delivery, not admission" means, on the same 290 ms stage
(3 006 of them):

```
mhd client slow data channel: group=32 started_us=6 open_us=13482 ready_us=13485 cmd_us=13494
```

- **`started_us = 6 µs`**: from the task being spawned to its first poll. The runtime is
  *not* saturated — the dispatcher, the pool, the scheduler are all doing their work in
  microseconds while eight gigabits a second of bulk flows past;
- **`open_us = 13 482`**: the stream open — three round trips of tiny packets — takes 13.5 ms,
  against 1.34–1.38 ms in the runs whose spine stays under 8 Gbit/s. A factor of ten, and it
  accounts for the arrival's whole tail.

So the delay is not the tool's scheduling and not its send path: it is that a small packet,
on a lane carrying eight gigabits a second, waits about 4.5 ms per hop — which is the shape
of a **socket receive queue** holding a few thousand bulk datagrams ahead of it. One
connection, one UDP socket, one FIFO: the carrier's own round-robin cannot reach a packet
that the kernel has not delivered yet.

And one anomaly worth naming, because it says the flight is not actually bounded: in that
stage the window line reads `cwnd=2097152` — the ceiling exactly — while the stall line
reads `flight=3847348`. **A flight 1.75 MB wider than the window it is supposed to obey.**
Writes are refused on `cwnd - flight`, so those bytes cannot have come through the ordinary
path; either `cwnd` shrank after they were admitted, or the opening overdraft added them a
reserve at a time. Either way a standing queue of that size is exactly what puts 4.5 ms in
front of every small packet, so **the flight's real bound is the next thing to fix**, and it
is the same fix for both problems.

### Six soaks: the arrival passes exactly when the bulk spine stays under 8 Gbit/s (2026-11-05)

Six `soak` campaigns, the first clean stage's p99 against that same stage's bulk throughput:

| `clean` p99 | bulk | |
|---|---|---|
| **290.6 ms** | 8.39 Gbit/s | fails |
| **135.5** | 8.64 | fails |
| 23.5 | 6.84 | passes |
| 22.5 | 7.86 | passes |
| 17.0 | 7.08 | passes |
| **12.8** | 7.07 | passes |

**No overlap**: every run at or above 8.3 Gbit/s failed the 50 ms limit and every run at or
below 7.9 passed it, with the worst passing reading 23.5 against the best failing one 135.5 —
a gap of 5.8× between the groups. Six points is not a curve, but a separation with no
overlap in six tries is the strongest quantitative statement this cycle has produced about
the first stage, and it names the regime: the arrival is fine until the bulk spine crosses
about 8 Gbit/s on this host.

Two hypotheses about *why* it crosses were tested against the 290 ms run and both are dead:

- **it is not refusal.** `opening=0` for the whole stage: not one opening write was refused by
  any bound, so the reserve overdraft from the previous round is not what this stage needs;
- **it is not loss.** The stall lines in that stage read `worst_stream_retransmits=0`,
  `unacked_ms=0`, `credit_blocked=true` — nothing lost, nothing waiting for an
  acknowledgement, and the queue is behind the peer's credit, not behind the network.

So at 8.4 Gbit/s the carrier refuses nothing, loses nothing, and the arrival is still twelve
times slower than the same arrival at 7.1. The delay is in **delivery**, not in admission: the
next candidates are the ones that scale with rate — the runtime's own scheduling of the
dispatcher under tens of thousands of datagrams a second, and the receive path's queueing —
and both are measurable with instruments that already exist (`spawn → first poll` for the
former, `backlog`/`parked_at` for the latter).

### Three suspects eliminated for the bulk wedge, and the instrument that names it (2026-11-05)

The wedge is "four bulk visitor connections whose bytes never reach the tool's forwarding",
and four things were checked against a run that has one (m111, wedged from `rtt100` on):

- **the half-close grace is not it.** `spawn_grace` fires 40 times over the whole run and
  **zero** times in the wedged window — so no bulk group had a direction cut after ten idle
  seconds;
- **no visitor was dropped at pairing.** `dropping the visitor` never appears in the run;
- **`FirstByte` is not it.** The arrival wrapper is a pure pass-through — it delegates
  `poll_read` and only observes the fill — so it cannot consume a visitor's bytes or report
  an EOF that did not happen;
- **the client's `frames=0 bytes=0` is not a symptom.** In a bulk *send* test the client's
  source is `iperf3 -s`, which sends nothing; a source read that returns nothing there is
  correct.

What the log showed instead: in the wedged window the server has exactly one class of
send loop waiting on a read — small groups (`frames=2 bytes=166`) — and **no send loop at
all waiting on the four bulk visitors**, while the join of pairing lines to group lines
lists six server groups that started and never finished their send direction, four of them
consecutive (13687, 13690, 13691, 13694).

So the bulk visitors are accepted, produce no pairing failure, and never get a running
group. The path from "accepted" to "forwarding started" has exactly one silent exit in it:
`pair_striped_group` returns `Ok` on more than one condition, and `serve_tcp_visitor`
**ignored the value**. A visitor could therefore be accepted, wait, and be dropped without a
single line naming it.

That is now instrumented: every visitor logs its pairing outcome (`stop_pool` true or false,
or the error) on the span that carries its address. The next wedged run will say whether the
four bulk visitors were paired at all, and if not, which exit took them.

### Four soaks: the last stage is tight, the first is bimodal, and it tracks bulk rate (2026-11-05)

Four `soak` campaigns on one build, one after another, with the arithmetic of both clean
stages and the bulk spine beside each:

| run | `clean` p99 | its bulk | `clean#2` p99 | its bulk |
|---|---|---|---|---|
| m107 | **135.5 ms** | 8.64 Gbit/s | 3.5 | none |
| m108 | **23.5** | 6.84 | 3.5 | 5.95 |
| m111 | **22.5** | 7.86 | 3.5 | none |
| m112 | **290.6** | 8.39 | 3.6 | 2.58 |

Two facts, both new and both sharp:

- **the last clean stage is 3.5 ms at p99 in all four runs, within a tenth of a
  millisecond.** Whatever the arrival path's problem is, it is not there: a warm stage with
  no shaped history behind it inside the same process reads the same number every time.
  That is the tightest distribution this file has ever recorded for an arrival metric;
- **the first stage is bimodal, and the split tracks the bulk spine's rate.** The two runs
  whose first stage passed carried 6.84 and 7.86 Gbit/s of bulk; the two that failed carried
  **8.64 and 8.39**. Four for four, in the direction that says an arrival gets slow when the
  bulk spine is fast — which is the same sentence this cycle started from, now with a number
  attached to "fast" (about 8 Gbit/s on this host) and with the arrival path's own stability
  measured separately.

That also explains why the last stage's bulk sometimes dies without hurting it: when the
spine does not reach that rate, the arrival does not queue behind it. The two observations
are one observation.

The relationship is across runs, not a controlled experiment — the next one to run is a
sweep with the bulk spine *throttled* to a fixed rate and the arrival measured at each, which
turns a correlation into a curve and says where the knee is.

### Number zero is lost before the peer's socket exists, and one duplicate fixes it (2026-11-05)

One line of logging on each side settles what thirty rounds of counters could not: the **first data
packet posted** and the **first data packet seen**.

```
client posted lane=0..3  number=0
server  seen  lane=0,2,3 number=1  below=0     the client's number 0 never arrived
client  seen  lane=0,2,3 number=0  below=0     the other direction is fine
```

The dialling end posts number 0 at 17:34:29.388 and the far end's first number seen is **1**, twenty-two
milliseconds later. So a lane's very first datagram is sent **before the far socket is associated**, and
it is lost with no trace: the kernel drops nothing (its queue never held it), the receiver refuses
nothing (it never saw it), and `posted` counts it because the socket accepted it.

That single loss is the whole of the pinned watermark. `AckSet::below` advances only when the number
*equal to it* arrives, so with number 0 gone it stands at zero for the connection's life. Everything the
sender subsequently issues is retirable only through the sixty-four-number bitmap, and the flight — six
hundred kilobytes against a window of sixty — can never come down.

**The fix is one duplicated datagram.** A lane's first numbered packet is now posted twice. The first copy
races the far socket and is lost as before; the second arrives. Fourteen hundred bytes, once per lane.

Measured on twelve runs: **eight complete**, against a baseline of seven and six — the best result this
cycle has recorded, though a twelve-run batch cannot resolve eight from seven on its own. What makes it
worth keeping is that it is aimed at a measured mechanism rather than at a correlation, and a confirmatory
batch is the next step. The instrumentation that found it was reverted with an experiment in the same file
and has to be re-added to re-verify the mechanism directly.

### A five-hundred-and-twelve-number bitmap is not better either (2026-11-05)

The previous entry's conclusion was that the seventy-two-hundred-byte flight needs an acknowledgement
that can name more than sixty-four numbers, so the bitmap was widened to **512 numbers** — eight words,
the acknowledgement growing from 28 to 84 bytes — with the wire codec, the set, the slide across words and
the window test all updated and the unit suite green.

Twelve runs: **five complete**, against a baseline of seven and six. Not a improvement, and inside the
noise of a twelve-run batch; the acknowledgement also costs fifty-six more bytes on every packet, which on
a full datagram is four percent of the wire.

So it is reverted. The rule this cycle has applied to every other candidate applies to this one: a change
kept only because it seemed right is a change nobody measured. The width of the acknowledgement is
therefore **not** the lever, and the pinned watermark has now survived every mechanism that could explain
it — a refused frame, a withheld acknowledgement, an unposted packet, a kernel drop, a frontier repair
from the sender, and an acknowledgement too narrow to name the hole.

What is left is the one thing none of those instruments looked at: **the receiver's own `below` when its
reader is stuck**. `below` advances only when a number arrives *equal to it*, and its walk is what turns
the acknowledgement's cumulative field into progress. If the receiver's `below` never advances past a hole
even though every number above the hole has arrived, then the sender is told the truth about the bitmap
and a lie about the frontier — and every retirement path in the sender trusts the frontier first.

### Both forms of frontier repair are harmful, and the second is catastrophic (2026-11-05)

The pinned watermark has one obvious cure: re-send the bytes the frontier is stuck under. It has now been
tried twice, and both times the measurement refused it.

| form | gate | twelve-run result |
|---|---|---|
| repair the frontier range every round trip, every stream | none | **4 complete** (round 63) |
| the same, only while the flight has closed the window | `flight > cwnd` | **0 complete** |

The second is not merely worse than the baseline (six or seven of twelve): it is the worst result this
cycle has recorded, with every partial stopping below one megabyte. The mechanism is not mysterious in
hindsight — `next_repair` answers the **lowest** offset it holds, so queuing the frontier range puts it
permanently at the head of the repair queue and the pump serves it on every pass, starving every other
stream. The more precisely the repair is aimed, the more completely it starves the connection.

So the frontier cannot be repaired from the sender, and the search has to move to the receiver's side of
the acknowledgement: the byte prefix and the cumulative watermark are both pinned by the same hole, and
the only way out is an acknowledgement that can say "I have everything except this one number" — which is
what the bitmap already says, and what its sixty-four-number width is too narrow to carry.

That is a wire change, and this dialect is still unreleased, so it may be made in place: **widen the
acknowledgement's bitmap from 64 to 512 numbers**, which covers six hundred kilobytes of flight — the
size the measurements keep stalling at — instead of eighty.

### Every number taken is posted, and number zero is the one that never arrives (2026-11-05)

Two cumulative per-connection numbers, which are the same kind of number on the same object:

| connection | posted_total | own_last_number | difference |
|---|---|---|---|
| client 0–3 | 3 660 / 2 771 / 3 767 / 2 903 | 3 571 / 2 697 / 3 686 / 2 822 | 89 / 74 / 81 / 81 |
| server 0–3 | 4 992 / 3 817 / 5 115 / 3 892 | 4 929 / 3 771 / 5 051 / 3 843 | 63 / 46 / 64 / 49 |

`posted` exceeds `last number taken` on **every** connection, by 46 to 89 — and that surplus is exactly the
control datagrams, which repeat a number and take none. So **no packet number is ever taken without a
datagram being posted**: the "issued but not posted" reading two rounds ago was the global-versus-per-
connection mismatch again, not a defect.

What remains is one number: **`below` is zero on several lanes** while `own_last` is 3 510. `below` is the
cumulative watermark — "every number under this one arrived" — so a value of zero says the receiver never
got **packet number 0**, and its staying at zero for the connection's life is the pinned watermark `wire.rs`
documents: the cumulative path retires nothing, the bitmap covers only `largest − 64 ..= largest`, and
every record between them can never be retired, acknowledged or declared lost.

So the defect is far smaller and far more specific than thirty rounds of search assumed: **the first packet
of a lane never reaches the peer, or is never sent, and everything above it is then unretirable.** The next
check is one line of logging — the first number posted on each lane, and whether the peer ever acknowledges
number zero — which separates "burnt before the lane existed" from "posted and still not there".

### Every number is acknowledged, and the counter names now say what they count (2026-11-05)

Two solid results, then the process fix that should stop this file's recurring mistake.

**`numbers_seen` equals `numbers_acked` on all eight connections, and the refusal count is zero** — a
receiver frames every number as its own and inserts every one of them into its acknowledgement set. So a
refused frame cannot be what pins a watermark, and acknowledgements are not being withheld.

**The window line's counters now carry their convention in their names**:
`numbers_issued_per_s`, `numbers_built_per_s`, `acks_applied_per_s`, `posted_datagrams_per_s` and
`received_datagrams_per_s` for the per-second windows, and `*_total` for the lifetime counters that never
reset. This is not cosmetic. Four rounds — 72 through 76, plus this one's first read — were spent on
comparisons that were meaningless because one side of the subtraction was a window and the other a
lifetime total, or because the two sides counted opposite directions. Each looked like evidence; none was.
A field name that states the convention makes that class of error visible in the line itself, which the
prose in this file demonstrably was not enough to do.

So the confirmed state is: the path is reliable (kernel drops are zero), every framed number is
acknowledged, no frame is refused, and the transfer still stops with a flight that will not retire. The
next instrument has to be one whose two sides are the same kind of number — the per-second issued count
against the per-second posted count on the *same* connection, which the renamed fields now make safe to
compare.

### The kernel drops nothing at all, which retires the whole loss line (2026-11-05)

`/proc/net/udp` ends every line with that socket's own **drops**, and it is kernel truth: nothing in this
code can inflate or hide it. Sampled once a second across four complete runs of the striped test:

**every socket, every sample, `drops` = 0.**

So no datagram was lost, dropped, or discarded by any UDP socket in any of those runs — and the "loss"
this file chased from round 72 through 75 was, in all three of its forms, an artefact of comparing two
counters that measure different events (a direction against its opposite, a build against a post, one
socket's count against another socket's).

That is worth having closed decisively, because it constrains where the stall can be. Every mechanism the
loss line implied — retransmission failures, repair loops that send nothing, acks that never arrive — is
now excluded **by the kernel**, not by our bookkeeping. The stall is therefore not a transport-loss
problem at all: it is a *progress* problem, inside the carrier, with a reliable path underneath.

Twelve runs of the sealed striped test after these fixes read **seven complete** — the baseline — so none
of the instrumentation rounds changed the behaviour they measured. The partials are 3 942 912 (three
times) and 465 920, plus one run that failed before the transfer started.

### The per-connection counters cannot measure loss, and the kernel's own can (2026-11-05)

The per-connection "posted" counter was incremented where a packet is **built**, not where it is posted —
so the 2–14 % "loss" of the previous entry was partly "built but never sent". Moving the count to the
post, where the socket accepted the run, gives:

```
lane 0: server posted 1652  client received 1828   (-176)
lane 1: server posted 1722  client received 1854   (-132)
lane 2: server posted 1899  client received 1855   ( +44)
lane 3: server posted 1653  client received 1829   (-176)
```

The client now reads **more** than the server posted, on three of four lanes. Two counters that should
differ only by loss differ by ten percent in **both** directions, which means they are not measuring the
same events — a single `Shared` carries datagrams from more than one socket, and a lane id does not name
one socket. That is the **third** population-style error on this question in three rounds (the first two
were the direction subtraction and the build/post confusion), and the right response is to stop
attributing loss with counters this code owns.

The kernel keeps the authoritative number per socket: every line of `/proc/net/udp` ends with that
socket's **drops**, and `RcvbufErrors` in `/proc/net/snmp` is the namespace total. Sampling
`/proc/net/udp` once a second during a run gives each lane socket's own drop count, with no bookkeeping
of ours in the path at all.

That is the next measurement, and it needs no code: run one failing case and read the per-socket drops
beside `ss -uan` during it. If the drops are zero, then the datagrams are not being dropped anywhere and
the disagreement is entirely in what these two counters count — which is worth knowing too, because it
would mean the carrier's own loss accounting has never measured loss either.

### Correcting it: the "loss" was two directions subtracted from each other (2026-11-05)

With the end named, the asymmetry dissolves into a mirror:

```
server lane 0:  posted 2232  received  253
client lane 0:  posted  297  received 2188
```

Server lane 0 posted 2 232 and the **client's** lane 0 received 2 188; the client posted 297 and the
server received 253. So the earlier "gap" was **one end's send volume minus the same end's receive
volume** — two unrelated directions — and every "lost datagram" this file has quoted since round 72 is
that artefact, including the 3 306.

The meaningful pair is **across ends, one direction**: server posted against client received, per lane:

| lane | server posted | client received | missing |
|---|---|---|---|
| 0 | 2 232 | 2 188 | 44 (2 %) |
| 1 | 2 255 | 2 211 | 44 (2 %) |
| 2 | 2 206 | 1 898 | **308 (14 %)** |
| 3 | 1 317 | 1 317 | **0** |

So there *is* a real per-lane loss, and it is between zero and fourteen percent — not the fifty percent the
wrong subtraction implied, and not enough to explain a wholesale stall on its own. Lane 2 loses 308 of
2 206 and lane 3 loses none, so the loss is neither universal nor lane-symmetric.

That is the second time this cycle a metric that looked decisive was a population mistake — the first was
the hand-off median compared across different populations in round seven. Both are the same shape: two
numbers that look comparable because they are the same field, and are not.

The next measurement is the one this pair implies: **the loss rate per lane over time**, not its lifetime
total — a lane that loses a fixed handful at start-up is a different defect from one that starts losing at
a particular moment.

### The loss is not uniform: one lane posts twice what it reads (2026-11-05)

Per-connection lifetime totals, from a run that stalled at 793 600 bytes:

| lane id | posted | received | difference |
|---|---|---|---|
| 0 | 4 926 | 2 593 | **+2 333** |
| 1 | 3 852 | 4 907 | −1 055 |
| 2 | 2 606 | 3 324 | −718 |
| 3 | 2 683 | 3 228 | −545 |

One lane id posts nearly twice what it reads while the others read more than they post — which is what a
lane sending into an address nothing reads looks like, with the other lanes' surpluses being the packets
they *do* receive from their own peers.

It is not yet conclusive, and this file should say why: both daemons run in one process in this test, so a
lane id names **two** `Conn`s (one per end) that share the number, and the last line for an id is whichever
end printed last. The attribution needs one more field — **which end** — beside the lane id.

That is a one-line addition and it closes a search that began in round 43 with a stalled reorder loop and
has since excluded every mechanism inside the carrier in favour of one outside it: the kernel drops
datagrams posted to a socket nobody reads, and the carrier's own counters cannot see a drop that happens
after `sendto` returns success.

### 3 306 datagrams are posted and never read, against 315 in a passing run (2026-11-05)

The lifetime counters remove the boundary ambiguity of the window pair, and they show a real loss for the
first time this cycle:

| run | posted total | received total | **never read** |
|---|---|---|---|
| passed | 37 272 | 36 957 | **315** |
| failed at 3.94 MB | 39 000 | 35 694 | **3 306** |

So a failing run loses **8.5 % of its datagrams between two sockets**, and a passing run loses 0.8 %. That
is not a bookkeeping artefact: the totals are monotonic, they count the same events on both sides, and
the difference is exactly the datagrams that left a socket and were never read.

It also reconciles every instrument that came before. The host's `RcvbufErrors`, `SndbufErrors` and
`InErrors` are all zero, so these are **not receive-buffer overflows**: at loopback a datagram sent to a
port nothing is listening on is dropped with an ICMP unreachable while `sendto` returns success, and a
socket whose send queue is full at the moment of a non-blocking send the same way. The sender cannot see
either — `posted` counts what the socket *accepted*.

That is the shape the whole search kept hitting from the other side: a peer whose `largest` never
advances while the sender posts, a flight that cannot retire, a window that stays at zero, a writer that
parks, and a frontier that cannot move — with every counter inside the carrier reading healthy, because
the loss happens **outside** the carrier, in the kernel, on the way to a socket that is not there.

The next measurement is per lane: `posted − received` attributed to each `Conn`, and each `Conn`'s peer
address over time. A lane whose peer port changed — a re-dialled pool entry — would send to an address
nothing reads, and that is the first thing to check.

### Numbers are never taken without a build, and the window counters cannot see a loss (2026-11-05)

Two counters that had never been compared — packet numbers taken and packets built — close one exit and
open a measurement question:

```
issued = 17 652   built = 17 652   (gap 0)
posted = 18 273   received = 17 097
```

**Issued equals built**, so the previous entry's thousand-and-seventy-six unseen numbers are not numbers
consumed without a packet: `encode_stream` failing after `next_number` would show here and does not.
(`posted` is larger than `built` because acknowledgements and credit updates repeat a packet number by
design and are posted without taking one.)

But `posted − received = 1 176`, and that is the first time this cycle has seen a gap in that pair. It is
**not yet evidence**: both are per-second windows that reset, so the last window's datagrams are counted
as posted and never as received when the run ends under them. A boundary artifact and a real loss look
identical in window counters.

So the next change is to make that pair **cumulative** rather than per-window — two monotonic counters,
reported once a second without being reset, and compared at the end of a run. `posted_total −
received_total` is then exactly the datagrams that left a socket and were never read, with no boundary
ambiguity, and it is the last quantity on this path that has not been measured cleanly.

### Acks arrive at 132 a second and the peer is 1 076 numbers behind (2026-11-05)

The acknowledgement counters answer the previous entry's question, and the answer is neither of its
options:

```
lane=2  applied=132  recv=178  cwnd=26980  flight=351912
        own_last=2610  largest=1534  base=950570  repairs_at_base=4  repairs_above=1621
```

- **acks are applied** — 132 a second, alongside 178 datagrams a second, so the peer is answering and
  the answers are landing;
- **the flight is thirteen times the window and does not retire**, so those answers retire nothing;
- **`own_last` is 2 610 and `largest` is 1 534**: the sender has issued a thousand and seventy-six
  packet numbers the peer has never seen.

That last pair is the whole of it. The bitmap covers `largest − 64 ..= largest`; the cumulative path is
pinned at zero by the lost first packet; so every record numbered above 1 534 — which is every record in
the flight — is **in neither mechanism**: never acknowledged, never declared lost, never retired. The
window stays at zero, the writer stays parked, and the peer keeps answering with the only number it can
still see.

And the earlier instruments now read consistently with that: `posted == received` counts *datagrams*,
most of which carry acknowledgements and credit (they repeat a packet number, by design), and
`recv_beyond = 0` says the peer's buffer holds nothing beyond a frontier that its own reader cannot
advance.

So the defect is not that acknowledgements stop and not that they are dropped: it is that the sender can
issue a thousand numbers beyond anything the peer has seen while every acknowledgement it applies is
about the numbers *below* that gap. The next step follows directly: **the acknowledgement's `largest`
must advance** — and if the peer's receive path is not seeing those datagrams, the place to look is what
`largest` is computed from when the peer's own reader is stuck.

### A killed run poisons every later one, and the symptom looks like a product failure (2026-11-05)

The acknowledgement counters were installed and then produced **no valid measurement**, because the
first two batches were void — and the way they were void is worth recording, since it can silently waste
any batch in this file.

A `cargo test` that is killed (by a timeout, or by cancelling a background loop) leaves its **test
binary alive**, and that binary **holds the test's fixed ports** (2333 and 2334) in `LISTEN`. Every later
run then fails at startup with

```
Failed to run the control session: Failed to connect to 127.0.0.1:2333: Failed to connect TCP socket
```

which reads exactly like the product failing to come up, while the server process is fine and the test
never started. Three runs were discarded that way before the ports were checked; `ss -ltn | grep :2333`
and `pgrep -af 'deps/integration_test-'` name the culprit in one line each.

Two rules for this harness, learned the hard way twice (round 7 was the same trap in the bench topology):

- after killing a test or a bench run, **verify the fixed ports are free before the next sample**;
- `pkill -f` on a pattern that also appears in the loop's own command line kills the loop — the pattern
  must match only the child (`target/debug/deps/integration_test-<hash>`, not `integration_test`).

Once cleaned, the first valid run stalls at **138 240 bytes** — earlier than the 465 KiB cluster, so a
third stop point exists. The acknowledgement counters are in place and unmeasured.

### A parked writer whose peer has acknowledged everything it ever saw (2026-11-05)

The early cluster's first instrumented stall shows two things at once. The group is short **three
frames** this time (`highest` 122, `delivered` 119) — a gap, not the tail — and beside it:

```
lane=2  parked=1  base=175964  ack=175964  below=142  largest=141
lane=0  parked=1  base=58660   ack=58660   below=53   largest=52
```

`below = largest + 1` is the healthy value: every packet number the peer has seen, it has. `base =
ack` says its byte prefix matches the sender's frontier. And the writer is nevertheless **parked**,
which means `cwnd - flight` is zero — and if every number the sender issued had been acknowledged, the
flight would be empty and the window wide open.

The two states cannot both be current, so the pair reported is **stale**: the acknowledgement fields are
the *last one received*, and if acknowledgements have stopped arriving, they freeze at the last good
value while the flight behind them grows. That is a third flavour of this stall, distinct from the tail
frame the framing fix addressed and from the pinned watermark before it: **the acknowledgements stop**.

The next instrument is the obvious one and it is small: **count acknowledgements received per second**,
beside the datagrams received and the stream's `ack_pending` state. If acknowledgements stop while the
peer still holds arrived bytes, the defect is in the peer's decision to send one — and `ack_pending` is
the field that decides it.

### Fixed: a packet could claim bytes it did not hold, and they were skipped for ever (2026-11-05)

The wire question was answered first and it pointed inward: **posted 37 417, received 37 432** — every
datagram one end posted was read by a socket, and the refusal counters stayed silent even after the two
places that discarded `on_datagram`'s result began counting. So nothing was lost between sockets and
nothing was refused: the bytes were **never in a datagram**.

`SendBuf::append_to` copies from the stream's chunk list and **stops at the first chunk it does not
have**, answering nothing; the caller had already framed the packet with the *requested* length and
then advanced the send point by that same length. When `trim` had dropped chunks the frontier passed,
the packet therefore claimed more bytes than it carried, and the difference was skipped **for ever**:
in no packet, covered by no acknowledgement, and waiting at the stream's frontier while every counter
in this file read zero.

`SendBuf::present(offset, len)` now answers how many bytes are really in the chunk list, and the header,
the seal and the send point all take that number. `append_to` returns what it copied and the caller
asserts the two agree.

Measured on twelve runs: **seven complete**, which is the baseline — and the partials are **all in the
early cluster** (465 920 four times, 564 224 once), where the baseline's eleven partials had **eight in
the middle cluster** between 3.5 and 6.9 MB. The mid-cluster stalls are gone; the early one remains,
and it is the next target.

### The frame never arrived, and both ends' counters are out of candidates (2026-11-05)

The binary question the previous entry left is answered: **the peer's receive buffer holds nothing
beyond its contiguous frontier** — `recv_beyond_pieces` and `recv_beyond_bytes` read **zero on every
lane, in every sample**, of a run that stalled at 3 670 016 bytes. So the missing frame did **not**
arrive and was not waiting to be walked; the "arrived but cannot pass it" branch is excluded.

Everything on the send side has now been excluded by measurement, each in its own run:

| candidate | how it was excluded |
|---|---|
| not built | `written == handed_over` (gap 0) |
| built and dropped | `built == posted` (7 730 = 7 730) |
| frame did not fit | `"frame does not fit"` — zero lines |
| seal failed | `no_key = 0`, and a failure would close the lane |
| never declared lost | the timer declares it; `retx` in the thousands |
| repaired but not sent | `retx == posted` |
| refused by the peer | `datagrams refused` never prints |
| dropped by a full buffer | host `RcvbufErrors`, `InErrors`, `SndbufErrors` all zero |
| arrived, cannot be walked | `recv_beyond_* = 0` |

Ten instruments, ten negatives, and the packet still leaves a socket that says it posted it and does not
arrive at a socket that says it received nothing. That is not a bookkeeping question any more: it needs
an observation **at the wire**, between the two sockets, which no counter in this carrier can make.

The next step is a different instrument class — a capture on the loopback path for one failing run
(`tcpdump -i lo udp port 2333` into a pcap, filtered to the lane's ports), or a send/recv datagram pair
counted at the socket boundary either side. Either answers in one run what ten counters could not: does
the datagram leave, and does it arrive.

### Correcting that entry: the timer was already armed from the oldest record (2026-11-05)

The previous entry said the loss timer is armed "from the newest activity", and that is **wrong**.
`Recovery::arm` computes `base = self.oldest_data_time().unwrap_or(now)` and sets `timer = base + rto`:
the timer has been armed from the oldest packet that carries stream data since the round that fixed the
seven-second reader. So the timer fires — the three thousand retransmits in these runs come from it —
and it is not being pushed forward by a busy lane.

Two more candidates are then excluded by the logs rather than by reasoning:

- **`repair_sent` trimming before the packet is posted**: `encode_stream` failing would do exactly
  that, and `"frame does not fit"` appears **zero** times in every failing run;
- **the seal**: a failed seal closes the lane and counts `no_key`, which reads zero.

So the last frame's record is declared stale, its range is queued, the pump builds it (built equals
posted), and the peer still does not acknowledge it — or the peer holds the bytes and its `arrived()`
cannot pass them. Those two are now the entire remaining question, and the difference is one field the
receiver does not report yet: **whether its receive buffer holds any range at or above the frontier**.

If it does, the frame arrived and the bug is in the receive-side contiguity walk; if it does not, the
frame never arrived and the bug is in the send side's build. That is the next instrument, and it is on
the peer, not on the sender every entry since round 43 has examined.

### The unacknowledged gap is exactly one stripe frame, and nothing follows it (2026-11-05)

Three numbers side by side on a stalled stream settle what rounds 43 to 63 were circling:

```
written = 1016126   handed_over = 1016126   (gap 0)
handed_over - base = 32778                  (= 32768 + 10: one stripe frame and its header)
base = ack = 983348
```

So every byte the writer produced was handed to a packet — **nothing was left unsent, and nothing was
built and dropped** — and the peer acknowledges all but **exactly one frame**, the *last* one that
stream sent.

That is the whole mechanism, and it explains every failed fix in this file. A packet number is declared
lost either by the cumulative watermark (pinned at zero by one lost first packet) or by the bitmap,
which needs a **newer** number to have arrived. The missing frame is the newest thing on its stream:
there is nothing after it, so no gap can ever form, so it is never declared lost — and its bytes are
never re-sent. The only thing that could declare it is the timer, and the timer is **armed from the
newest activity** on a lane that four streams keep busy.

That is why repairing the frontier on its own clock made things worse (four of twelve against six): it
fired every round trip for every stream, flooding the repair queue, instead of firing once for the one
frame nothing else can reach.

The next change is precise and small: **arm the loss timer from the oldest unacknowledged record, not
from the newest activity.** A record at the end of a flight that nothing follows then ages out and is
repaired, which is the one case the whole mechanism misses.

### Repairing the frontier itself makes it worse, which inverts the conclusion (2026-11-05)

The natural fix for the previous entry's ratio — fourteen frontier repairs against two thousand one
hundred above it — was to give the frontier its own clock: when a stream's frontier has not moved for a
round trip, queue the range that covers it, chosen by offset rather than by packet number.

Measured on twelve runs: **four complete**, against a baseline of six or seven. Worse, and reverted.

That result is more informative than the ratio was. If re-sending the range that *covers* the frontier
does not unblock the stream — and actively hurts, presumably because it floods the repair queue with a
range the peer cannot use — then **the byte the frontier is stuck on is not in a range that can be
re-sent**. It was never appended to a packet.

Which moves the search entirely off the recovery machinery and onto the send buffer's own accounting:
`SendBuf::append_to` decides which bytes of a stream go into a packet, `head`/`cursor` track what has
been handed over, and `written` counts what the writer produced. A stream whose `written` is ahead of
every range any packet ever carried would look exactly like this from every counter in the file: no
gap in the reorder map, nothing unsent, nothing refused, the peer inserting bytes, and a frontier that
cannot move because the bytes at it never left.

That is the next thing to instrument, and it is one line: **how many bytes of a stream have been
appended to packets** (`sent`, which the send buffer already tracks) beside `written` and `base`.

### 99.4 % of the repairs cannot unblock the stream (2026-11-05)

Two counters on the two places a declared loss becomes a repair — the acknowledgement path and the
timer's — split every repair by which side of a stream's frontier it lands on. One failing run:

```
max repairs_at_base = 14        repairs that cover the frontier
max repairs_above    = 2163     repairs that land above it
```

**Fourteen out of two thousand one hundred and seventy-seven.** The repair machinery is not idle and
it is not broken: it re-sends, continuously, ranges that by construction cannot move the frontier the
stalled stream is waiting on. That is why every counter this file has quoted since round 43 looked
healthy — `retx` grows, `posted` equals it, the peer accepts everything — and the transfer still stops.

It also explains the previous entry: the stale scan added to the acknowledgement path **never fires**,
because the timer is doing the declaring (3 112 retransmits in that run came from there). The timer's
scan takes records older than the loss delay in number order, and the records that cover a frontier
sitting a megabyte into the stream are not the oldest ones — the oldest are their predecessors, whose
bytes the peer already acknowledged by prefix, so their repairs are skipped as below the base (round
53's fix). The one range that matters is neither the oldest nor the newest.

So the next change is in the declaration's *choice*, not in its trigger: when the oldest records are
all below the frontier, the scan has to reach the record that **covers** it — the range that starts at
or before the base and ends after it — and declare that one, every time the frontier has not moved for
a round trip.

### The parked writer, and the record nothing could ever declare lost (2026-11-05)

The per-stream instrument fires on the first failing run: **one writer parked for 11 880 ms**, and the
line beside it names the bound:

```
lane=3 parked=1 oldest=11596ms cwnd=63403 flight=527868 base=ack=1533710 below=0 largest=5821
```

`window = cwnd − flight = 0`: the flight is eight to eleven times the window, so every write on that
stream is refused and its loop parks. The flight cannot retire because **`below` is zero** — the
cumulative path retires nothing at all — and the bitmap retires only `largest − 64 ..= largest`. The
byte hole the frontier sits on corresponds to a packet number hundreds of numbers below the window, so
it is **in neither mechanism**: never acknowledged, never declared lost, never re-sent.

`on_timeout` does scan the whole flight for records older than the loss delay and declares them stale —
but the timer is **armed from the newest activity**, and a lane carrying four streams always has newer
activity. On a busy lane the timer may never fire, so the only loss declarations that happen are the
bitmap's, which cannot reach the record that matters.

The stale scan now also runs on the acknowledgement path, restricted to the records the bitmap cannot
reach (`number < largest − 64`, older than the loss delay, eight per call). It is kept as a correctness
fix: a record outside the window is otherwise unrecoverable by construction.

Measured on twelve runs: **six complete**, with partials at 8 169 984 (the closest this cycle has come
— 97.4 %), 4 103 168, 286 720 and three at 465 920. Six is the baseline, so no improvement is claimed.

So declaring the record lost is not sufficient. The next question is one number: whether the range a
stale declaration repairs actually **covers** the byte the frontier is stuck on, or sits above it — the
same question the repair counters asked in round 43 and never answered for a declared-stale range.

### The stall instrument is blind to a parked writer on a busy lane (2026-11-05)

Both ends' group lines are in one file and they show the freeze from the sending side:

```
group 1: sourced 4073984 -> 4139520 -> 4172288 (frozen) | written 160768 -> ... -> 3670016 (frozen)
```

`sourced` — bytes this group read **from its source** — freezes at 4.17 MB while the visitor still has
eight mebibytes to give. So the fault is in the `send_loop`, not in the receiving side that every
earlier entry examined: the loop stopped reading its source.

The loop parks in `StripeSender::poll_send`, whose `poll_write` reaches `Conn::write`, and `write`
answers 0 when `room = credit.min(window).min(held)` is zero — the congestion window or the peer's
credit. `write` does set `unsent_at` on a short write, which is what `Conn::waiting` reports… and the
stall line still never printed in any of these runs.

**That is a blind spot in the instrument, not in the counter.** The pump's stall line needs a pass that
built *nothing* while bytes were queued. A lane carrying four streams is never idle: three of them keep
producing packets, so the pass always builds something, and the one parked writer is invisible — for
hundreds of seconds, in every failing run this cycle.

So the loop closes as a fixed point: a pinned watermark means records do not retire, a flight that does
not retire keeps `window` at zero, a zero window parks the send loop, a parked loop reads no source, no
source means no new data, no new data means no new acknowledgements, and no acknowledgements leave the
watermark pinned. Every counter in the carrier is consistent with that and every instrument pointed at
the wire agrees there is nothing wrong with the wire.

The next instrument is the missing one: **a line for a strand's writer parked while the lane is still
building packets** — `Conn::waiting` already knows the age; what is missing is reporting it per stream
rather than only when the whole pass is empty.

### The echo is excluded for the third time, now on a metric that can resolve it (2026-11-05)

The previous entry's first candidate was the test's own echo: `io::copy` over both halves of a socket
cannot read while it is blocked writing, so a carrier that applies backpressure could stall it against
itself, and the symptom would be exactly what the group line shows — no gap, and a source that stopped
producing.

The decoupled echo (reader → bounded queue → writer) was measured on the bytes-delivered metric:
**six complete of twelve**, with partials at 3 775 488, 4 398 080, 4 398 080, 1 481 728, 1 867 776 and
302 080. The baseline is thirteen of twenty-four, whose two batches read seven and six.

**Six is the baseline.** So the echo is not the cause — and this is the third time it has been
measured, the first two on eight-run samples that could not have resolved it either way. Reverted.

What that leaves is the previous entry's second candidate, one hop further out: the **client's** group
stopped reading its source. The instrument for it is already installed — `sourced` per group — so the
next run reads the client's own group state at the moment the server's group goes idle.

### The group is idle, not stuck: `delivered` equals `highest` (2026-11-05)

A per-group counter of bytes read **from the group's source** settles the direction of the fault:

```
group 1: sourced=4172288 written=3670016 highest=114 delivered=114 idle_ms=860
group 2: sourced=3702784 written=3844608 highest=122 delivered=118 idle_ms=3635
```

Group 1 has **no gap at all**: `delivered` equals `highest` (114), so the reorder loop has released
every frame it has seen and is waiting for a frame that has not been sent. The visitor's byte count at
the stop was 3 670 016, which is exactly `written` — so the visitor stopped reading because the group
stopped writing, and the group stopped writing because nothing more arrived.

So the stall is **upstream of the receiving group**: the peer's `send_loop` — whose source is the echo
backend — stopped producing. Group 1's own `sourced` (4.17 MB) is its *other* direction and was still
moving.

That is a different shape from every entry before it in this file, which all assumed a *gap* in the
reorder map. There is no gap: the pipeline is simply empty.

Two candidates, and the next run separates them:

- **the echo backend stopped**: the test's `io::copy` echo cannot read while it is blocked writing, so
  a carrier that applies backpressure can stall it against itself. This file has "refuted" that twice
  on eight-run samples — comparisons that could not resolve anything, as the last two rounds showed —
  so it deserves a proper A/B on the bytes-delivered metric;
- **the client's group stopped reading its source**: the same question one hop further out.

### All four stripes stop together, so the stall is the source (2026-11-05)

The stall line carries a number that had not been read carefully:
`stripe group stalled behind sequence 123 with 1 frame(s) parked`. One frame parked, and `highest`
125 — so the reorder loop is missing **one** frame and has only **one** frame waiting behind it.
With four stripes that means the group's source stopped after 125 frames in total: the other stripes
did not run ahead and pile up, they stopped too.

That dismantles the last version of the story in this file. It is not one stripe whose carrier
stalled while the rest carried on; at the moment of every partial stop the whole group is at the same
frame, which means **the thing feeding it stopped producing**.

The feeder is the forwarding loop: it reads the visitor (on the server) or the backend (on the client)
and writes frames into the group. For it to stop producing while the carrier is healthy — built equals
posted, the peer inserts bytes, nothing is refused, and **no stall line ever fires** — it must be
parked on its source read, and the source must be a socket with nothing in it. A visitor that wants to
send 8 MiB and whose socket is empty is a visitor whose bytes the tool has already read… or a visitor
the tool has stopped reading, which is the same thing from the counters' point of view.

So the next instrument is on the feeder: **bytes read from the group's source per second**, per group,
under the stats switch. If that number is zero at a stall while the visitor still has bytes to send,
the tool stopped reading its visitor; if it is zero and the visitor has nothing left, the visitor
stopped writing and the question moves to the harness's own writer.

### Naming the laggard stream: no change either (2026-11-05)

The stall's two clusters — 455 KiB and 3.5–6.9 MB — put the earliest one at frame 14, and fourteen
mod four is stripe 2, which is the stripe on the lane whose watermark read zero. So the story this
round tested was: one lane loses its first packet, its watermark is pinned at zero for the
connection's life, its records retire only through the 64-bit window, and once the window passes a
record that lane stops delivering.

The escape from a pinned watermark is the byte prefix, and it names one stream per acknowledgement.
Round-robin over the live streams made it worse (two of eight against five); naming the **laggard** —
the stream whose frontier is furthest behind — is the version that always helps the stream that needs
help. Measured on twelve runs: **six complete, and the partials at 465 920, 531 456, 3 185 664,
4 365 312, 4 725 760 and 7 347 200**.

The baseline is thirteen of twenty-four complete, and the two twelve-run batches that make it up read
seven and six. **Six is the baseline.** No improvement, reverted.

That is the sixth mechanism excluded for this stall — after the RTO, the socket queue, the retirement
past the peer's word, the sealed-payload clamp, the `AHEAD` window and the round-robin prefix — and
every one of them was excluded by a measurement rather than by an argument. The next thing to look at
is not the acknowledgement at all: it is the stripe layer's own read loop, because every partial stop
so far has been reported there (`stripe group stalled behind sequence N`) and the carrier's counters
have been healthy or merely pinned in every case.

### The baseline's own scatter: 13 of 24 complete (2026-11-05)

The twelve-run baseline was repeated on the same build, which is the first thing any A/B on this
metric needs — the metric's own scatter:

| batch | complete (8 388 608) | partials |
|---|---|---|
| first | 7 of 12 | 6 859 264 · 5 250 048 · 4 824 064 · 4 365 312 · 465 920 |
| repeat | 6 of 12 | 3 942 912 · 3 844 608 · 3 702 784 · 3 578 880 · 465 920 · 465 920 |

**Combined: 13 of 24 complete (54 %), eleven partials in two clusters — three at 465 920 bytes (455
KiB) and eight between 3.5 and 6.9 MB.**

So the floor for any change on this path is a completion rate that moves well outside 13/24: the
binomial scatter of a 54 % rate over twenty-four runs is roughly ±20 points, which means a change has
to take the path to the high seventies before it can be called an improvement, or the partials have to
shift clearly — all eight of the middle cluster up, not two of them.

That is the standard every stride change from here is judged by, and it is stricter than the eight-runs
comparisons that produced three "no difference" verdicts in a row.

### The stride baseline is bytes delivered, not a pass rate (2026-11-05)

Twelve runs of the sealed striped test — `MOLEHILL_MHD_SEAL=1 cargo test --test integration_test --
--test-threads=1 striped_data_channels`, eight runs a side is not enough to resolve anything around
fifty percent — give a distribution rather than a coin:

| delivered | runs |
|---|---|
| 8 388 608 (the whole transfer) | **7** |
| 6 859 264 | 1 |
| 5 250 048 | 1 |
| 4 824 064 | 1 |
| 4 365 312 | 1 |
| 465 920 | 1 |

So the baseline is **seven of twelve complete, and five partials spread from 0.47 MB to 6.9 MB**. That
is the right metric for this work from here on: the byte count carries about four bits of information
per run instead of one, so a change can be judged on the same twelve runs that a pass rate cannot
resolve. Fisher's exact test on eight runs a side, which is what the last few rounds used, can only
resolve differences of about forty-five points — which is why three separate changes were each
measured as "three of eight against five of eight" and none of them could be told apart from noise.

Every stride measurement from here states both numbers: how many runs completed, and where the
partials stopped.

### The watermark is at zero for the connection's life, and rotating the prefix is worse (2026-11-05)

Two more results, one a structural fact and one a refuted fix.

**Structural.** The sender's own highest issued number and the peer's `largest`, side by side:

```
lane=2 own_last=4639 peer_largest=4512 below=0 base=1925414 num=4596
lane=1 own_last=3382 peer_largest=3357 below=0 base=2047664 num=3358
lane=0 own_last=3900 peer_largest=3875 below=0 base=2089648 num=3876
```

`own_last` and `peer_largest` are close — the numbers *are* getting through — and **`below` is zero on
every one of them**: the peer's cumulative packet watermark has never advanced past 0, for the
connection's whole life. Numbering starts at 0, so `below = 0` means **packet 0 never arrived**: one
lost first packet pins the watermark for ever, exactly as `wire::ACK` documents, and from then on
every retirement depends on the 64-bit window sliding past each record.

**Refuted fix.** The documented mitigation is the byte prefix, and it names one stream per
acknowledgement. It is set to the stream that just received a frame, so a stream that has *stopped*
receiving is never named again. Rotating the named stream round-robin across live streams looked like
the obvious repair; measured, it takes the sealed striped pass rate from **five of eight to two of
eight**. Naming a stream whose frontier has not changed is a no-op for the peer, and the stream that
needed the update gets it one time in N. Reverted.

So the prefix-starvation theory is out as well, and the mechanism that remains is the one the format
documents: a single packet lost at the start of a connection, a watermark stuck at zero, and every
stream's retirement leaning on a 64-bit window that cannot carry a long-lived hole.

### Built equals posted, and the wider window does not help (2026-11-05)

Two of the three candidates from the previous entry are now measured and both are out.

**Nothing is built and thrown away.** Per-second datagram counts, summed over a failing run:
**built 7 730, posted 7 730** — exactly equal. So every packet the sender recorded did reach the
socket, and the pump's run handling is not the defect.

**The `AHEAD` bound is not it either.** Raising the receiver's out-of-order window from 256 to
16 384 — enough to cover the largest flight a 16 MiB buffer can hold in packets — changes the sealed
striped pass rate from five of eight to **four of eight**. No improvement, inside noise, and it costs
memory; reverted.

So the facts that now have to be explained together are these, and they are narrower than ever:

- every packet is posted (7 730 = 7 730);
- the peer keeps **inserting** bytes (117 KB a second in the run before, up to 2 MB of total inserts
  in this one);
- the peer's `largest` and `below` stay **frozen hundreds of numbers** below the packet that covers
  the frozen frontier, with its bitmap all ones;
- `retx == posted`, so the repairs the sender sends are exactly the repairs it intends;
- the hole sits at a byte offset that two live records cover, and neither is ever acknowledged.

A peer that inserts bytes but never advances its marks is receiving packets whose numbers it does not
record as new. With the window wide enough that cannot be the gap set. The next instrument is
therefore on the *receive* side of the marks: how many packets `AckSet::arrived` accepted as **new**
versus as **duplicates**, per second, beside the bytes inserted.

### The peer's `largest` is below the packet that carries the frozen bytes (2026-11-05)

Reading `largest` — the one mark this cycle had never printed — settles what `Ack::contains` was
being asked and answers it in the negative:

```
lane=0 base=994110 num=1785 below=1760 largest=1759
lane=1 base=983348 num=1761 below=1761 largest=1760
lane=2 base=983348 num=1804 below=1779 largest=1778
```

**The peer's highest-seen packet number is below the number of the packet whose range covers the
frozen frontier.** So that packet was recorded as sent by the sender, and the peer never received it
— and because `largest` cannot advance past a number it has not seen, nothing above it can ever be
acknowledged, and the sender's gap rule (which compares against `largest`) can never declare them
lost either.

Meanwhile the receive path is alive: 117 KB a second are being inserted, which by these numbers can
only be duplicates of ranges at or below `largest` — the repairs of old data — while every packet
numbered above it fails to arrive.

Two candidate mechanisms, and this round did not separate them:

- **the `AHEAD` bound.** `AckSet::AHEAD` is 256 and the gap set drops arrivals beyond it, by design,
  so that a peer far ahead of a hole has its data re-sent rather than tracked. But a re-send takes a
  *new* number from the same monotonic counter, so with the sender's numbers already thousands past
  the hole, every re-send is discarded the same way. That is a deadlock by construction — and the
  observed `largest` sitting *below* the hole is not quite its signature, so it is a candidate and
  not a finding;
- **a packet recorded and never posted.** `next == written`, `flight` holds hundreds of kilobytes,
  and the pump logs no error — but nothing yet counts built datagrams against posted ones.

The next instrument separates them in one run: **datagrams built versus posted per second**, beside
the peer's `largest`. If built exceeds posted the discard is in the pump; if they are equal, the
packets left the socket and the `AHEAD` trap is what took them.

### The receiver inserts 117 KB while its frontier stands still (2026-11-05)

A per-stream counter of bytes inserted into the receive buffer, summed per lane, answers the last
ambiguity of the previous entry:

```
lane=0 inserted 1279698 -> 1397002 (+117304) | base 994110 -> 994110 | ack 994110 -> 994110 | retx 1820 -> 1996
lane=2 inserted 1782126 -> 1899430 (+117304) | base 983348 -> 983348 | ack 983348 -> 983348 | retx 1526 -> 1834
```

So the repairs **are** arriving and being inserted — 117 KB between two samples, on two lanes with
*identical* deltas, which is what re-sending the same fixed set of ranges every second looks like —
and the byte frontier does not move. The receiver is confirming, from its own side, the conclusion
the sender's counters reached in round 43: **the repairs land above the hole.**

The packet marks say the same thing once more, with the numbers side by side:

```
lane=0 base=994110 ack=994110 below=1760 bits=ffffffffffffffff num=1785 covering=2 pending=1 at=1463326
```

The record covering the frozen frontier is packet **1785**, the peer's watermark is **1760**, and its
bitmap is all ones — so by `Ack::contains` the peer *has* packet 1785, and the sender should have
retired the range, absorbed it into `acked`, and advanced `base` through it. It did not. Two records
cover the frontier and neither was absorbed; one repair is owed a megabyte higher.

So the disagreement is now between two functions in one file: `Ack::contains` says the covering
packet arrived, and the retirement path did not act on it. The next instrument is one line inside
`Recovery::on_ack` for that number — whether it was still in `in_flight` when the ack arrived, and
what `contains` answered — and it is one run away.

### One stream's prefix per acknowledgement, and the stream that needs it stops being named (2026-11-05)

`wire.rs` documents this failure mode and its mitigation in the same paragraph: a packet number is
never retransmitted under the same number, so one real drop stops `below` for the life of the
connection, everything that scrolls out of the 64-bit window is declared lost, its record is
forgotten, and the acknowledgement it would have carried lands nowhere. The mitigation is the byte
prefix: twelve bytes per packet naming one stream's frontier, monotonic across losses.

Reading the sender of that field:

```
    /// The stream whose arrived prefix rides this end's acknowledgements.
    arrived_stream: u32,
    /// How many of that stream's bytes arrived with no hole.
    arrived_prefix: u64,
```

and it is set to **whichever stream last received a frame**. On a lane carrying four stripes that
means the acknowledgement names the stripe that moved most recently — and a stripe that has stopped
receiving, which is exactly the one whose frontier is stuck, is **never named again**. Its last
reported prefix is frozen, `advance_to(prefix)` becomes a no-op, and the only remaining mechanism is
the 64-bit bitmap the paragraph above says cannot carry it.

That is a real limitation of the current format rather than a bookkeeping slip, and it matches the
observed shape: four stripes, three of them reporting a frontier that does not move, `retx == posted`
because the repairs are of ranges the peer's prefix never mentions.

It is not yet the whole cause, and this file should not claim it is: in the failing run the peer's
reported prefix for the stalled stream **equals** the sender's frontier (48 bytes), so the peer
genuinely holds nothing beyond that byte. Rotating the named stream would therefore not move it —
the bytes are not there.

So the last ambiguity is one counter, on the peer, per stream: how many bytes `RecvBuf` **inserts**
per second, beside the sender's posted repairs for that stream. If inserts are zero while repairs go
out, the packets are not arriving; if inserts are non-zero and `arrived()` does not move, the insert
path has a hole. Either answer ends the inference.

### The watermark never leaves zero: the opening packet of a stream is never sent (2026-11-05)

The stall line the stripe layer already writes names the frame
(`stripe group stalled behind sequence 123 with 1 frame(s) parked`, and 123 mod 4 is stripe 3), and
the window line names the stream:

```
lane=2 stream=5 base=48 next=32826 covering=3 num=10 below=0 pending=0 retx=50 posted=50
lane=0 stream=5 base=48 next=24634 covering=3 num=10 below=0 retx=38 posted=38
lane=1 stream=5 base=48 next=32826 covering=3 num=10 below=0 retx=25 posted=25
```

Three readings, and together they name a defect this cycle has already fixed twice on other paths:

- **`below = 0` while all sixty-four bitmap bits are set.** The peer has acknowledged sixty-four
  packets and its cumulative packet watermark has never advanced past zero. That is only possible if
  the packet numbered at the very start of these streams **never arrived** — the bitmap covers the
  64 most recent, so everything older is invisible, and retirement leans entirely on that window;
- **`base = 48`** on all three lanes: the byte frontier stopped after 48 bytes while `next` is thirty
  thousand. So the peer holds 48 bytes and nothing more;
- **`retx == posted`** on every one (50/50, 38/38, 25/25): each declaration produced its repair, so
  the repair machinery is working — it is repairing ranges the peer cannot use, because the hole
  sits below the bitmap's reach.

So this is the same class as the two never-sent packet numbers already fixed (a refused packet and a
failed seal each consuming a number), one level further out: **a number is consumed without a packet
going out, at a stream's opening.** That poisons the watermark for the connection's life, and once
64 packets have passed, the hole is out of the bitmap's reach and the stream can never be
acknowledged again — a stall that no repair can fix, which is exactly what the counters show.

The next check is one counter: per stream, packet numbers **consumed** versus packets **posted**. The
two never-sent-number bugs were both found by reasoning about that identity; measuring it directly
would find the third without another round of inference.

### The early stall has a different signature: a group stuck at frame 124 and a watermark at zero (2026-11-05)

After the `arrived()` fix the sealed striped test passes five of eight, and the three failures stop
much earlier than the ones that fix addressed. Their instruments read:

```
group 1: highest=125 delivered=123 written=3975680 idle_ms=1848
lane=0  base=1007974 next=1040752 covering=1 num=2228 below=0 bits=ffffffffffffffff pending=0
```

Three things are new here and none of them is the shape just fixed:

- the group stalls at **frame 124** with only 125 frames ever written, so this is the *first*
  megabyte of a transfer, not a long transfer's tail;
- `idle_ms` 1 848 — the group stopped moving about two seconds before the last sample, and the
  visitor's byte count froze at exactly `written`;
- one lane's peer reports **`below = 0`** with all sixty-four bitmap bits set: it has acknowledged
  sixty-four packets without ever advancing its cumulative packet watermark. Retirement by prefix is
  impossible on that lane; only the bitmap path can retire anything, and `base` sits a megabyte
  ahead of where prefix-based retirement would leave it.

So this stall is not the same defect, or not only it. What is missing to finish the diagnosis is one
mapping: **which lane and stream carry the frame the group is waiting for**. The group knows its
stripe count and the reorder loop knows the gap; the lanes know their stream ids; nothing ties them
together in a log line. The next change is that tie — one line at group start naming each stripe's
stream, and one at a gap naming the frame and the stripe it should have come from.

### Fixed: the arrival frontier walked only its first piece (2026-11-05)

`RecvBuf::arrived()` reported `max(first_piece_end, read)` — the first piece and nothing else. A
32 KiB stripe frame arrives as twenty-five sealed packets, so the frontier advanced by one piece per
read and could not move at all while the reader waited for a frame the reorder loop would not
release. That is the shape the marks described: the peer's bitmap said the covering packet had
arrived (`below` at its number, all sixty-four bits set) while the byte frontier stood still.

It now walks the contiguous chain from `read`, stopping at the first hole.

Measured on the sealed striped test, eight runs each way:

| | pass | fail |
|---|---|---|
| before | **3 of 8** (this cycle, after the two packet-number fixes) | 5 |
| after | **5 of 8** | 3 |

and the unsealed path is unchanged at 5 of 5.

Not cured: the three remaining failures stop at 465 920 / 433 152 / 465 920 bytes — the *earliest*
stopping point this file has recorded, from the very first round of the cycle — so they are a
different stall from the one this fix addressed, and the next run should be read with the same
instruments already installed (the group state line, the covering packet number, and the peer's
marks).

### The peer says the covering packet arrived, and its byte prefix does not move (2026-11-05)

Two facts added — the packet number of the record covering the stalled frontier, and the peer's
`Ack { below, bits }` — and the previous entry's conclusion had to be corrected on the way: in the
failing run there was **no** sample with a stalled frontier and nothing covering it. Every stalled
sample had two to four records covering the frontier, so something *is* owed there.

What the marks say, in a stalled sample:

```
lane=0 base=1247424 next=2035838 covering=2 num=4477 below=4477 bits=ffffffffffffffff
```

- **`num = 4477`**, the packet whose range covers the frozen frontier;
- **`below = 4477`**: every packet numbered below it arrived, by the peer's own word;
- **`bits = 0xffff…f`**: all sixty-four numbers below the watermark arrived too.

So the peer reports the covering packet as **arrived**, while its byte-space prefix (`arrived()`)
stays frozen at `base`, and the sender never retires that range. Both ends agree on the packet and
disagree on the bytes in it.

That is a much sharper statement than the last one and it moves the search into one seam: how an
arrived packet's `SentRange` becomes a byte-space acknowledgement. On the sender that is
`SendBuf::ack(offset, len)`, which absorbs `[offset, offset+len)` and then advances `base` while
`acked` covers it contiguously; on the receiver it is `RecvBuf::arrived()`, which reports
`max(first_piece_end, read)` — **only the first piece**, never walking a contiguous chain of them.

`arrived()` is the more suspicious of the two now: a frame's bytes arrive in many pieces (a 32 KiB
stripe frame is twenty-five sealed packets), and a frontier that advances by one piece per *read*
cannot move while the reader is waiting for a frame the reorder loop will not release — which is
exactly the stall's shape.

The next change is to make `arrived()` walk the contiguous chain from `read` and count every piece
it passes, then measure the sealed striped pass rate again: before this cycle's fixes it was two to
three of eight, and after them three to four.

### The sender repairs above the frontier and never the hole itself (2026-11-05)

One line from the previous run had the answer in it and was read too quickly:

```
lane=3 base=196716 next=983388 ack=196716 repair_at=783236 pending=1
```

**The repair queue's head is 783 236 and the frontier is 196 716.** So the sender is not short of
work — it is repairing a range **well above** the hole — and the hole at the frontier has **no
obligation at all**. That is why the queue reads empty at the other samples: the range the peer is
waiting for is never in it.

So the mechanism narrows to the **declaration** of a loss, which happens in exactly two places: the
timer (`on_timeout`, oldest tracked packet) and the acknowledgement path (the ranges a peer's
`AckSet` says it is missing). Neither has ever declared the range that covers the frontier.

Three facts now bound it:

- the peer **refuses nothing**: `datagrams refused` sums all six reasons and never prints, so it is
  not `over_window`, not a retired stream, not a seal failure;
- the host's own UDP counters read **zero** `InErrors`, `RcvbufErrors` and `SndbufErrors`, so
  nothing was dropped by a full receive buffer either;
- `repair_sent` is called only where a packet is **built**, and `send_run(...).await?` aborts the
  pump on a post error rather than discarding quietly — so a consumed obligation did reach the
  socket.

Which leaves the one place not yet measured: **why the range covering the frontier is never
declared lost.** The next instrument is the peer's `Ack { prefix, blocks }` and the sender's
`in_flight` keys, side by side for the stalled stream, for one second. If the peer's blocks cover
the frontier's range while its prefix sits below it, the peer is telling the sender it *has* those
bytes — and then the bug is in what the peer puts in `blocks`, not in what the sender does with it.

### The repair queue is empty while the frontier is stuck: obligations are consumed, not delivered (2026-11-05)

The user chose to keep chasing it, so two more facts were added to the window line — the repair
queue's **head** and its **length** — and they answer the question the previous entry could not:

```
lane=3 base=957748 next=2013484 ack=957748 repair_at=0 pending=0 posted=538 retx=1032
lane=2 base=954164 next=2009900 ack=954164 repair_at=0 pending=0 posted=560 retx=1076
```

- `base == ack` again, frozen at the peer's word;
- `next` a megabyte ahead: everything was sent;
- **`pending=0` and `repair_at=0`**: the repair queue is **empty** — the sender believes it owes
  nothing at all;
- and **`retx` (1 032 declarations) is twice `posted` (538 repair packets)**: obligations are being
  created and then consumed without a packet reaching the wire for them.

So the mechanism is no longer a mystery of selection: **an obligation is spent without the bytes
being posted.** `repair_sent` trims a range when a packet for it is *built*; if that packet never
reaches the socket — a built packet discarded before the post — the range is gone, the peer never
sees it, and its frontier can never move past it.

That is the last thing to confirm, and it is one counter: **packets built versus packets posted**,
per second, on the lane. `stats::note_post` already counts the post; the build side needs the same.
If built exceeds posted during a stall, the discard is the bug and it lives in the pump's run
handling — a much smaller place than the twelve the cycle has searched.

### Stop and ask: the stall has outlived three attempts and every named suspect (2026-11-05)

`AGENTS.md` §9 says to stop after three failed attempts on one problem, write the findings down
and ask. This is that point, and this entry is the account.

What is **established** about the stall (sealed striped, and the production wedge — same shape):

- the reorder loop of one stripe group sits on **one missing frame** while the readers have seen
  up to fifty frames beyond it (`highest` 254, `delivered` 122);
- that stripe is one mhd stream, and it stops delivering while the other three on the same lane
  carry on (frames 124–254 arrive);
- the stalled stream's frontier `base` equals the peer's last cumulative `ack` **to the byte**, on
  two different runs and two different lanes — so nothing is misretired and the peer's word is
  honest;
- `next == written`: nothing is left unsent;
- repairs are **posted** (1 120 of 2 030 declarations) and the peer **refuses nothing**;
- `flight` stays at hundreds of kilobytes the peer never acknowledges.

What has been **refuted**, each by its own measurement: an RTO (the slow-open distribution is not
quantised), a socket receive queue (`Recv-Q` 0 across thirty samples), a retirement past the
peer's word (`base == ack`), a peer reporting bytes it lacks (same), the sealed-payload clamp
(the range is recorded with the clamped length), `SendBuf::trim` failing to prune stale repairs
(it prunes them), and the receiver dropping the frames as retired-stream traffic (four such drops
in a whole run, all on one id at startup).

So the remaining contradiction is exact and I have not found its cause: **the peer's first missing
byte is at `base`, the sender posts repairs continuously, the peer refuses nothing, and the
peer's frontier never moves** — therefore the byte at `base` is not in the repairs that go out,
and nothing in the sender's bookkeeping says so.

Three attempts on this have failed (the frontier pair, the posted-vs-declared pair, and the
`next_repair` pruning read). Per the rule, this stops here and asks the human which way to spend
the next rounds.

### Repairs are posted, the peer accepts everything, and the missing byte is never in them (2026-11-05)

Two more counters separate "declared lost" (a decision) from "repair packet posted" (a fact), and
at a stall they read:

```
lane=2 base=950570 next=2032244 written=2032244 retx=2030 posted=1120 ack=950570 flight=469216
```

So the repairs *are* going out — 1 120 of them against 2 030 declarations — the peer refuses
nothing, and:

- **`base` equals `ack` again**, to the byte, on a different run and lane: the frontier matches the
  peer's word, so retirement is not retiring anything the peer lacks;
- **`next == written`**: nothing is left to send;
- **`flight` is 469 KB**: the sender still holds records the peer has not acknowledged.

Put together, the picture is now narrow enough to state as a contradiction: the peer's first
missing byte is at 950 570, the sender posts repairs continuously, and the peer's frontier never
moves — so **the byte at 950 570 is not in any repair packet that goes out**. Nothing is lost on
the wire (the peer refuses nothing) and nothing is misretired; the repair *selection* is offering
ranges that do not include the one byte everything is waiting for.

That is the last place in this path that has not been measured, and it is one function:
`next_repair` takes the lowest key in `SendBuf::repairs`, clamps it up to `base`, and returns
`None` when the clamped range is empty — without ever removing that entry. A `repairs` map whose
lowest key sits *below* `base` therefore returns `None` for ever while higher repairs are trimmed
away by `repair_sent`, which is exactly a stream that keeps posting repairs and never fills its
frontier's hole.

The next change is to make `next_repair` skip entries that are entirely below `base` (and drop
them), and to count the times it had to skip: a number that is non-zero at a stall confirms the
hole was unreachable by construction.

### The frontier and the peer's word are identical, so retirement is not the bug (2026-11-05)

The window line now carries `worst_base` (the stream's delivery frontier) and
`last_ack_prefix` (the peer's last cumulative word), and at a stall they read:

```
lane=3 cwnd=262144 flight=997084 worst_base=1309660 worst_next=2039422
       worst_written=2039422 worst_retransmits=2108 last_ack_prefix=1309660
```

**`base` and `ack_prefix` are equal to the byte.** So the two candidates this file named
are both excluded: the sender has not retired a range the peer lacks, and the peer has not
reported bytes it does not hold. `next == written` says everything has been sent, `retx=2108`
says repairs are still going out, `flight` is nearly a mebibyte — and the peer's word does not
move past 1 309 660.

So the peer's frontier is frozen at a point where **the sender believes it owes nothing there**:
the repairs it does send are of *other* ranges. The bytes the peer is waiting for are not being
re-sent, and the range that covers them is in the sender's live set (it is above `base`) without
ever being declared lost.

Two instrument errors were found and fixed on the way, both of the same family as the rest of this
file: `worst_stream()` filtered on *unsent* bytes, so it reported "no worst stream" at a stall
whose stream had caught `next` up with `written`; and the window line's default `0` could not be
told from a real stream id `0`.

The next question is narrow: which live range covers the frozen offset, why it is never declared
lost, and whether its repair is being skipped by the scheduler's `next_repair` — the one place
that decides what a stream with nothing unsent is still owed.

### The stalled lane repairs for ever and retires nothing (2026-11-05)

The per-second window line now carries the worst stream's own numbers (`worst_next`,
`worst_written`, `worst_unsent`, `worst_retransmits`), and the first sealed failure it caught
reads:

```
lane=1 cwnd=20235 flight=788414 worst_next=2065022 worst_written=2065022
       worst_unsent=1081674 worst_retransmits=1928
writes refused, by the bound that refused credit=0 window=0..14 aged=9 opening=0
```

Four facts, and together they name the mechanism:

- **the flight is thirty-nine times the window** (788 KB against 20 KB), so the lane refuses every
  write: `next == written` says the writer has nothing more to give, and what it did give is
  stuck in flight;
- **the stream has repaired 1 928 times** — a repair loop, not a stall for want of work;
- **the age gate is closed** (`aged=9` refusals a second), which is the gate doing its job on a
  flight that never drains, and is also why no *new* data could paper over the hole;
- **the peer rejects nothing** — `datagrams refused` never prints in the whole run — and sealing
  is healthy throughout (`sealed_sent` 1 300/s, `unsealed_tags=0`, `no_key=0`).

So the sender re-sends, the peer accepts every datagram it receives, and **no record retires**.
The fault is therefore not the wire and not the seal: it is either the peer's acknowledgement —
its `AckSet` not covering ranges it has accepted — or the sender's retirement, which fails to
clear a record whose bytes the peer reports having.

The next instrument is that pair, for the stuck stream: the oldest unretired range (offset, len)
on the sender and the peer's last `Ack { prefix, blocks }`. Whichever of the two is wrong will say
so in one line, and it is the same pair the production wedge needs.

### The stall is one stream, one frame: `highest` 254, `delivered` 122 (2026-11-05)

A group's state is now reported once a second with the stats switch on
(`spawn_group_monitor`), because the line at a group's *end* cannot describe a stall — a stalled
group never reaches its end, which is why this took several rounds.

The first sealed failure it caught:

| group | highest | delivered | written | idle |
|---|---|---|---|---|
| 2 | **254** | **122** | **3 910 144** | 4.1 s → 6.2 s, growing |
| 3 | 254 | 254 | 8 268 288 | finished |
| 1 | 258 | 231 | 7 514 624 | still moving |

So the readers of group 2 have **seen frame 254** and the reorder loop has **released frame 122**:
frames 123 through 254 are sitting in the reorder map, and the loop is waiting for **one missing
frame, sequence 123**, while `written` stays frozen.

That is a precise statement and a new one: the stall is **one frame on one stripe**, and because a
stripe is one mhd stream, it is **one mhd stream that stops delivering** while the other three on
the same lane carry frames 124–254 without trouble. It also rules out the reorder loop itself: it
is doing exactly what it should with a gap, and the bytes it is waiting for never arrive.

So the defect is inside a single stream's delivery on a healthy lane — the same shape the
production wedge has (four bulk connections, one or more of which stop while the rest move). The
next instrument is that stream's own account at the stall: its `next`/`written`/`retransmits`,
its `AckSet` frontier, and whether the peer's cumulative acknowledgement has advanced past the
gap. If the frontier has passed it, the sender believes the missing bytes are delivered and no
repair will ever be asked for; that is the one failure mode that explains a permanent stall with
nothing reported.

### The sealed striped failure does not reproduce in-process, and the attempt taught a lesson (2026-11-05)

Three tests were added, and the third one is an honest negative.

- `four_sealed_streams_carry_thirty_two_kib_frames` — four sealed streams, each written a **32 KiB
  frame at a time** (what the stripe layer does, and the shape no unit test had), 1 MiB per
  stream: **passes**, byte-exact.
- `frames_through_loss(sealed)` and its two tests — the same transfer with one datagram in five
  hundred dropped in each direction, sealed and unsealed: **both pass**.

The first version of the lossy test **failed** (stream 3 stopped at 165 293 of 262 144 bytes) and
looked like the reproduction this cycle wanted — four streams, frames, loss, a stall. It was the
harness: the loop advanced the clock but never called `on_timeout`, so a lost datagram was never
re-sent. The carrier's own account at the stall said so plainly in hindsight — `cwnd=262144`,
`flight=291384`, `backlog_streams=0`, three streams complete — a window closed by a flight that
nothing was repairing. With the timer driven the same test passes in 0.01 s.

So the lesson is the same family as the others in this file: **a stall is reproducible only if the
harness does what the runtime does**, and the missing `on_timeout` was invisible because the
transport still moved most of the bytes. Two of the twelve stalls this cycle chased could have
been this shape.

What that leaves for the sealed striped integration failure is exactly what the unit harness does
not have: the stripe layer's own loop, the echo backend, real sockets, and eight mebibytes.

### The send run is a local optimum at 44, and the lever is somewhere else (2026-11-05)

Both directions from the shipping constant were measured, one `soak` each, same build
otherwise:

| `SEND_RUN` | slow opens | of those p50 / p90 | first stage p50 / p99 | its bulk |
|---|---|---|---|---|
| 8 | 5 530 | 10.53 / **22.76** ms | 13.10 / 68.03 | 6.20 Gbit/s |
| **44** | **213** | 10.84 / 32.14 | **2.51 / 23.03** | **6.89** |
| 96 | 7 853 | 16.71 / **35.27** (max 1 619) | **58.78 / 165.64** | 3.32 |

So 44 is a **local optimum on both sides** — twenty-six times fewer slow opens than eight
packets a run, thirty-seven times fewer than ninety-six — and the first stage is three to
twenty times better at 44 than at either neighbour. The burst geometry is not the lever, and the
constant as shipped is the best of the three by a wide margin.

That is the second hypothesis about the handshake's 20–180 ms tail to be tested and rejected
(the first was an RTO, the third a socket queue). What is left is what the cycle started with:
the open is three round trips of tiny packets, and the only way to make three into one is
sealing — which is built, tested, and blocked by the striped path's deadlock.

### Smaller send bursts are worse: the A/B, and its numbers (2026-11-05)

The last hypothesis had a mechanism and a one-constant test: the sender builds up to `SEND_RUN`
packets (44, about 61 KB) per syscall, so an interactive packet that arrives mid-run waits for
the run. Reduce the run and the wait should shrink.

Measured, same build otherwise, one `soak` each:

| | `SEND_RUN = 44` | `SEND_RUN = 8` |
|---|---|---|
| slow opens in the run | **213** | **5 530** |
| of those, p50 / p90 | 10.84 / 32.14 ms | 10.53 / **22.76** ms |
| first stage p50 / p99 | **2.51 / 23.03** | **13.10 / 68.03** |
| first stage bulk | 6.89 Gbit/s | 6.20 |

So the tail's *shape* did improve — p90 32.14 to 22.76, which is the mechanism working as
predicted — and everything else got worse: **twenty-six times as many opens were slow**, the
stage's median rose fivefold and its p99 tripled. Five and a half times as many syscalls for the
same bytes is more CPU per byte, and every small packet now queues behind more processing rather
than less.

Reverted. The hypothesis was half right (a shorter run does shorten the worst wait) and the trade
is wrong by a wide margin, which is exactly what the objective's priority order — latency, then
throughput, then memory, then processor — says to check rather than assume: a change can improve
the statistic it was aimed at and lose on the metric that matters.

### The handshake's tail is neither a timer nor a queue (2026-11-05)

Two mechanisms were tested against the slow opens and both are excluded.

**Not an RTO.** `MIN_RTO_SMALL_FLIGHT` is 20 ms and the slow opens' p50 is 20.18 ms, which looked
like a lost handshake packet repaired by the timer — a distribution quantised at 20, 40, 60 ms.
It is not: the 763 slow opens decay smoothly (0–10 ms 194, 10–20 201, 20–30 158, 30–40 82,
40–50 33, and a long tail to 190). A timer produces spikes at its own period; this is a wait
proportional to something continuously varying.

**Not a socket queue.** The receive queue of every UDP socket in the server's namespace, sampled
thirty times a second apart during a load stage, read **0** every time. The receiver is keeping
up; there is nothing piled in front of the handshake's datagram at the kernel.

So the wait is inside the tool, not in the kernel: the open's three round trips are each fast
(`accept → pairing` 0.058 ms, `paired → first byte` 0.022 ms, `resume` 4–14 µs), and yet the
whole open occasionally takes 20 to 180 ms with nothing refused, nothing lost and no queue.

One more fair pair, meanwhile — the second in the cycle, and both stages passing with bulk on
both:

| stage | p50 | p99 | bulk |
|---|---|---|---|
| `clean` | 2.51 ms | **23.03** | **6.89 Gbit/s** |
| `clean#2` | 2.39 | **7.38** | **5.55 Gbit/s** |

So the fair pair exists twice now, and both times the failing reading came from the run whose
spine was faster (8.83 against 6.89). The relationship the six-run ledger found — pass below
about 8 Gbit/s, fail above — still holds, and the handshake's unexplained 20–180 ms tail is what
the failure is made of.

### The gate's one failing cell is the handshake's tail, and sealing is its fix (2026-11-05)

The sweep's failing stage decomposes cleanly, and it is not the regime the earlier entries
chased:

| cell | p50 median | p99 median | p99 p90 | verdict |
|---|---|---|---|---|
| `clean` | **3.0 ms** | **51.4** | 116.0 | fails (81.8 for the cell) |
| `clean#2` | 2.5 | 3.2 | 23.8 | passes (23.5) |

So the *median* arrival is the same in both stages — three milliseconds — and only the **tail**
differs. The earlier "bad mode" (p50 8.8–11.3, the ratchet hypothesis) is a different phenomenon
and is not what the gate is failing on.

The tail is the handshake, counted by the client's own INFO line: in that stage **102 slow
opens**, `open_us` p50 **20.18 ms**, p90 45.82, max 101.45, against three thousand arrivals —
3.4 % of them paying a 20-to-100 ms stream open, which is what a p99 of 51 ms is made of.

That closes the loop this cycle opened in round seven: the arrival's cost is its per-stream
handshake (three round trips), the handshake's tail is what pushes the first stage's p99 over
the line, and **sealing deletes two of those three round trips**. It is built, tested, and off by
default because enabling it deadlocks the striped path in about five runs out of eight — and
that deadlock is now the single bug between this cycle and a gate pass.

Two of its causes are already fixed (a refused packet and a failed seal each consuming a packet
number for ever). What remains is inside the stripe layer over sealed streams, where the packet
is clamped to `MAX_SEALED_PAYLOAD` while the frame is 32 KiB, and the reorder loop's `written`
is the byte count the visitor's EOF is measured against.

### The release gate is one SLO cell short, and it says so itself (2026-11-05)

A full-scale `sweep` on the product arm, then `just bench gate` on the record. The gate's own
verdict, verbatim:

```
coverage: 18 measured arm/scenario/cell triples
endpoint invariant: 16 samples carry a dialed endpoint
SLO: 2 clean-stage cells gated
drowned cells: 0 refused
drift: checked the run cells' slopes and wedges
capacity: l4 sustained 8 of 8 stream(s)
gate: 1 FAILURE(S)
  - SLO: l4 / timeline[clean] p99 81.8327 ms > 50.0 ms on a clean stage
```

So of everything the objective asks for, the gate now fails **one** item:

- **coverage** — every declared cell measured, nothing refused or drowned;
- **endpoint invariant** — every throughput sample dialled the tool, not the backend;
- **drift and wedge axes** — checked, clean;
- **capacity — 8 of 8**, with all eight levels inside the SLO (p99 23.4–24.4 ms on every one);
- **SLO** — `timeline[clean#2]` **passes at 23.46 ms**; `timeline[clean]` fails at **81.83 ms**.

And the run's own shape says what that failure is: the first stage carried **8.83 Gbit/s** of bulk
and the last carried none (the wedge took it), so the stage that fails is the one with the fast
spine — the same bimodality the last four soaks showed at 12.8, 16.0, 17.0 and 23.3–23.8 ms, and
the same regime the path-scaled ceiling moved from 135–291 ms into the twenties.

That is the whole of the release argument now: **one cell, in one regime, and the mechanism of
that regime's bad mode is the last thing standing between this cycle and a gate pass.** The
objective's own conditions are otherwise met in a single record for the first time — capacity,
coverage, the endpoint invariant, drift, `just check` and the L3 acceptance.

### The wedge again, this time with the carrier's counters silent (2026-11-05)

A `soak` on the build that carries the L3 fix. Its first stage is the best this cycle has
recorded — **p50 2.54 ms, p99 16.03 ms at 8.0 Gbit/s** — and its last clean stage wedges
exactly as before (`throughput_gbps` absent).

The per-visitor line, in that stage's window, and the counters beside it:

| reading | value |
|---|---|
| visitors forwarded and returned (`stop_pool=false`) | **9 486** — every probe and churn visitor |
| visitors dropped at pairing | **0** |
| pairing budgets expired | **0** |
| `mhd stalled` lines in the window | **0** |

So in this wedge the carrier is not merely quiet, it is **healthy by its own accounting**: no
unsent bytes, no unacknowledged flight, no refusals, no dropped visitors, and sixty-odd
micro-visitors a second served normally throughout. The four bulk connections are accepted,
paired and forwarding — they simply never drain, and nothing in the carrier is holding their
bytes.

That eliminates the last carrier-side explanation this file had for the wedge. What remains is
the path *before* the carrier: a forwarding loop that is neither reading its visitor (whose
socket is full, which is why `iperf3` blocks) nor writing into the carrier (which would show as
an unsent or a stall). A loop parked in neither of those places is parked on a queue between
them, and that is where the next instrument goes.

The first stage's 16.03 ms is worth recording on its own: with the path-scaled ceiling, a
Gbit/s-8 run has now read 12.8, 16.0, 17.0 and 23.3–23.8 ms, against 135–291 ms before it. The
bad mode (102–122) has not recurred in the last four runs, though four runs is not yet a claim.

### The bulk bytes are not in the carrier at all (2026-11-05)

The instrument pre-registered last round answers its first question on the first wedged
stage it sees, and the answer is not the one the previous entry guessed. During a wedged
`rtt100` (a hundred milliseconds of delay, four bulk flows, the interactive probe serving
normally):

| reading | client | server |
|---|---|---|
| receive backlog, all four lanes | **0 bytes, every second** | **0 bytes** |
| in flight | — | **0**, with `cwnd` 9.8–15.9 MB and `slow_start=false` |
| stall lines | none | none |
| bulk first-frame reads in that window | **0** | **0** |
| stripe groups started in that window | 600 (all 64-byte requests) | 600 |

So nothing has arrived at the client because **nothing ever left the server**, and nothing
left the server because it had nothing to send: `flight` is zero, its own receive backlog is
zero, and it is not refusing anything. The four bulk connections' bytes are sitting in the
**visitor's** socket, unread — while six hundred small connections through the same lanes are
served normally, in microseconds, all through the same stage.

That corrects round 13's sentence. The 697 bulk frames counted there were a **whole-run**
count compared against a **per-stage** claim — the same population error this file keeps
making, in the same direction — and in the wedged stage itself the count is zero on both
sides. The wedge is **upstream of the carrier's data path**: the tool stops reading four
bulk visitor connections, and everything after that is a consequence.

The leading suspect is now in `stripe.rs` and not in the carrier at all: `spawn_grace` arms
when **either** direction of a group ends and aborts the other after **ten seconds without
activity**. A bulk visitor that has been idle for ten seconds — which is exactly what a
hundred-millisecond shaped path produces — has its read side cut, and from then on the
visitor's socket is never drained again. That would look precisely like this, on both
carriers, and only for connections that outlive a shaped stage.

It is a hypothesis with a mechanism and a test: instrument whether `spawn_grace` fires on a
`send_loop` that is still owed bytes (the counters it prints already carry `highest` and
`delivered`; it needs the visitor's read side and the group's own byte count), and if it
does, the grace must be **conditioned on the dying direction's work being done** rather than
on the survivor being busy.

### The fair pair, at last: both clean stages with bulk, both inside the SLO (2026-11-05)

The same `soak` schedule, one hour later, does not wedge — and it is the first run in this
cycle where **both clean stages carried bulk and both met the SLO**:

| stage | p50 | p99 | throughput |
|---|---|---|---|
| `clean` (first) | 2.52 ms | **23.51** | **6.84 Gbit/s** |
| `clean#2` (last) | 2.24 ms | **3.51** | **5.95 Gbit/s** |

That is the pair the previous entry said had never existed, and it changes two sentences in
this file:

- **mhd's own first-versus-last gap is 23.5 → 3.5 ms at p99**, a factor of 6.7, with both
  inside the 50 ms limit — not the 302 → 22 and 146 → 23 this file has been quoting, which
  compared a stage with four bulk flows against one whose bulk instrument had died;
- **the target is reachable on this carrier.** Two clean stages, one run, real bulk both
  times, both passing. What is not reachable is *stability*: the wedge appears in about
  half of these runs, and when it does the stage loses its whole bulk spine.

So the objective's condition 1 is not blocked by a design flaw in the arrival path; it is
blocked by an intermittent loss of the bulk spine after a shaped stage, which is the
defect the previous entry localised between the server's stripe write and the client's
stripe read.

The instrument the previous entry pre-registered is in this build — the per-second `mhd
window` line now carries `backlog_streams`, `backlog_bytes`, `worst_stream` and
`worst_ready_ms`, the receive-side mirror of the send-side numbers — and this run has no
wedged stage to show it. It stays installed for the next one.

### The bulk wedge is inside the carrier: one second, then nothing (2026-11-05)

Reproduced on a **fifteen-minute** schedule instead of the thirty-minute sweep — the
`soak` timeline ends with a clean stage after `loss1`, `rtt100` and `loss5`, and its last
clean stage loses the bulk exactly as the sweep's does (`gbps` absent,
`control socket has closed unexpectedly`). That makes this defect reproducible in a
quarter of an hour with debug traces on.

Four instruments agree on the same minute, and they are the first set that does:

| what | evidence |
|---|---|
| iperf3's own JSON, second by second | second 0–1: **4 063 232 bytes** across four streams; second 1 through 120: **0,0,0…**, then the sink's idle timeout closes the control socket |
| the sink | `0.00 Bytes` on all four streams for the whole 180-second stage |
| the server's stripe send loops | **697** first-frame reads of `bytes=32768` — the visitor's bulk was read and framed |
| the client's stripe layer | **0** bulk frames ever; its largest `receive direction finished` in that window is `frames=4 written=167` |

So ~3.9 MB is accepted by the tool, read by the server's forwarding loops, framed and
handed to the carrier — and the client's stripe reader never sees a single bulk frame,
while 64-byte requests through the same lanes are served continuously in microseconds.
The bulk bytes are sitting unread on the client's streams (the earlier wedge read
`credit=16` — the peer's window 16 bytes from exhausted — which is the same fact from the
other side).

**Where that leaves the search**: not the harness (measured twice), not the sink, not the
server's forwarding, not the arrival path. It is between the server's stripe write and the
client's stripe read, i.e. the carrier's own data path for four concurrent bulk streams
after a lossy stage — and the client holds the bytes without a reader consuming them.
The next instrument is the client's per-stream reader state for those four streams
(`Conn::backlog`, `parked_at` and `wakes` already exist): a reader that is parked for
120 seconds with a heap of bytes behind it names the defect that a park of two seconds
did not.

### Found: an arrival could not write a byte while bulk held the window (2026-11-05)

The first stage's tail has a counter of its own, and the full-scale debug run prints it:
**18 492 `mhd opening refused` in the first two minutes** of the run, of 18 496 in the whole
run, and every one of them names the same bound:

```
opening refused: credit=11726577 window=0 held=28962992
                 flight=3847348 cwnd=2097152
```

`window` is `cwnd - flight`: the ceiling is `SLOW_START_CEILING` (2 MiB), the bulk spine's
flight is **3.85 MB**, so the difference is **zero** — and at zero an arriving stream cannot
write its handshake, whatever its credit (11.7 MB) or the buffer (28 MB) would allow. The
reserve that exists for exactly this case could not help, because it was a *share* of the
minimum and the minimum was already zero.

An opening now **overdraws the congestion window by `OPEN_RESERVE`**: the reserve is a lane
budget it can spend, not a fraction of what happens to be left. Measured on the short
timeline, which reproduces the state in three minutes:

| | `mhd opening refused` |
|---|---|
| before | **2 410** |
| after | **1** |

That counter is the honest evidence for this change, and it is the first one in this cycle
that moved by three orders of magnitude on the mechanism it names. What it does **not**
establish is latency: the paired A/B on that cell is **unresolvable** — the run's own A/A
twin differs by **+133.65 %** on `timeline[clean] rtt_p99_ms` and the other arm by
+115.59 %, and the model's planning line says about **53 rounds** would be needed to resolve
its 15 % floor. So the change is kept because an arrival that cannot write is a defect by
construction, and the latency is recorded as *not measured* rather than as improved.

Also worth recording, because it is cheap and it discriminates: the refusal line itself,
counted, is a better first-stage instrument than the probe's p99 — it is a carrier counter
with no probe in it.

### The first clean stage fails for TCP too, so it is not mhd's (2026-11-05)

The comparison objective condition 2 asks for, on the cell that decides the release,
and the first time this cycle has run it: the same full-scale timeline, the same host
(load 0.73), with `MOLEHILL_DATA_CARRIER=tcp`.

| stage | p50 | p99 |
|---|---|---|
| `clean` (first) | 29.83 ms | **120.0 ms** |
| `clean#2` (last) | **1.22 ms** | **19.54 ms** |

**TCP misses the 50 ms SLO on the first clean stage by the same factor mhd does**, with
the same first-versus-last gap. A yamux-over-TCP data path and an mhd lane share almost
nothing except the topology, the probes and the fact that *a tool is in the path at all*
— so the first stage's tail is not a property of either carrier's design.

Where TCP's version of it lives is measured, because the probe's split was in this run:

| window | connect p50 | echo p50 |
|---|---|---|
| t = 0–25 s | 0.223 ms | 3.036 ms |
| t = 25–50 s | 0.281 | **31.999** |
| t = 50–150 s | 0.28–0.30 | 32.5–34.4 |

`connect` never moves — so this is not the accept queue, not ephemeral-port exhaustion,
not the listener. The **wait for the echo steps up tenfold at t ≈ 25 s and stays there**,
which is a small request queueing behind the bulk spine: the same sentence this file
wrote about mhd, in a different carrier, an order of magnitude larger.

Two consequences for the objective, and they are uncomfortable but they are what the
data says:

- **condition 1 as stated — both clean stages at or under 50 ms — is not currently
  reached by either carrier on this host**, so it cannot be used to judge mhd until it
  is reached by TCP as well. "mhd must pass a cell that TCP fails" is not a carrier
  property;
- the first stage measures **cold start plus the bulk spine**, which the earlier
  `--warmup-rounds 1` diagnostic already showed (34.7 ms against 135.3), and which the
  archive records as a method property. The honest options are to state the SLO on a
  stage that is not the run's first, or to make the *product* avoid accepting visitors
  until it can serve them promptly — and the second is a real change, not a
  measurement, because it is the same for both carriers.

What is mhd's own, and remains: on the warm stage it reads 22.5 ms at p99 against TCP's
19.5, and its arrival performs a per-stream handshake that TCP does not — which is the
11.5 ms term the accounting closed last round, and the reason sealing is still the lever
worth unblocking.

### The plan's stages, as they actually stand (2026-11-04)

The plan was re-cut this cycle: instead of one fixed chain (sealing → RACK/PTO →
scheduler → BBR), it runs four independent work fronts and lets each measurement
say which one is next. Everything below is judged by its own A/B against the
run's own floor, and nothing is called a win because it is cleverer.

| stage | state |
|---|---|
| **A1** load-generator health | **done.** The bench echo backend's two directions are decoupled (16–19 → 22–27 MiB absorbed while its peer reads nothing), and the change's *justification* was refuted on the way: 16 MiB of room means an 8 MiB echo cannot have blocked, so "the backend was the stall" is withdrawn. A permanent check runs it: `bench/src/bench/utils/echo_check.py` |
| **A2** a stage's shape | **done.** Every staged cell now carries `interactive_per_s` (per-second n/p50/p99/max/failures), `interactive_cadence` (the probe's own intervals and missed cadences), the churn rate asked for beside the one achieved, and the window's edges in unix time. `series_logs` names the probes' files instead of pasting every sample into the results file as text |
| **A3** what the gate can see | **done.** The SLO is judged on *held stages* only (`hold_s`), a clean stage with no round trip **fails** instead of passing silently, and a ramp that stops short of `streams_max` fails instead of printing a line. Re-checked against the published `v0.11.2` record, which still reads `gate: PASS` |
| **B** position of the first stage | **done, and it is the carrier's.** One full-scale run, same binary, control and molehill together: control's first `clean` stage p99 **2.57 ms**, molehill's **112.85 ms** (44×), 38× its own p50. The server's `paired_p99_us` 112 454 agrees to 0.4 %. *The follow-up run that was meant to confirm a fix is **void** and is not a regression: its host carried a five-minute load average of **4.74** against **0.84** in the reference run, and its first stage read p50 66.9 ms — that run measured the host, not the carrier* |
| **B2** the harness's four lost stages | **half done.** A failed bulk instrument no longer deletes its stage's arrivals (four cells per run were being discarded, including the last clean stage); the `iperf3` wedge itself after `loss5` is still open |
| **C** no per-visitor handshake | **wired, tested, OFF.** The root is now the handshake's *secret* split salted with its transcript (not the transcript alone). Sealing deadlocks a four-stripe group: 3 of 6 runs pass with it on, 6 of 6 with it off, so it ships behind `MOLEHILL_MHD_SEAL` until that is fixed |
| **D** fewer round trips per arrival | not started; `StartForward` speculation is the cheap half, a server-initiated visitor stream the large one |
| **E** recovery and control | not started. The refusal mix is honest (credit 64 %, window 29 %, age gate 7.5 % at 0.3 scale), and the age gate has since been caught blocking a stream whose flight was ~0 |
| **F** scheduling | **not refuted, and now the strongest remaining lever**: the tail was measured as a small record queueing on a lane carrying bulk (`us == hs_us`, share 1.000), which is what class-aware transmit order exists to prevent |
| **G** release gate | **fails on one thing**: the first clean stage's p99, whose within-campaign spread is larger than the 50 ms limit. Coverage is complete when the harness cooperates |

The gate, measured on the last full-scale run before this cycle's changes:
eight of eight stages measured, `clean#2` 1.89 / 3.91 ms, `clean` 6.06 / **134.1** ms
— the first stage is the only SLO failure and the only thing between this branch
and a release record.

**What the first stage costs, and where**: an arrival's channel spends **its whole
open inside the per-stream Noise handshake** — `us == hs_us` with a share of
1.000 at p50 and p90, `ack_us` zero, and on the server `hs_us == total_us` with
the selector and hello at zero — while credit reads 16.5 MB and the buffer sits
at its ceiling. On an idle lane that handshake is 1.8 ms; under bulk its median
among the slow openings is **11.5 ms** and its p99 128.7, because two small
records have to be scheduled on a lane that is carrying megabytes. The count of
round trips is what sealing deletes, and *when* those records get the wire is
what scheduling owns.


## The goal, and what may not change

The objective: **make `mhd` the more reasonable first carrier — better than
TCP where it can be, and honest where it is not.** The trade order is fixed and
binding:

> **latency > throughput > memory > processor**

Ties are broken by that order, not by which number is easier to move.

What may **not** change, under any redesign below:

- **The configuration model and every field's meaning.** No key added, removed,
  renamed, re-defaulted or re-scoped. `src/config/` may change only prose that
  names the carrier (`KCP` → `mhd` in a doc comment or a removal message).
  `diverged`/`REMOVED_KEYS` behaviour stays exactly as it is.
- **`Carrier::{Tcp, Mhd}`, the `"mhd"` wire tag, the TCP fallback path**, and
  the runtime choice between them.
- **No new privilege and no new `unsafe`.** The crate-wide deny and the
  per-module opt-ins of [docs/lint-policy.md](docs/lint-policy.md) hold.
- **Log levels** follow the contract in
  [docs/configuration.md](docs/configuration.md) — `tests/log_budget_test.rs`
  fails a healthy run that emits one `WARN`/`ERROR`.
- **Both languages** for user-facing pages, in the same commit.
- **A number is only a claim** when it clears its metric's materiality floor
  *and* the run's own measured noise (AGENTS.md §10). A mechanism that would
  explain a number is not evidence that it produced it.

## What is measured, on this tree

Everything below was produced by the bench model on this host. Absolute values
describe this machine; the shapes and the orderings travel.

### Warm: a connection that already exists

| cell | mhd | TCP | note |
|---|---|---|---|
| `rr-16` p50 / p99 | **0.46 / 1.32 ms** | 0.44 / 1.34 | at parity |
| `rr-1` p50 / p99 | 0.098 / 0.130 ms | — | |
| `bulk-n` throughput | **10.98–11.49 Gbit/s** | 10.90 | A/A twin 10.88–11.53 |
| `capacity_streams` | **8 of 8** (good window) | 8 | = `streams_max` |

The last clean stage of a 0.2-scale timeline — the warm state — is the best
arrival measurement this cycle has:

| stage | n | p50 | p90 | p99 |
|---|---|---|---|---|
| `clean#2` | 850 | **1.87** | **2.27** | 15.07 |
| TCP, same cell | — | 2.24 | 2.59 | — |

**At the median and the ninetieth percentile the warm carrier beats TCP.**

### Cold: a visitor that has just arrived

`churn-16` is **sixteen concurrent workers, each opening a fresh connection per
request**: 6 400 arrivals in a round, about **four thousand a second**. (The
`churn_per_s: 16` field in the model's parameters belongs to the *timeline's*
churn probe — a different instrument at one arrival every 62 ms. Reading it as
this cell's rate is what withdrew the concurrency explanation for a dozen rounds,
and it was wrong; see the correction below.)

| cell | mhd | TCP |
|---|---|---|
| `churn-16` p50 | 1.95–2.51 ms | 2.24 |
| `churn-16` p99 | **17.97–21.75 ms** | **2.58** |
| `churn-16` rate | 3 500–4 061 /s | 6 728 /s |

**Where an arrival's time goes, in one second of a measured round.** Every stage
is timed behind `MOLEHILL_MHD_STATS`, at one five-millisecond-or-finer scale, as
a per-second window:

| segment | p50 | p99 | max |
|---|---|---|---|
| accepting the visitor (`accept_us`) | 15 µs | — | 133 µs |
| opening a channel: placement + Noise handshake + prologue (`open_us`) | **1.73 ms** | 4.66 ms | 7.6 ms |
| **waiting for a channel to be provided (`wait_us`)** | 0.95 ms | **20.1 ms** | 50.9 ms |
| writing the pairing command on it (`write_us`) | 2 µs | 7 µs | 15 µs |
| the visitor's first byte after pairing (`first_*`) | 13 µs | 117 µs | 575 µs |

**Read the two bold rows together, because they decide the diagnosis.** The
pairing's p99 is more than twice the *worst* open the client performed in the
same second, so the tail is not a slow handshake. And the pairing's *median* is
below the open's median, which is only possible when a visitor is served by a
channel that was already in flight for somebody else.

That is the mechanism: **channels are interchangeable.** The client opens one per
request, hands it to a shared queue, and `pair_striped_group` takes whichever
channel is at the head — so with sixteen opens in flight, a visitor's own channel
can be delivered to another visitor and the waits add up. The control is the same
pairing at sixteen arrivals a second with no bulk beside it: **one pairing in
~480** over five milliseconds, against 471 of ~480 in a scheduled stage's first
`clean`. Bulk is what puts a queue in front of an arrival, and the queue's depth
is what one open costs.

The four explanations this replaces, and why each is now retired: the tail is not
a stall (the multi-second ones are fixed), not the handshake alone (a 4.66 ms p99
cannot make a 20.1 ms one), not the command write (2 µs), and not the forwarding
loop (13 µs after pairing).

The first stage of a timeline is where this lands as an SLO failure. Across the
23 timeline records this cycle produced:

| | first `clean` | last `clean#2` |
|---|---|---|
| p99 median | **179.9 ms** | **6.06 ms** |
| p99 range | 36.3 – 800.9 | 3.2 – 375.7 |
| under the 50 ms SLO | **3 of 23** | **18 of 21** |

and per-stage, in one 0.2-scale run:

| stage | n | p50 | p90 | p99 |
|---|---|---|---|---|
| `clean` (first) | 621 | **12.13** | 51.65 | 103.70 |
| `clean#2` (last) | 850 | **1.87** | 2.27 | 15.07 |

**The same binary, the same stage definition, sixty times apart at the median,
in one run.** That is the defect the gate reports, and it is why a single
"carrier is fast/slow" summary is not a usable statement.

### What the old instruments still say

The pairing diagnostic fires once per successful pairing, at and past 5 ms,
behind `MOLEHILL_MHD_STATS=1`. It is the *conditional* distribution of the slow
subset, not the population's — 25 % of arrivals in a `churn-16` round — so it is
read as "how bad is the slow part", never as a median:

| field | p50 | p90 | p99 | max |
|---|---|---|---|---|
| `accept_us` | 17 µs | 25 µs | 37 µs | 62 µs |
| `paired_us`, on pairings over 5 ms | 8.0 ms | 17.6 ms | 29.1 ms | 57.8 ms |

and per stage at 0.2 scale, out of ~480 visitors, the count of pairings over
5 ms: `clean` **471**, `loss1` 243, `loss5` 4, `clean#2` **1**. The probe's own
rate field confirms the last stage was fully loaded (`churn_per_s` 15.99–16.005
in `clean#2` in all 23 records), so no stage's numbers are an idle artefact.

### What landed this cycle, each with its own measurement

| change | evidence |
|---|---|
| one-flight open (hello and ack inside the handshake) | open 3 313 → **927 µs** idle; `wire_per_visitor_byte` −15 % |
| the flight may not outlive its timer, openings exempt | capacity worst arrival **3 000–5 000 → ≤ 204 ms**; `loss5` tail 1 427 → **862 ms** in an interleaved A/B |
| a lane needs an *opening* datagram | 29 stray lanes → **0**, by a test rather than by a run |
| a stage restores its own condition | a cascade that cost five stages now costs none |
| the reorder loop wakes itself; a parked buffer has a deadline | three rounds in six read p99 7.7–49 ms where none did |
| pairing patience 25 s → 10 s (`PAIR_ATTEMPTS` 2) | no regression on three cells |
| three diagnostics at one 5 ms scale | the tail became visible at all; it had been measured 4× too coarse |
| `Cargo.lock` records 1.0.0 | a release build now leaves the tree clean |

## What is open

Each item states its own gate. A change that misses its gate is reverted, and
the reason is recorded — not tuned around.

### O1. The first `clean` stage fails the SLO (latency, blocking)

`timeline[clean]` p99 is 36–801 ms against a 50 ms bar, in 20 of 23 records,
and the first stage's median is 12.13 ms against the last stage's 1.87 in the
same run. The mechanism is localised (above): a queue of interchangeable channels
in front of an arrival, whose depth is what one open costs.

- **Gate**: both clean stages independently under 50 ms p99 with error rate
  under 0.5 %, in a full-scale run, with the first stage's hold unchanged.
- **Not allowed**: a warm-up round, a shortened first stage, dropping the stage,
  or averaging the two.

### O2. `churn-16` p99 is 8× TCP (latency)

17.97–21.75 ms against 2.58 at ~4 000 arrivals a second from sixteen concurrent
workers. The median is level (1.95–2.51 against 2.24). Same mechanism as O1, on a
cell with no bulk: the pairing's p99 is 20.1 ms while the worst open in the same
second is 7.6 ms, so the waits are adding up across concurrent arrivals rather
than each one being slow.

- **Gate**: within the run's own A/A floor of TCP's 2.58 on an interleaved A/B,
  with `rr-16` and `bulk-n` unmoved.

### O3. Throughput under loss and on a long path (throughput)

`loss1` p99 is one RTO for every tool in the record (molehill 1 076, frp 1 065,
rathole 1 058), so a p99 claim there is a claim about TCP's retransmission, not
about this carrier. `rtt100`'s 1.70 Gbit/s is 16 MiB ÷ 400 ms over four lanes —
the window is the ceiling by arithmetic.

- **Gate**: `bulk-n` within 15 % of its baseline on an interleaved A/B, and no
  shaped stage regressed, when recovery or the window changes.

### O4. The harness loses cells to an intermittent sink failure

A `control socket has closed unexpectedly` costs 0–3 stages per run,
arm-independently, and one `rathole` capacity cell aborted after 7.83 s. This is
not the carrier: it hits every arm, the tool's group outlives the client's own
exit, and a stage's condition is now restored properly.

- **Gate**: a full sweep with no missing cell for any arm. Until then a
  coverage failure is an instrument result, not a product one.

### O5. Constants nothing measures

`MAX_CWND`, `MAX_BUFFERED`, `OPEN_RESERVE`, `SEND_RUN`, `KEEPALIVE`,
`DEAD_AFTER` are unexercised by every measurement this cycle made. The audit
that says so is in the archive ("P4′: the constants audited").

- **Gate**: a constant is changed only with the cell that shows it binding.

## The plan

Ordered, each stage gated on its own measurement, each revertible alone.
Configuration keys do not move in any of them.

### Stage 0 — this page (done here)

Consolidate the handoff; keep the negative results as a table and the narrative
in the archive.

### Stage 1 — attribution (done, 2026-11-03)

The arrival is now attributable to five named segments, each timed behind
`MOLEHILL_MHD_STATS` and reported per second: `accept_us`, `open_us` (placement
plus handshake plus prologue), `wait_us` (the pairing's wait for a channel),
`write_us` (the command on it) and the existing `first_*` (the visitor's first
byte afterwards). The numbers are in "Where an arrival's time goes" above, and
they answer the question Stage 1 was written for: the tail is a queue of
interchangeable channels, not one slow step.

What it took, and what is worth keeping from it:

- Two instruments, both at the same five-millisecond-or-finer scale as the older
  two, plus a per-second window on each end — because a per-event line at a
  threshold shows the tail and never the population, and the population is what
  distinguishes "a few slow opens" from "every open is slow".
- Three instruments had to be re-scaled first: the client's lane open (5 ms), the
  client's open-to-prologue (was 100 ms), the client's task-scheduling delay (was
  100 ms), and both windows (were 10 s, against a 5 s cell). An instrument coarser
  than the thing it measures reports a healthy system — four times in this cycle.
- One earlier conclusion had to be withdrawn *before* it could mislead the plan:
  that `churn-16` runs at sixteen arrivals a second. It runs at about four
  thousand, and the `churn_per_s: 16` field belongs to the timeline's probe.

Measurement discipline from here (unchanged from AGENTS.md §10, restated because
it is what the next stages will be judged by): per-stage numbers, no added
warm-up, the TCP contrast from `MOLEHILL_DATA_CARRIER=tcp` on the same build
family, every optimisation an interleaved `--ab-arm`/`--binary-b` A/B with its
own A/A twin, and artifacts under `target/evidence/` or `~/tmp/`.

### Stage 2 — a channel belongs to the request that asked for it (latency, first)

**This is the change the measurements above ask for, and it is ahead of the
handshake work** because the tail is queueing, not a slow step.

Today a channel is anonymous: the client opens one per request, drops it in a
shared queue, and `pair_striped_group` takes whichever is at the head. So a
visitor's own channel can be delivered to another visitor, and with sixteen opens
in flight the waits add up. Give the pairing an identity:

1. A channel names the request it was opened for. The group id already exists on
   the wire and in both ends' hands; it goes into the tunnel prologue next to the
   service and tunnel ids, which is four bytes and no new key.
2. The server reads it where it already reads the prologue and delivers the
   channel to **that group's waiter**, with the shared queue kept for plain
   visitors. A waiter registers before its request is written, so a channel that
   arrives early is not lost.
3. An abandoned group is released as it is today (`ReleaseGroup`, plus a TTL),
   so a visitor that gives up cannot leak a waiter.

This also removes a latent correctness hole rather than only a latency one: with
`stripes > 1` the current FIFO can hand a channel opened for group A to group B,
which is exactly the "K stripes on K distinct tunnels" invariant the client's
placement works to keep.

- **Gate**: `churn-16` p99 falls toward TCP's 2.58 ms; the pairing's p99 stops
  exceeding the open's p99; `bulk-n` inside 15 %; both clean stages improved; the
  striped integration test still passes with `stripes > 1`.
- **Revert**: no movement beyond the run's floor, or any bulk regression.
- **Tests first**: a channel for group A never satisfies group B; an early
  arrival is held for its waiter; an abandoned group leaks no waiter; a plain
  visitor still takes the shared queue.

### Measured: identity delivery does not move the tail (2026-11-03, `ed6fa76`)

The change the queueing story predicted, built and interleaved against a build
of `78923cb` on `churn-16` (4 rounds, `--aa`, `--ab-arm l4 --binary-b`):

| arm | p50 | p99 | max | rate |
|---|---|---|---|---|
| `l4` (identity delivery) | 2.28 ms | **19.52 ms** | 41.0 | 3 688 /s |
| `l4~aa` (the same build again) | 2.84 | 21.49 | 51.1 | 3 235 /s |
| `l4~b` (before) | 2.57 | 21.74 | 44.3 | 3 489 /s |

**No claim**: the run's own p99 floor is 8.19 %, the point estimate is −10 %,
and the A/A twin sits *between* the two arms. The traffic floor is 12.2 % from
four rounds.

So the exchange-of-channels story is refuted as the cause of the tail, though it
was the right reading of the earlier distributions: the pairing median does sit
below the open median, and with identity delivery it no longer can — yet the
tail is unchanged. **The 15 ms that separates the pairing's p99 from the open's
p99 is somewhere else**, and it is now the only unexplained interval left:

- not accept (15 µs), not the command write (2 µs), not the first byte (13 µs);
- not the open, whose p99 is 4.66 ms;
- and not the exchange, which this A/B removed.

The change is **kept as a correctness fix and as the identity the next
instrument needs**, explicitly not as the fix for the tail — the same disposition
the small-flight timer got earlier. It closes a real hole: with `stripes > 1` the
old FIFO could hand a channel opened for group A to group B, which is exactly the
"K stripes on K distinct tunnels" invariant the client's placement keeps.

### The residual, and three candidates excluded (2026-11-03)

With the group identity in place the two ends can finally be matched **by
identity** rather than by magnitude. Twelve events had both ends' diagnostics in
one round, and they bound the two intervals that were left:

| interval | p50 | p99 | max |
|---|---|---|---|
| control request: server enqueued → client reads it | **132 µs** | 357 µs | 357 µs |
| prologue written → server has paired | **3.0 ms** | 12.2 ms | 12.2 ms |

and the second one is not any of the things inside it, each measured on its own:

| candidate | reading | verdict |
|---|---|---|
| the send-buffer dwell: bytes accepted → a packet carries them | client p50 **5 µs**, p99 498 µs, max 591 µs; server p50 4 µs, p99 25 µs | **excluded** |
| the per-tunnel accept loop reading prologues serially | **0** route lines over 5 ms in two rounds | **excluded** |
| the exchange of channels between visitors | interleaved A/B: p99 19.52 against 21.74, twin 21.49 | **excluded as the cause** |

So every instrument inside the path reads microseconds while the arrival's tail
is twenty milliseconds — the same shape the archive records for the earlier
search: **the time is between the stages, not inside them.** Two intervals are
still unmeasured and both are scheduling rather than work:

1. the client's pump posting a new stream's first packet → the server's listener
   dispatcher handing that stream to the lane's accept loop;
2. the lane's accept loop waking the stream's reader task and that task being
   polled.

The instruments that would separate them are a wake count and a poll-gap on the
accept path — the same pair that was needed for the reader park earlier.

### Every segment measured, and the one interval left (2026-11-03)

Ten instruments, each at five milliseconds or finer and each behind the stats
switch. In one second of a `churn-16` round (~3 600 arrivals):

| segment | p50 | p99 | max |
|---|---|---|---|
| accepting the visitor | 16 µs | 80 µs | 153 µs |
| control request crosses to the client | 132 µs | 357 µs | 357 µs |
| the client's channel open + prologue | 1.70 ms | 4.66 ms | 8.05 ms |
| bytes accepted → a packet carries them (client) | 5 µs | 26 µs | 210 µs |
| waiting for the connection lock (client) | 0 µs | 2 µs | 246 µs |
| a stream's reader park (server) | 21 µs | 752 µs | 2 639 µs |
| a pairing woken → polled | **14 µs** | 52 µs | 254 µs |
| the command write on the channel | 2 µs | 7 µs | 15 µs |
| the visitor's first byte after pairing | 14 µs | 120 µs | 451 µs |
| **the pairing's whole wait** | **0.97 ms** | **18 987 µs** | 42 615 µs |

**Nothing inside the path costs the twenty milliseconds, and the pairing is not
descheduled**: it parks about 37 000 times a second — ten wakes per arrival —
and is polled 14 µs after each one. So a pairing wakes, finds no channel, parks
again, and repeats until one arrives. The channel itself arrives late.

Excluded so far, each by its own measurement: accept, the control trip, the
client's open, the send-buffer dwell, the connection lock, the pairing's own
scheduling, the reader's park, the per-tunnel prologue read, the command write,
the post-pairing forwarding, the worker count (4 vs 8 is inside the floor), and
the exchange of channels between visitors.

**The one interval no instrument covers is on the client: from the control
command being read to the task that opens the channel being spawned.** The
client's `ready_us` starts when that task is first polled, so a command that
waited in the control loop's own queue is invisible — and the loop is a single
task that reads, decodes, places and spawns, at four thousand commands a second.

### The mechanism: a lottery over a shared queue (2026-11-03)

The last interval is now measured too. On the client, from the control command
leaving the loop to the tunnel prologue being written — the whole client-side
path, including the open — is **2.1 ms at the median, 5.3 at p99 and 7.3 at
worst** across a second. Add the control trip (132 µs), the server's accept
(16 µs), its routing, the reader park (0.75 ms at p99) and the command write
(2 µs), and the *entire* path from a visitor's accept to a channel sitting in the
queue is **under eight milliseconds even at its worst**.

The pairing it feeds reports 23 ms at p99. **The wait is larger than everything
in the path it is waiting for**, which leaves one explanation, and the counters
say so directly: a pairing parks ~37 000 times a second against ~3 600 arrivals —
**ten wakes per arrival**. `ChannelQueue::push` calls `notify_waiters`, so every
channel wakes every parked pairing and exactly one of them wins. A visitor's wait
is therefore the number of times it loses a lottery with nine other waiters: the
median waits ~5 draws (1.7 ms) and the unlucky hundredth waits ~100 (23 ms), at
3 000 arrivals a second. That is the measured shape, and the arithmetic closes.

**So the identity work was right about the defect and wrong about the path.** A
visitor's channel on an `mhd` lane is a *direct* stream, and it reaches the queue
through `push` — counted as `pooled=0 direct=5 770` in one round. The group id
I put in the tunnel prologue only rides the multiplexed path, which an mhd lane
never takes, which is why its A/B measured nothing.

### It works: the lottery was the tail (2026-11-03, `74b4258`)

Interleaved A/B, four rounds, `--aa`, `--ab-arm l4 --binary-b a495d1d`:

| arm | p50 | p99 | rate |
|---|---|---|---|
| `l4` (identity delivery) | 3.71 [3.56..3.83] | **10.27 [10.06..11.30]** | 3 545 /s |
| `l4~aa` (the same build again) | 4.71 [3.74..8.14] | 10.78 [9.40..14.32] | 3 241 /s |
| `l4~b` (before) | 2.93 [2.56..3.33] | **21.95 [20.22..23.96]** | 3 207 /s |

**`rtt_p99_ms: −53.20 %`, and the model records it as a claim** — beyond the
15 % threshold, beyond the run's floor, with the twin inside the new arm's
range and outside the old one's. The pairing's own p99 fell from 19–23 ms to
**7.6 ms**, which is the first time it has been below the client's own worst
open plus transit.

**The median paid for it, and the reason is worth stating:** p50 rose from 2.93
to 3.71 ms (+27 %). The old median was *borrowed* — a visitor often took a
channel that had been opened for somebody else, so its wait was short and the
other visitor's was long. Removing the borrowing moves the median to the true
per-channel cost and halves the tail. Against TCP (p50 2.24, p99 2.58) the cell
is now 1.7× at the median and 4.0× at p99, where it was 1.3× and 8.5×.

By the objective's own order — latency first, and the SLO is stated on p99 —
this is a net win, and the p50 is now the number to attack. It equals the true
cost of one channel: 2.1 ms on the client (measurement + prologue) plus transit.
That is Stage 3's subject, and the identity work has made the median *mean*
something it did not before.

### What is left of the arrival

| segment | before | now |
|---|---|---|
| the pairing's p99 | 19–23 ms | **7.6 ms** |
| `churn-16` p99 | 21.95 | **10.27** |
| `churn-16` p50 | 2.93 | 3.71 |
| wakes per arrival | ~10 | **1.0** |

The claim is hit for essentially every channel (`claimed=3581 shared=0` in a
second), and removing the blanket notify that still fired on a claimed delivery
took the wakes per arrival from ten to one. **That change bought no latency** —
a paired A/B reads p99 9.74 against 9.65 and p50 3.73 against 3.73 — so it is
kept for the work it removes, not as a latency claim.

**The median is now the target.** It is the true cost of one channel: 2.1 ms of
client-side work (the per-stream Noise handshake and the prologue) plus transit,
and it is what Stage 3 attacks. The tail that remains — p99 9.7 against a p50 of
3.7 — is the same shape as before on a quarter of the base, and the next
instrument for it is the one already built: `cmd_us` and `open_us` per window,
now read against a pairing that no longer waits for a lottery.

### Stage 3 — no per-visitor Noise handshake (the median, quantified)

**The target is now a single number.** On every open, not only the slow ones:

| | p50 | p99 | max |
|---|---|---|---|
| the stream's Noise KK handshake | **1 771 µs** | 5 781 µs | 8 348 µs |
| the whole open (placement + handshake + prologue) | 1 779 µs | 5 748 µs | 8 369 µs |
| the command's whole path (dequeue → prologue) | 1 806 µs | 5 784 µs | 8 377 µs |

**The handshake is 99.5 % of what a channel costs.** Removing it takes the open to
single-digit microseconds and the pairing's median from 2.6 ms to roughly 0.8 —
which is where TCP's 2.24 ms p50 actually is.

The design, and its one honest trade:

1. **One handshake per lane.** A lane's first stream runs the KK it runs today;
   both ends keep the handshake hash.
2. **Derived keys per stream.** `HKDF-SHA256(lane hash, info = direction ‖ stream
   id)` gives each direction of each stream its own 32-byte key and its own
   counter; the packet number is the AEAD nonce and the header is the associated
   data, so **a packet is the record** — there is no separate framing to park
   in, which is the failure mode that killed the three record-layer attempts.
3. **The selector stays in the clear.** A new selector byte says "derived"; the
   rest of the first packet and every later packet of that stream are sealed.

The trade, stated so a reviewer can refuse it: **forward secrecy moves from
per-stream to per-lane.** Today every visitor's channel has its own ephemeral
X25519 exchange; with derivation, a lane's compromise exposes its streams'
keys. Mutual authentication is unchanged — the lane's KK is the same KK — and a
lane is per **session and carrier connection**, so the window is one pool
connection's lifetime rather than one channel's. Two further constraints keep it
honest: stream ids never reuse within a lane, and a lane re-establishment starts
a new hash.

- **Gate**: the open's p50 falls to ≤ 100 µs and the pairing's p50 to ≤ 1 ms;
  `churn-16` p99 stays at or below its current 9.7 ms; `bulk-n` inside 15 %;
  CPU per carried Gbit is measured but does not gate (priority four).
- **Revert**: any bulk regression, or any failure in the crypto tests below.
- **Tests before the wire**: (key, nonce) never reused across stream or
  direction; a packet replayed, reordered, truncated or from another stream is
  refused; a lane re-established with the same four-tuple does not decrypt the
  old lane's traffic; plain (`PLAIN_SELECTOR`) and per-stream KK paths both still
  work; the striped, half-close, reset and hot-reload suites stay green.

### The derivation's root is proved, and the rule that must not be broken (2026-11-03)

Two tests settle the premise:

- `both_ends_agree_on_the_handshake_hash` — the initiator and the responder of one
  session end on the **same** hash, and two sessions over the same keys do
  **not** share one. The second half matters as much as the first: a root that
  repeats across sessions would make one lane's keys derivable from another's.
- `both_ends_agree_with_a_payload_each_way` — the same for the carrying shape
  production uses, where each side's hello rides a handshake message. The
  payloads are part of the transcript, so an implementation that mixed them
  differently on the two ends would still round-trip its own bytes and still
  disagree on the root.

The process-level stream ids that did not line up (client 3, server 1) were an
instrumentation artefact, not a protocol one: both `Conn`s allocate stream ids by
**side parity** (`next_stream` starts at the dialer's bit and steps by two), so a
client-opened stream is odd on both ends and the first hashed stream on each lane
is 1. The client's `3` is the first stream whose wrap *succeeded* — the earlier
ones are the lane's reservation probe and a stream that ended first.

**A design rule that fell out of writing this down, and that would otherwise have
shipped as corruption.** "Sealed" must not be inferred from *lane state* ("is
this lane's root set?"). Streams are opened concurrently: a second stream can be
created while the lane's first handshake is still in flight, so one end would
decide "sealed" and the other "not", and each would read the other's bytes as the
wrong thing. The decision has to travel **with the stream**. The cheapest place
is the frame header the carrier already writes — a sealed-stream frame kind or
flag — because the header is also the AEAD's associated data, so the same bytes
that say "sealed" are the bytes the tag covers.

### The key schedule exists and is tested, and is not wired (2026-11-03, `6cdc25a`)

`src/transport/mhd/cipher.rs` derives one direction of one stream's key from the
lane's handshake root: `HKDF-SHA256(root, "molehill mhd stream v1" ‖ direction ‖
stream id)`, nonce = packet number, associated data = the packet header. Six
tests pin the properties the design depends on — per-(stream, direction) keys,
roots that never collide across lanes, an authenticated header, tampering and
truncated tags refused.

It is `#[cfg(test)]` until it is used, because a key schedule with no caller is a
cipher nothing uses. Three things wire it, and they are the next round in this
order:

1. **A frame flag that says "sealed".** In the header, because the header is
   already the associated data: the bytes that say which stream a packet belongs
   to are the bytes the tag covers, and the decision travels with the stream
   rather than being inferred from lane state (which would race — see above).
2. **The lane's root.** Both ends already record it; the client keeps the first
   successful handshake's, the server the same. A stream opened after it is sealed.
3. **`Conn` seals and opens per packet.** `transmit_stream` seals the payload span
   it just laid out; `on_datagram` opens it; the payload's bound shrinks by the
   sixteen-byte tag, which is the one arithmetic that must be right everywhere.

**Gate** (from the design above): the open's p50 ≤ 100 µs, the pairing's p50 ≤ 1 ms,
`churn-16` p99 ≤ 9.7 ms, `bulk-n` inside 15 %.

### The sealed-stream wiring: two bugs found, a third left (2026-11-03)

The 794-line patch is kept outside the tree (`~/tmp/stage3-wiring-v2.patch`). It
does not yet pass the integration suite, and getting this far took three
measurements that are worth more than the patch.

**Bug 1 — the sealing rule was per lane, and must be per stream.** The first
attempt refused any stream without a key once its lane had a root. But a lane's
*first* stream is opened before the handshake that produces the root — it is the
control channel, and it stays plain for its whole life. Refusing its traffic the
moment the lane acquired a root killed the lane that had just been established,
which is why the client's control session looped on "connection refused" and the
suite fell from 22 of 22 to 16. The counter said it in one run: `no_key=12`,
`sealed_sent=0`. `StreamState` now carries `sealed`, set at creation.

**Bug 2 — the frame's length must describe the payload on the wire.** The sender
wrote the *plaintext* length while the receiver carved the last sixteen bytes out
of the frame's length as the tag, so every sealed packet was read sixteen bytes
short and its tag taken from the body. One counter caught it exactly:
`sealed_sent` and `unsealed` were **equal**, packet for packet. With the length
fixed, `unsealed` is zero.

**What is left.** Packets now open on both ends, and the visitor still never gets
a channel: the server's log repeats `pairing budget expired with no data channel
… gathered=0`, and the client never logs `client asked for a stripe` — so the
request is not reaching it. That is the next measurement, and it is one grep away
from a cause: either the server's control task is not draining
`data_ch_req_rx`, or the client's control loop is not reading, and both are
visible with the existing `debug` lines on the request and on
`ControlChannel::run`'s send.

**Do not start from the design again.** Apply the patch, set
`RUST_LOG=molehill=debug MOLEHILL_MHD_STATS=1`, and read those two lines. The
three counters (`sealed_sent`, `unsealed`, `no_key`) are in the per-second stats
line and made both bugs above findable in one run each.

### The sealed wiring: four bugs down, and the crypto is now clean (2026-11-03)

Patch v5 is outside the tree (`~/tmp/stage3-wiring-v5.patch`, 848 lines). The
fourth bug was the one that made a busy lane fail while an idle one worked, and
it is worth stating plainly because every counter pointed away from it.

**Bug 4 — the associated data was the whole run buffer.** `transmit_stream` seals
each packet into a shared run buffer of up to `SEND_RUN` (44) packets, and the
header it signed was `out[..payload_at]` — every packet *before it in the run*
included. The receiver signs one datagram's header. So the first packet of each
run opened and the other forty-three did not: `sealed_sent=704, unsealed=688` is
exactly `1 in 44`. The fix is one line — remember where the packet starts and
sign `out[packet_start..payload_at]`.

**With that, the crypto is correct under load.** A striped 8 MiB echo runs at
`sealed_sent ≈ 1100` packets a second with `unsealed_tags = 0` and `no_key = 0`.
Four bugs were found in four rounds, each by one counter:

| counter | bug |
|---|---|
| `no_key=12` | the sealing rule was per lane; a lane's first stream predates its root |
| `unsealed == sealed_sent` | the frame's length described the plaintext, not the wire |
| nothing at all | a sealed stream was created but never announced to the accept loop |
| `1 in 44` | the AD was the run buffer, not the packet |

**What still fails is not the cipher.** The same test closes the visitor after a
*variable* number of bytes — 0.4 MB, 2.2 MB, 4.5 MB across runs — and the close is
a clean EOF (`n == 0`), not a reset: no `mhd stream ended: …` error, no rejection
counted, no key failure. Two candidates, in order:

1. **A lost finish.** `ends_at(offset, len)` compares a *plaintext* offset with
   `fin_at`, and the payload clamp changed how much one packet carries; if the
   last chunk never satisfies it and `pending_fins` does not cover the gap, the
   peer sees a stream that stops without ending — and the forwarding loop closes
   on its own timeout.
2. **Flow control under sealing**: the receiver's credit is plaintext, the wire is
   sixteen bytes per packet larger, and a window that is exactly sized could stall
   the sender at the point where the two diverge.

Both are visible in one run with the existing `mhd stalled` line and a counter for
`pending_fins` drained against `ends_at` firings.

### Sealing delivers the arrival — and costs the bulk (2026-11-03)

The patch's end-to-end numbers, same host, same hour, one round each:

| cell | with sealing | baseline | |
|---|---|---|---|
| `churn-16` p50 | **2.30 ms** | 3.71 | **−38 %** |
| `churn-16` p99 | **3.59 ms** | 9.74 | **−63 %** |
| `churn-16` rate | **6 463 /s** | 3 545 | **+82 %** |
| the handshake per open | **14 µs** (p99 45) | 1 771 µs | **−126×** |
| `bulk-1` throughput | 2.11 Gbit/s | 5.60 | **−62 %** |
| `bulk-1` CPU per Gbit | 1.27 s | 0.67 | **+90 %** |
| `bulk-1` cores busy | 1.95 | 3.36 | fewer, at lower throughput |

**Against TCP the cell reads p50 2.30 vs 1.97 and p99 3.59 vs 2.16** — 1.17× and
1.66×, where the carrier stood at 1.7× and 4.0× before this work. Rate 6 463
against TCP's 6 728.

So the top-priority axis moves exactly as designed: the 1.8 ms handshake is gone
(14 µs), the arrival median falls by 38 %, the tail by 63 %, and the cell carries
82 % more arrivals. **That is the largest latency win of the whole effort and the
first one that is not a trade against itself.**

**And the bulk pays for it.** Two thirds of `bulk-1`'s throughput, at 1.9× the CPU
per bit, with *fewer* cores busy — the signature of a stall on top of a per-byte
cost, not of crypto alone. Both are named below.

### What is left before this can land (2026-11-03)

The patch is v9 (`~/tmp/stage3-wiring-v9.patch`, 937 lines) and it now carries two
tests that refute the arithmetic hypotheses this file proposed:

- `a_sealed_stream_sends_the_tail_of_an_oversized_payload` — a 1 400-byte record
  on a sealed stream **does** go out as two packets, tail included.
- `a_sealed_stream_carries_eight_mebibytes` — 8 MiB crosses one sealed stream,
  byte for byte, in process, with no stripes and no sockets. **It passes.**

So neither the "tail never emitted" theory nor "the volume itself stalls" is true.
Whatever kills the striped echo needs the real topology: the striped 8 MiB echo
still ends early, and its own trace says **the visitor→backend direction
finished after 257 frames** while the reader side closed at a frame boundary.

That sentence is the next instrument's target: 257 frames is far short of the
transfer, so the client's *send* direction stopped reading the visitor, and the
question is why — a stalled write into the sealed stream would do it, and the
`mhd stalled` line (credit-blocked, `unsent_ms`) is the one that would say so. It
has not been seen yet in any run of this patch, which itself is information: if
the writer is not blocked, the client stopped reading for another reason and the
forwarding loop's own end conditions are where to look.

**Do not re-derive the crypto.** It is correct under load (`unsealed_tags = 0` at
~200 000 sealed packets a second) and it delivers the arrival (p50 −38 %, p99
−63 %, rate +82 %, the handshake 1 771 µs → 14). What stands between this and a
landable change is one striped transfer, and the two failures below.

### Two switches isolate the striped defect to "sealing **and** stripes" (2026-11-03)

Two one-line experiments on the patched tree, each a single test run:

| experiment | result |
|---|---|
| **growth frozen** (`STRIPE_BACKPRESSURE` = 1 h, so the plan never grows) | still fails, at 465 920 of 8 388 608 |
| **sealing off** (`set_lane_root` a no-op, every other change in place) | **passes** |

So the growth path — the only part of the striped design that adds and removes
members mid-transfer, and the previous round's prime suspect — is **not** the
cause. Sealing is, and it needs the stripe layer to show: an in-process 8 MiB
transfer over **one** sealed stream passes, and `bulk-1` over real sockets (one
stream per visitor) runs at 2.1 Gbit/s without closing. The defect appears only
when a **group of several sealed streams** carries one transfer.

That is as narrow as the problem has ever been stated, and it names the next
instrument: a stripe group of **two** sealed streams in process — the layer's own
`spawn_group` test, with `Conn`s behind the duplexes instead of bare streams. If
it reproduces there, the cause is inside the layer's frame distribution across
sealed streams and can be bisected without sockets; if it does not, the sockets
are part of it and the next step is the run builder's segmentation of
differently-sized packets.

**Where the whole stage stands**, since five rounds have gone into it:

- the cipher is correct under load, with a proof in tests and `unsealed_tags = 0`
  at ~200 000 sealed packets a second;
- it delivers the arrival: `churn-16` p50 2.30 ms (from 3.71), p99 3.59 (from
  9.74), rate 6 463/s (from 3 545), handshake 14 µs (from 1 771);
- it costs two thirds of `bulk-1`, of which **one copy per received packet is
  removable** and the rest is the seal itself;
- and one striped transfer still dies, isolated this round to "sealing **and**
  stripes together".

### The carrier is clean with several sealed streams; the stripe layer is not (2026-11-03)

A new in-process test, `four_sealed_streams_carry_their_own_bytes`: four sealed
streams on one lane, written interleaved under the peer's credit and read at the
other end, each with 256 KiB of its own bytes. **It passes.** So the carrier has
no defect with several sealed streams — the interleaving, the per-stream keys and
the run builder are all fine together.

That leaves the stripe layer, and one experiment would say whether the *number* of
stripes matters: run `striped_data_channels` with `set_initial_stripes(2)`. It is
**not a valid experiment as the test stands** — the test asserts that all four
pool tunnels carried a stream (`no pool carried a stream on each of its four
tunnels`), so halving the stripes changes the fixture rather than the load. A
2-stripe run needs a striped fixture that expects two, which is a small piece of
work for the next round rather than a one-line change.

**Two ways forward, both cheap and both named here so they are not re-derived:**

1. **A stripe-group harness with sealed ends.** `spawn_group` takes `AsyncWrite`
   halves; a sealed end is a `StreamIo` over a `Shared`, which needs a pump task
   to move bytes. Building that harness is the *only* way to bisect the layer's
   frame distribution without sockets — and sockets are the last variable left
   that has not been excluded.
2. **A second striped fixture** that expects two stripes, so the count can be
   varied against a test written for it.

Until one of them exists, the state of this work is: the cipher is proved, the
arrival is delivered, `bulk-1` pays two thirds, and one striped transfer dies for
a reason that lives in the layer between the carrier and the forwarding loop.

### Stage 3 is PARKED: the design works, one striped transfer does not (2026-11-03)

The rule set two rounds ago was two more, then park. The harness did not close it:
the two-group version — a real `spawn_group` on each end with an echo between them
— stalls at **zero bytes**, which is a defect in the harness rather than a
reproduction, and this round could not finish it.

**What stage 3 proved, and it is not nothing:**

| | with sealing | baseline |
|---|---|---|
| `churn-16` p50 / p99 | **2.30 / 3.59 ms** | 3.71 / 9.74 |
| `churn-16` rate | **6 463 /s** | 3 545 |
| the per-open handshake | **14 µs** | 1 771 µs |
| against TCP | 1.17× / 1.66× | 1.7× / 4.0× |

with the cipher correct under load (`unsealed_tags = 0` at ~200 000 sealed
packets a second), four tests pinning its properties, and four real bugs found and
fixed on the way. The cost is two thirds of `bulk-1`, of which one copy per
received packet is removable.

**What it never got:** a striped 8 MiB echo through a sealed group. Nine
configurations were excluded by measurement — growth, stripe count, GSO, many
streams, single stream, four streams, in-process volume, the prologue length, the
accept announcement — and the boundary is exactly "sealing plus two or more
stripes". The cause is in the stripe layer's interaction, and the harness that
would have found it is unfinished.

**So the patch is parked, not deleted.** `~/tmp/stage3-sealing-parked.patch`
(1 171 lines) carries every fix, all six tests, and the half-built harness. A
later cycle that wants the arrival can pick it up from a known state instead of
rediscovering four bugs.

### Where the next cycle should work instead

Stage 3 was the only route tried for the arrival tail, and it is now bounded off.
Two untouched stages are independent of the handshake and carry the same priority:

1. **Stage 4 — RACK and PTO.** One `MIN_RTO` decides both "this packet is lost"
   and "this flight is too old"; the record already shows that every attempt to
   move that one number breaks a different cell. Splitting it is the change the
   measurements have asked for since the first `loss5` run, and it is local to
   `recovery.rs`.
2. **Stage 5 — latency-aware scheduling.** The `rr-16` and `bulk-n` cells already
   show mhd at parity with TCP when work is interleaved across streams; the stall
   cells are where it is not. A per-stream quantum that keeps an arrival from
   queueing behind a bulk run is the shape, and `dwell`/`rx_total` are the
   instruments that would show it working.

Both are less glamorous than removing a handshake and both are measurable with the
cells that already exist.

### The release gate is the handshake under bulk, 29× slower (2026-11-03)

One 0.2-scale timeline with the stats switch, the committed windows compared
between the **first** 30 seconds and the **last** 30 of the same run:

| | first stage | last stage |
|---|---|---|
| `hs_p50` per window (the Noise KK) | **28 876 µs** | 1 002 µs |
| the channel's `open_p50` | **12 149 µs** (p99 79 291) | ~1 000 µs |
| `ready − open` (the prologue) | 2–7 µs | 2–7 µs |
| `cmd − ready` (the dequeue) | 8–10 µs | 8–10 µs |
| the send-buffer dwell | 9 µs | 0 µs |
| the connection lock | 0 µs | 0 µs |
| the server's reader park, p99 | 936 µs (max 5 497) | 210 µs |

**The whole first-stage penalty is inside `open_stream`, and the handshake is all
of it.** The prologue and the dequeue are single-digit microseconds in both
stages, the send buffer and the lock are microseconds in both, and the server's
park is under a millisecond at p99 — so the twelve milliseconds are not the wire,
not a queue in either send buffer, and not a lock. What is left is the handshake's
own polls competing with four bulk streams on four workers, which is why the same
code costs 1 ms on an idle lane and 29 on a busy one.

**That reframes the gate.** It is not the arrival path being slow; it is the
arrival path being *scheduled* behind bulk. Three consequences:

1. The parked sealing patch would have removed this exactly — it took the same
   measurement from 1 771 µs to 14 — which is why it is worth a future cycle.
2. Without it, the cheapest lever is **where an opening stream is placed**: an
   arrival landing on the lane carrying the most bulk pays the most. The pool picks
   a lane by load already; whether that ordering accounts for a *newcomer* is a
   measurement, not an assumption.
3. And the second lever is the client's own runtime: four workers saturated by
   bulk do not schedule a newcomer's polls promptly, and no wire change fixes that.
   `TOKIO_WORKER_THREADS` was measured once (4 vs 8, inside the floor) on a cell
   with no bulk; it has never been measured on the *first stage's* arrival.

### The release gate fails the same four stages every time (2026-11-03)

Two full-scale `timeline` runs, back to back, both fail **exactly** `rate100`,
`rate20`, `jitter` and `clean#2` — every stage after `loss5` — with the same
reason (`iperf3 error: control socket has closed unexpectedly`). This is not the
intermittent harness failure the record describes as costing "0–3 stages, arm
independently"; it is deterministic and positional.

What each failure looks like, from the artefacts rather than the summary line:

- the **sink** accepted the connections (`[ 5] local 127.0.0.1 port 2402 connected
  to …`) and then logged **0.00 Bytes for 117 s** before iperf3's idle timeout;
- the **pool** reports `established=4` once and never repairs or reaps, so no lane
  is missing;
- the **control session** stays up; `mhd stalled` never fires; no rejection, no
  key failure, and the daemon's log has no `ERROR` or `WARN` at all.

So the tunnel forwarded the *connection setup* and none of the *payload*: the four
streams exist and carry nothing.

**And `rate100` on its own is fine.** A single-stage campaign under the same
condition and shaping puts the **control** arm (no tool in the path) at
0.0956 Gbit/s and molehill at **0.0954** — within 0.3 % of a path that has no tool
in it. So the condition is not the defect; **the stage after a lossy one is**.

That is the shape the plan's Stage 5 exists for: the window grows and is never
cut, `slow_start` latches off at the first loss, and nothing re-arms it. A sender
whose window collapsed under `loss5` has no mechanism to grow back — and the next
stage is where that shows, as a connection that opens and then carries nothing.

**The instrument for the next round is one word away and it is already in the
tree**: the window line is a `debug!` in `transport::mhd::pump`, and this run
logged with `RUST_LOG=molehill::transport::mhd=debug`, which did **not** reach it
(`mhd window` count: zero). Use `RUST_LOG=molehill=debug` — the form the earlier
per-arrival run used successfully — and read `cwnd`, `flight` and `slow_start`
across the `loss5 → rate100` boundary. If `cwnd` is pinned at a few kilobytes and
`slow_start` is false, Stage 5's first half (re-arm growth after an idle or
loss-free period) is the fix and it is local to `recovery.rs`/`conn.rs`.

### Retraction: the drain expired on the churn probe's sockets, not on a backlog (2026-11-03)

The previous entry read `drain_expired` as "the carrier left a backlog the drain
could not clear". **That was wrong**, and the code says so plainly: `drain`'s
predicate is a small queue **and no send-capable socket**, and the timeline runs
the churn probe for the whole schedule. There are therefore always `ESTAB` sockets
on a service port while churn runs, so `drain_expired` is *structural* in the
timeline and says nothing about the arm. `drain_final_backlog` also reads ~500
packets on stages that passed, so that number is the ordinary state of a busy leg,
not a tail.

What survives from that round, and matters: the **retry fired on all four missing
cells and all four failed again**, so the loss is deterministic within a run. And
the sink of `clean#2` shows it directly — on a **clean** leg, four streams
established and **zero bytes for the whole stage**, in a run whose first `clean`
stage carried 4 Gbit/s.

### `malformed` was real, and it is the UDP probe crossing the lane (2026-11-03)

The refusal counter, which this file has twice said reads zero on a healthy run,
reads **2–6 per second** through a whole timeline. It was one number for two
different checks, so it was split this round — and the split answers it:

| run | `malformed_decode` | `malformed_lane` |
|---|---|---|
| `churn-16` (no UDP probe) | **0** | 0 |
| `timeline` (UDP probe running) | 2–6 /s | rare, 1 in one window |

A datagram on a lane's own socket that cannot be framed, only while the UDP probe
runs, is the UDP probe's traffic arriving where the lane's decoder reads it. The
counter is telling the truth about a shared port, not about a broken lane.

**So the counter needs a third home**: traffic that is not ours must not be counted
as a refusal of ours, or every future run reads a defect that is not there. Until
it has one, a run's `malformed_decode` is evidence only when the UDP probe is off.

### The dead stages were never read: `snd_wnd = 0` from the first second (2026-11-03)

The per-sample raw artefacts the model keeps answer the question the summaries could
not. From one run, the iperf3 client's own first and last intervals:

| stage | outcome | first interval | last interval |
|---|---|---|---|
| `clean` | passed | `bytes=180 092 928  snd_wnd=98 304` | `bytes=106 561 536` |
| `clean#2` | failed | `bytes=917 504  snd_wnd=0` | `bytes=0  snd_wnd=0` |
| `jitter` | failed | `bytes=786 432  snd_wnd=0` | `bytes=0  snd_wnd=0` |

**`snd_wnd = 0` in the *first* second of a connection that then carries nothing.**
The peer's receive window was closed from the outset, which means the tool never
read that visitor socket at all — not "the credit ran out later under load". About
900 KB arrives (the kernel's socket buffer) and then the connection is stuck for the
whole stage, until iperf3's sink gives up after 117 s and the client exits with
`control socket has closed unexpectedly`.

This also explains why every other instrument was silent: `dwell` measures a
sender's unsent bytes, `mhd stalled` measures a flight, the pool counts lanes, and
none of them looks at *a receiver that was never started*. The chain that should
end in "somebody reads this visitor's socket" ends in a channel nobody serves.

**The prime suspect is the one recent change on that path**: the identity delivery
that hands a channel to the pairing that named its group. A channel delivered into a
group that can no longer take it — already complete, or already finished — waits in
a queue that nobody drains, and its visitor's window stays shut for ever. That would
be a regression from this cycle's own work, and it is cheap to test: count a channel
handed to a group with no live stripe waiting for it.

**The instrument for the next round**, in order:

1. A counter pair beside `claimed`/`shared`: **delivered to a group that was gone**
   (`dead_group`), and **claimed by a stripe that never started reading**
   (`orphaned`). Either rising during a failing stage names the defect outright.
2. If both are zero, the missing read is downstream of delivery, and the next place
   to look is the client's `register` → `run` handoff for a stripe whose group
   completed while it was still being registered.

### Refuted: it is not the identity delivery (2026-11-03, round 29)

`deliver` did have a hole — it returned `Ok(())` and dropped a channel when the
pairing that named the group had gone, while its own comment said the channel falls
through to the shared queue. That is fixed, and a counter beside `claimed`/`shared`
records it.

**But on a full-scale run that failed the same four stages, `dead_group` stayed at
zero from the first window to the last.** So the hole was real and is not the cause.
Two other numbers from the same windows:

```
delivered pooled=0 direct=5..9 claimed=5..9 shared=0 dead_group=0
```

Channels were **delivered and claimed normally through the failing stages**. The
defect is therefore *after* delivery: the channel exists, the pairing has it, and
the data it carries is never read.

**That narrows `snd_wnd = 0` to its proper owner.** The visitor's window is closed
because the **server** never reads the visitor's socket, and the server reads when
`wake_readers` fires for that stream. Delivery is counted and healthy; the read is
not happening. This cycle has been bitten by exactly this class twice before (the
`push` path and the claim path each missed a wake), which is why it is the first
suspect rather than a new theory.

**The instrument for the next round**, chosen to distinguish "no wake" from "no
data":

- count, per second, the times a stream's `poll_read` returned `Pending` **while
  its receive buffer held unread bytes** — a reader starved with data waiting;
- and count `wake_readers` against `RecvBuf::note_ready` per lane.

A starved-read count above zero during a dead stage is the missing wake, named. If
it stays zero, the data never arrived at the buffer, the sender never sent, and the
next place to look is the client's stripe sender for a group whose stripes were
never started.

### Not starvation either: there is no stream to starve (2026-11-03, round 30)

The pump now samples, once a second, how many streams have had unread bytes for
longer than a second — the question a reader cannot answer about itself, because a
reader that is never polled never runs a line of code. On a full-scale run it never
fired once, while two stages still carried nothing.

So the dead stage is **not** a stream whose data waits unread. With `snd_wnd = 0`
on the visitor's socket and no stream starving, the only shape left is:

> the visitor's connection is accepted and its bytes shut the sender's window, and
> **no data channel is ever attached to it** — so there is no stream to read, nothing
> to starve, and every stream-level instrument is silent by construction.

That is one layer above everything measured so far. Three hypotheses have now been
killed by measurement, in order:

| hypothesis | killed by |
|---|---|
| the drain left a tail | `drain_expired` is structural: the churn probe always holds sockets |
| a channel is dropped for a dead group | `dead_group = 0` through a failing run |
| data arrives and is not read | `starved = 0` through a failing run |

**The instrument for the next round is a pairing, not a stream:**

1. Count pairings that **wait for a channel and never receive one** before their
   visitor gives up — the visitor-side counterpart of `parks`.
2. Count channels delivered to a group whose **visitor has already finished** — the
   mirror image of `dead_group`, which counted the pairing being gone rather than
   the visitor.
3. And record, per pairing, the group id it named and the group id of the channel it
   was given, so a mismatched pairing is visible as an id and not as a suspicion.

A run in which (1) rises is the defect stated in one number.

### The stall line was never silent: it names the wedge (2026-11-03, round 31)

Earlier rounds searched for `mhd stalled` in runs logged at `INFO` and concluded it
never fired. It is a `debug!` line, and a run with `RUST_LOG=molehill=debug` has
**2 282 of them** on the server. Read properly, they are the clearest evidence this
investigation has produced.

Server side, over one failing run:

```
unsent_ms=200            median (max 3 211)
unacked_ms=196           median
credit_blocked=true      1 492 of 2 282 lines
worst_stream=5  worst_stream_unsent=14 994 767
worst_stream_next=73 582 695   worst_stream_written=88 523 502
worst_stream_retransmits=0
```

So the wedge is a **live stream that has carried 88.5 MB, has sent 73.6 MB, and is
holding 15 MB it cannot send because the peer's window is shut** — and the peer
reports the opposite of what that implies:

- the client's own stall lines are **never** credit-blocked, its unsent is **0**, and
  the starvation sampler never fired, so by every receiver-side instrument its read
  buffers are empty and its reader is running.

Fifteen megabytes is the receive window (`WINDOW` is 16 MiB). A window that is full
on the sender's side and empty on the receiver's is not a reader that stopped: it is
**credit that was consumed and never given back**. That is a bookkeeping defect in
`advance_read`/`advertised`, not a scheduling one, and it explains every symptom at
once — the shut `snd_wnd` on the visitor's socket (the forwarding loop is parked on a
stream that will not accept bytes), the empty receiver buffers, and the silence of
every stream-level instrument, because those bytes never arrived to be starved over.

**The instrument for the next round** is on the receiver, once a second, for the
worst stream: `recv_credit`, `advertised`, the stream's unread bytes, and whether a
credit update was due. Three numbers in one line separate "full because unread" from
"full because the accounting lost it" — and the second is a bug that has been in this
carrier for as long as the window has.

### The credit wedge is fixed, and the release gate drops to two failures (2026-11-03)

`advance_read` moved the window and never armed an acknowledgement. `credit_due`
woke the pump, the pump had nothing to send, and a packet that is not an
acknowledgement carries no credit — so a reader that drained a window on a quiet or
one-way stream left the sender with `send_credit - written == 0` and no way to hear
otherwise. One line in `advance_read` now arms that acknowledgement.

Measured on the next full sweep, against the run before it:

| | before | after |
|---|---|---|
| stages with no measurement | **5** (`loss5`, `rate100`, `rate20`, `jitter`, `clean#2`) | **1** (`clean#2`) |
| `credit_blocked` lines (server) | **1 492** of 2 282 stall lines | **0** — no stall line at all |
| `clean` p50 / p99 | 78.3 / 248.5 ms | **3.5 / 132.2** |
| `rate100` p50 / p99 | no measurement | **84.6 / 105.1** |
| `jitter` p50 / p99 | no measurement | **81.7 / 107.9** |

`just bench gate` on that run: **2 failures**, down from 5 — `clean#2` produced no
measurement, and `timeline[clean]` p99 132 ms against the 50 ms limit.

The defect was real — credit moved and was never spoken — and it is fixed. **Its
effect on the gate is not established**, and the next run says so: with the same
build, four stages carried nothing again. It is therefore recorded as a correctness
fix whose gate effect is one run old, not as the cure.

What that second run does establish is that the *shape* of the failure changed. In
it, `mhd stalled`, `credit_blocked` and the starvation sampler are all **zero**, and
two other counters are equal to the visitor: `accepted = paired = 33 995` over the
whole schedule, with a per-window gap never above 2. So pairing is exonerated, no
credit is held, nothing is queued — and four stages still carried nothing.

That leaves one state: a visitor whose pairing returned, whose socket the forwarding
loop never read, and which therefore never put a byte in front of the carrier. Its
sender sees `snd_wnd = 0` from the first second, which is what a socket nobody reads
looks like, and every instrument in the carrier is silent because *the carrier was
never given anything to do*.

**The next instrument is on the boundary between the visitor's socket and the
carrier**, and it is two counters per visitor: that the forwarding loop polled the
visitor's read at all, and that the poll returned bytes. A visitor that pairs and is
then never polled is the remaining state, and it is the one no counter can currently
see.

`clean`'s p99 is separately 2.6× the limit while its p50 is 3.5 ms, so the tail is a
few very slow arrivals rather than a shift. That is its own instrument's work.

### Retraction: the age gate's refusals were counted wrong (2026-11-03, round 35)

The entry that stood here said the age gate refused 4.6 million writes against 0.87
million by the congestion window, and called it the dominant bound. **That number is
wrong and is withdrawn.** The label was read back from the previous call's
`last_refusal`, and `Some(Aged)` short-circuited: once the gate had refused one write,
every later refusal — on any stream, of any kind — was labelled `Aged`. The counter
measured "refusals after the first aged one", not the gate.

The label is now built for the call it describes, and the gate's own input is logged
once a second: how many packets the recovery table holds and how old its oldest
unacknowledged one is. That separates a gate doing its job (a real hole older than the
timer) from a gate held open by a record nothing will retire — and the first samples
of the fixed counter show the gate firing with **1 580 packets in the table and the
oldest at 200 ms**, which is exactly `MIN_RTO`: the gate working as designed, not
stuck.

**What this costs the record.** Two of this cycle's entries leaned on that number: the
"dominant bound" claim and the experiment it justified. The revert of the per-stream
scoping stands on its own measurements and stays; the claim that the gate dominates
does not.

**A lesson worth its own line**: the instrument bug was invisible because the counter
was *plausible*. A number that large was read as a finding — "the hidden bound is the
big one" — instead of as a bug report about the instrument. The repository's own rule,
"re-check every consumer after reshaping data", did not catch it because the reshaping
was in the label, not in the data.

### First green gate, and the refusal mix measured honestly (2026-11-03)

With the refusal label fixed, a 0.3-scale `timeline` — every stage measured, none
failed — gives the proportions the broken counter had inverted:

| bound | writes refused | share |
|---|---|---|
| the peer's credit | **565 039** | 64 % |
| the congestion window | 255 688 | 29 % |
| the age gate | **66 251** | 7.5 % |

**The age gate is the smallest of the three, not the largest.** The dominant bound is
the peer's reader, which is what `credit_blocked` said all along and what the earlier
1 492-of-2 282 ratio suggested before the label went wrong.

And the run itself:

| stage | p50 | p99 |
|---|---|---|
| `clean` | **2.17 ms** | **22.3 ms** |
| `rtt100` | 402 | 403 |
| `loss1` | 42.0 | 1 054 |
| `loss5` | 403 | 1 460 |
| `rate100` | 82.3 | 103 |
| `rate20` | 163 | 164 |
| `jitter` | 81.6 | 109 |
| `clean#2` | **2.18 ms** | **23.0 ms** |

**`bench gate`: PASS** — the first green gate of this effort. Both clean stages are
inside the 50 ms SLO by a factor of two on the median and by more than a factor of two
on the tail, and the coverage is complete.

Two caveats, both stated by the gate itself and neither to be dropped: the timeline ran
500 s, which is **under the 900 s a drift slope needs**, so the drift axis is only
partly gated; and this is scale 0.3. Every full-scale run this cycle has lost between
one and five stages, so the 0.3 → 1.0 transition is the open question, not the SLO.

### What the gate refuses now is the first stage's tail, and nothing else (2026-11-04)

A full-scale run after the refusal fix:

| stage | p50 | p99 | |
|---|---|---|---|
| `clean` (first) | 22.0 ms | **135.3** | SLO fail |
| `rtt100` | 402 | 422 | |
| `loss1` | 42.0 | 1 068 | |
| `loss5` | 402 | 1 463 | |
| `rate100` | 84.8 | 105 | |
| `rate20` | 359 | 397 | |
| `jitter` | — | — | failed, instrument |
| `clean#2` (last) | **1.74 ms** | **2.41 ms** | |

Seven of eight stages measured, and **`clean#2` — which failed in every recent full
run — is now the best cell in the schedule**. The one SLO failure is the *first*
stage, whose own last-stage twin reads 1.74/2.41 in the same code and the same run.

That fixes the target precisely: the first stage's arrivals queue behind the run's own
start-up — pool establishment, the control session, the bulk spine's first flow — and
a queue is a scheduling problem, not a handshake one. The parked sealing patch would
have removed the handshake entirely (1 771 µs → 14) and is still parked; the remaining
route is Stage 4, whose subject is exactly this: an opening must not wait behind bulk.

`jitter`'s single loss is the instrument's (`control socket has closed unexpectedly`),
one stage in eight, after `reap_iperf`. That is the harness's remaining leak, and it is
separate from the carrier.

### The sealed striped failure is the half-close grace, and it is not deterministic (2026-11-04)

The previous entry read a repeat at exactly 465 920 bytes as proof of a deterministic
defect. **It is not deterministic**: a third run of the same tree failed at
**5 184 512** bytes. The "same byte" was a coincidence of two runs, and the claim is
withdrawn.

What the trace does say is better. On the failing run, with `molehill=debug`:

```
stripe send direction finished, frames and bytes seq=257 bytes=8388608
stripe reader ended at a frame boundary  last_seq=Some(253..256)
stripe receive direction finished after 257 frames
stripe half-close grace expired; cutting the other direction      <- seven seconds later
Error: echo service closed after 5184512/8388608 bytes
```

Read carefully, that trace says less than it seems and something important:

- **One** direction logged `seq=257 bytes=8388608`; the log does not say which, and the
  other direction logged only `finished after 257 frames` with its readers ending at
  `last_seq` 253–256. So at least one direction's readers ended **before the last
  frame**, which is a gap the reorder queue cannot fill.
- The grace is **10 s, started when the first direction ended** (17.93 → 28.04 in the
  trace), and it does not drain: it aborts the other direction outright.

So the two candidate mechanisms are now distinguishable and neither is guessed: a frame
that never arrived on its stripe (leaving `last_seq` short), or a direction aborted at
the grace with frames still buffered. What is certain is that a close overtook data,
which the archive already states as binding — "FIN/reset/close must not overtake queued
data".

**The instrument that separates them is one counter per direction**: frames expected
(the highest `seq` seen plus one) against frames delivered in order, logged when the
grace fires. A gap in one stripe and a truncated direction look identical in the
visitor's byte count and completely different in that number — and the fix differs
with it: a drain predicate on the grace for the second, a missing-frame repair for the
first.

**And a correction this cycle has now made twice**, worth stating once: a single repeat
is not a mechanism. Two runs at the same byte looked like determinism; a third run
dissolved it. The instrument that would have shown it is a repeat count, and the rule
is to run the failing test three times before naming a number.

### The sealed striped failure is intermittent, and a direction ends after one frame (2026-11-04)

Three runs of the same tree, in a row, on the nine-second test:

| run | outcome |
|---|---|
| 1 | **passed**, 56 s |
| 2 | failed at **465 920** of 8 388 608 |
| 3 | failed at **465 920** |

So it **can pass** — the design is not broken, the layer has a race — and when it fails
it lands on the same byte. That is the shape of a start-up ordering defect, not of a
missing feature, and it is the first time this work has produced a *pass*.

What the trace adds:

```
stripe receive direction finished after 1 frames
Error: echo service closed after 465920/8388608 bytes
```

A group's receive direction ends after **one frame**, and the visitor is left with
465 920 bytes — which is a kernel socket buffer, not a coincidence: the tool read
almost nothing before the group closed under it. The half-close grace never fired in
these runs (no `grace` line at all), so the loss is not the grace and not a truncation
this time: a direction reached its own clean end far too early.

**465 920 is therefore the size of an untouched receive buffer**, and the number to chase
is not it but *who ended the group after one frame*. The instrument is in place: the
receive direction now tallies frames seen and frames delivered, and the send direction
tallies frames and bytes, so a group that ends after one frame while the visitor still
has megabytes queued is a single line away.

### Two failure modes, told apart by the group id (2026-11-04)

With the group id on every lifecycle line, three runs of the sealed striped test say
what one line could not:

| run | outcome | what the groups did |
|---|---|---|
| 1 | **passed** | group 6 sent 258 frames / 8 388 608 bytes and received 258 |
| 2 | failed at **465 920** | groups 1 and 2 each carried **one frame of 4 bytes** — the pool probes — and no group carried the transfer at all |
| 3 | failed at **4 939 776** | group 3 sent 8 388 608 and group 4 received 257 frames, then group 4 sent 257 back; the response stopped at 4.9 MB |

**So there are two defects, not one.**

- **Mode A — the transfer's group never starts.** The visitor is accepted and its socket
  buffer fills with 465 920 bytes while the only groups in the log are the pool probes.
  Whatever the server paired is not the group that carries the visitor.
- **Mode B — the response truncates part-way.** The request completes (8 388 608 bytes
  across a group), and the response's own send direction stops at 4.9 MB with no gap in
  its receive direction (`frames == seen`), so the *source* ended: the client's read of
  the backend returned EOF early.

Both are start-up ordering defects in the layer, both reproduce in nine seconds, and
they need different instruments: mode A wants a line at the pairing's end carrying the
group it spawned, and mode B wants the client's send direction to say *why* its source
ended — EOF from the backend socket, or an error.

**For the record**: this test passes about one run in three. It is not a broken design
and it is not a fixed byte; it is a race with two outcomes, which is why single runs
have misled this file twice.

### The visitor's group exists, and its send direction never carries a byte (2026-11-04)

Both instruments landed, and one more thing was needed to read them: **the group id
collided between the two ends**, because both processes count groups from one and this
test runs both in one process. The server now tags its groups `(1 << 32) | group`, and
with that the three runs say the same thing:

```
DEBUG stripe: stripe source ended: eof group=1 frames=1              <- client, 4 bytes
DEBUG stripe: stripe receive direction finished group=4294967296 frames=1 seen=1
DEBUG ...: pair_striped_group: Visitor paired with a striped group group=1 stripe_count=4
Error: echo service closed after 138240/8388608 bytes
```

Read as roles rather than ids:

- the **client's** group sent one 4-byte frame and its source — the backend socket —
  reached EOF, so the backend had four bytes to echo and nothing more;
- the **server's** group (`4294967296`) exists, was paired with the visitor, and its
  receive direction saw **one** frame;
- the server's send direction **never logged a finish**, and the visitor's socket held
  138 240 / 990 208 bytes when the test gave up.

So the server read nothing from the visitor: its send loop is parked on the first frame
it tried to write into a stripe. That is a *bounded* statement — the pairing is done,
the group exists, and the first write blocks — and it points at the one thing that can
block it: the peer's group had not started reading yet, so the frame waits on a stream
nobody is draining.

**That is exactly what a lane-level root would fix**, and it is the last mile of the
parked design: with the handshake removed the peer's group starts in microseconds
instead of after a KK, so the window in which the writer blocks closes. The next attempt
should therefore **measure the gap between the server's first write and the client's
`group started`** — one timestamp each — rather than add another theory.

### The sealed stream ends with megabytes unaccounted (2026-11-04)

The previous entry read a partial delivery as a close that resets a connection. **A
clean EOF refutes it** — the visitor's read returned zero, not an error — and the byte
counter added this round replaces the guess with a number from each end:

```
stripe receive direction finished group=3 frames=257 seen=257 written=8388608   <- client to its backend
stripe send direction finished group=2 frames=257                                  <- client sent 257
stripe half-close grace: what the receive direction had left
        highest=167 delivered=167 missing=0                                        <- server, at the cut
Error: echo service closed after 5464064/8388608 bytes
```

Read across, that is:

- the **client** wrote all 8 388 608 bytes of request into its backend, so the request
  crossed the carrier intact;
- the client then sent **257 frames** of response;
- the **server's** receive direction had seen **167** when the grace cut it, and
  `missing=0` says it had delivered every frame it ever saw — the last ninety frames
  were never seen at all;
- 167 frames is 5 472 256 bytes, and the visitor saw 5 464 064. The arithmetic closes.

So the loss is inside the **sealed stream**, not in the stripe layer's ordering: 90
frames' worth of bytes were accepted by the sender's carrier, never arrived at the
receiver's, and the stream then ended **cleanly** — which is why nothing counts a gap,
a rejection or a retransmission. This is the third appearance of one rule in this file:
**a close must not overtake queued data.** Here it is the close of a *sealed stream*
that overtakes its own send buffer.

**The instrument for the next round is in `Conn`, where both ends are already known**:
at stream close, log the stream's `written`, `sent` (highest offset put on the wire) and
the peer-side `arrived` from the last acknowledgement. A stream that closes with
`sent > arrived` is this defect, named, on the exact stream — and it is one line per
stream while the stats switch is on.

### Two causes refuted, and the arithmetic that is left (2026-11-04)

The close line landed and refuted the first candidate on three runs: **every stream
closes with `unsent = 0`** — `written == next` — so nothing is dropped from a send
buffer, and the close does not overtake its own queue.

```
mhd stream closed stream=5 written=2097800 next=2097800 unsent=0 reset=None
    peer_fin=false local_fin=true
```

The second candidate was the FIN's ordering, and it is refuted by reading the code:
`poll_transmit` tries credit, then **stream data**, then reset, then **fin** — so a
finish cannot precede its own data, and `send_run` loops until every span of a run is
accepted (`at += sent`), falling back from GSO to the batched syscall, so a partial send
is not a silent loss either.

That leaves the arithmetic, and it is now the only thing left to explain:

| | bytes |
|---|---|
| request, into the client's backend | **8 388 608** (counted, `written`) |
| response, frames the client sent | **257 × 32 KiB** |
| response, frames the server's reader saw | **167** (`highest = delivered = 167`) |
| response, the visitor received | **5 464 064** |

The request crossed intact; the response lost ninety frames on the way, both ends
reported clean, and the losing end — the sender — reports `written == next`, which means
it believes every byte went out. **So the number that decides it is one nobody has
printed yet: the carrier's `next` on the client's own streams.** If the client's four
streams closed with `written ≈ 2.09 MB` each (8 MiB total), the carrier accepted the
frames and lost them downstream; if they closed at ≈ 1.4 MB each, the stripe layer never
handed the last ninety frames over despite logging 257.

`local_fin=true, peer_fin=false` on every server-side stream also dates the close: the
receiver finished its half before the peer finished sending, which is what let the rest
of the response be dropped as "unknown stream" — the path that documents itself in
`apply_datagram` as *"a retired stream"*.

### The grace was cutting a direction that was still working (2026-11-04)

The previous entry blamed `release_unread` for acknowledging discarded bytes. **That was
wrong** and is withdrawn: `RecvBuf::arrived()` returns the *contiguous* frontier and stops
at a hole, and `discard()` does not move it, so releasing unread bytes cannot over-ack.

What the numbers did say is where the bytes stopped: the sender's streams closed with
`written == next == acked ≈ 2.1 MB` each — every byte legitimately received — and the
stripe layer's receive direction had delivered **167 of 257 frames** when the group's
half-close grace **aborted it**. The grace was a fixed ten seconds from the moment the
*other* direction ended, and it did not care whether the surviving direction was still
working. It cut a receive direction in the middle of a transfer; the frames that never
came were then dropped at close by the carrier, correctly, because the stream's own
frontier had never reached them.

**So the grace now waits for ten seconds of no progress**, using the activity clock the
group already keeps for its idle watchdog — which is the condition it was always meant to
describe. Three runs of the sealed striped test after the fix: **two passed**, one failed
at the familiar 465 920 (the mode where the transfer's group never starts at all). Before
it, one of three passed.

That is a real improvement and not yet a cure, and the remaining failure is a *different*
defect — the one the group-id trace isolated as "no group carries the visitor" — with its
own line still to write.

**One more retraction for the pile, and it is the same lesson twice in three rounds**: a
mechanism that fits the numbers (`release_unread` lying) is not evidence that it is the
mechanism. Reading `arrived()` took two minutes and would have saved the round. Read the
code before naming the cause; measure before writing the entry.

### The sealed striped test is seven in eight, and the failure is the second visitor (2026-11-04)

With the grace fix in place, **eight runs of the sealed striped test**: **seven passed**,
one failed. That is the first quantitative statement of the sealed path's health, and it
replaces three rounds of "one in three" impressions.

The failing run's trace says exactly where it goes:

```
stripe group started with 4 stripes group=1          <- first visitor, client side
stripe receive direction finished group=2 frames=257 seen=257 written=8388608
stripe receive direction finished group=1 frames=257 seen=257 written=8388608
Visitor paired with a striped group group=2           <- SECOND visitor, server side
stripe group started with 4 stripes group=2
stripe group started with 4 stripes group=3
stripe send direction finished group=2 frames=257
Error: echo service closed after 1243136/8388608 bytes
```

**The first visitor's transfer completes in both directions — 257 frames and 8 388 608
bytes each way.** The failure is in the second phase, the back-to-back visitors the test
exists to check: the second visitor's group starts after the first one's streams have
already closed, and its transfer stops at 1.2 MB.

So the remaining defect is **a group whose stripes are recycled after a previous group's
streams closed** — the pool hands the second visitor channels that were just retired, and
their first frames go nowhere. That is a much narrower target than "the striped path is
broken", and it has its own instrument already: the close line now prints `written`,
`next`, `acked` and `peer_fin` for every stream, so a recycled channel shows as a stream
that closed `peer_fin=false` shortly before the next group's first frame.

### The failing run drops data on the lane's first stream (2026-11-04)

Looping the sealed striped test until it failed, then reading the carrier's own log:

```
mhd dropped data for a retired stream stream=1      (five times, in the failing run)
```

`Conn::open_at` allocates ids monotonically by two and **never reuses one**, so `stream=1`
is the *first* stream this end ever opened on that lane — the one the lane's validation
used. The receiver had retired it; the sender was still writing to it; every one of those
datagrams was dropped, and `apply_datagram`'s own comment says why that is invisible: the
packet is still acknowledged, because a sender that keeps re-sending an unacknowledged
packet counts ten unanswered retransmissions and declares the lane dead.

Reading the carrier's own plumbing corrects the guess that stood here: `CarrierStream` is
`Box<dyn CarrierIo>` and a data channel **is** a stream on the lane, so `stream=1` is
simply the first channel the client ever opened — not a validation stream. And the server
retired it because **its visitor finished**, which is the normal end of a channel: the
server's side of a channel serves exactly one visitor, and when that visitor goes the
stream is retired.

**So the defect is channel reuse across visitors.** The client's pool takes a channel back
after its visitor ends and hands it to the *next* visitor's group, while the server has
already retired that stream. Every frame the new visitor's group writes into it is dropped
by the receiver with five lines at `debug`, and the visitor carries nothing.

That is Mode A as an identity: **a channel outlives its server-side stream**, and the
second visitor — the one the back-to-back phase creates — is the one that pays. The fix is
a rule about ownership: a channel must not return to the pool once its stream is closed;
it must be replaced, and the two ends must agree on when that happens.

**The next step is a decision, not an experiment**: either the first stream is a data
channel on both ends (and the server must not retire it), or it is validation-only on
both (and the client must never hand it to the pool). The measurement that picks between
them is already in place — the eight-run rate — and either choice must take it to eight
of eight.

### A retirement reset made it worse, and is reverted (2026-11-04)

`Conn`'s reset queue was **only ever drained**: nothing pushed into it, so retiring a
stream said nothing to the peer, whose next frames were dropped with a debug line. That is
a real hole, and the fix looked obvious: queue a reset on retirement unless the peer had
already finished.

Measured on the instrument this investigation trusts — eight runs of the sealed striped
test — **it made things worse: four of eight, against seven of eight before.** A reset
tells the peer to forget the stream, and the peer here is a stripe group that is *still
transferring*: the reset truncated transfers that would otherwise have finished.

So it is reverted, per the plan's rule, and the numbers stay in this file. The queue being
write-only is still a defect; the lesson is that "retired" is not one condition. A stream
retired **after both ends finished** needs no reset — telling the peer anything is what
breaks it — while one retired **while the peer may still write** needs a reset, and the
two cases are distinguishable: `peer_fin` says which. The attempt that failed checked
`peer_fin` and still cut live transfers, which means the *retirement itself* was happening
too early in these runs, and that is the thing to fix, not the announcement.

**Eight-run rates so far, all on the sealed striped test**: one of three before the grace
fix, seven of eight after it, four of eight with the retirement reset. The instrument is
that test and the number is the pass count; no other claim in this file depends on it.

### The false acknowledgement is load-bearing, and that is the real finding (2026-11-04)

Two halves of one obvious fix, both measured on eight runs of the sealed striped test:

| change | pass rate |
|---|---|
| none (after the grace fix) | **7 / 8** |
| a retirement announces itself with a reset | 4 / 8 |
| returning a dead stream's window no longer counts as delivery | 5 / 8 |

Both are *correctness* fixes and both are reverted. Their combination is what the design
currently runs on: a stream that ends with unread bytes returns its window **and** claims
delivery, so the peer retires the stream, the channel goes back to the pool, and the next
visitor gets one. Take either half away and streams stop completing — the pool stalls, the
transfer truncates, and the pass rate falls.

**So the defect cannot be patched locally, and five rounds of local attempts are the
evidence.** The sealed path's remaining failure is a *lifecycle* problem: a channel is
created per visitor, its server side ends with that visitor, and the client's pool then
hands the same channel to the next one. Every honest way of ending a stream early makes
that reuse fail; the dishonest way works seven times in eight.

**What the design needs, stated as a contract**: a channel that has served a visitor is
finished for both ends, and the pool must replace it rather than recycle it — with the
replacement opened *before* the next visitor needs it. That is the same shape as the
pre-opened-stock idea this file rejected long ago, and the difference that makes it
legitimate here is that the stock is *per lane, one deep, and refilled by the retreat of a
channel* rather than a fixed pool of four.

That is a design change of its own, with its own measurement, not a patch. Until it exists,
the sealed path stays parked at seven in eight, and this file says so rather than
pretending a local fix is one line away.

### The first stage is cold start, and the cold cost is establishment (2026-11-04)

A diagnostic, not a way to pass: the same full-scale schedule run once with
`--warmup-rounds 1`, so the pool, the control session and the first channels exist before
the measured stage begins.

| stage | cold (no warmup) | **warm (one round first)** |
|---|---|---|
| `clean` p50 / p99 | 22.0 / **135.3** ms | **2.01 / 34.7** ms |
| `clean#2` p50 / p99 | 1.74 / 2.41 | (stage lost to the instrument) |

**With the pool already up, the first stage is inside the SLO** — 34.7 ms against a 50 ms
limit — and it reads like the last stage. So the SLO failure is not the carrier's steady
state and not the handshake's cost on a quiet lane: it is what a *cold* process pays to
establish four lanes, a control session and its first channels while the churn probe is
already arriving.

**Two readings since then sharpen it, and both refute the obvious next step.**

1. **Establishment is not the cost.** The client's own log: control session at **+1.5 ms**,
   `pool-stats: tunnel pool established tunnels=4 established=4` at **+46 ms**. Four lanes
   in forty-six milliseconds, and `establish_initial` already spawns them concurrently in
   one `JoinSet` — so "the lanes are built one after another" was never true, and there is
   nothing there to win.
2. **The first stage is not slow from the start.** Its first two windows read
   `paired_p50` **1 111 and 1 094 µs** with 5 and 24 arrivals; from the third window on it
   is **4 865–13 360 µs** at 34–37 arrivals a second and stays there for the whole stage.
   The break happens about a second in, when the churn probe reaches full rate and the
   bulk spine starts.

So the slow phase is **load**, not cold start, and it is not the arrival rate either: the
last stage carries the same 36 arrivals a second over the same bulk spine and reads
1 166 µs. Something differs between the first minute of a run and the twentieth with the
same offered load, and that difference — not the handshake, not establishment — is what
the next round measures: the same counters (`dwell`, reader park, lock, `credit_blocked`)
compared at minute one and minute twenty, to find which one carries the factor of ten.

That is a measurement, not a theory, and the plan's ban stands untouched: no warmup stage,
no deleted first stage.

### The first minute is credit pressure; the twentieth is reader parks (2026-11-04)

The same counters, from one full-scale run, at minute one and in its last ten minutes:

| counter (server) | minute 1 | t = 600–1 200 s |
|---|---|---|
| send-buffer dwell p50 | 0 µs | 6 µs |
| **reader park p99** | **434 µs** | **40 704 µs** (max **2 001 414**) |
| connection lock p99 | 46 µs | 2 µs |
| **writes refused by credit** | **8 489 / s** | **3 / s** |
| writes refused by the age gate | 1 678 / s | 0 / s |

**The two phases are opposite, and neither is the handshake.**

- **Minute one**: credit refusals at eight and a half thousand per second, tiny parks, no
  dwell. The bulk spine is filling the peer's window and every write — including an
  arrival's — waits on it. That is the phase whose arrivals read 5–13 ms.
- **Late**: almost no credit refusals, and readers parked **40 ms at p99 and two seconds at
  worst**. The window is not the constraint any more; readers are waiting for bytes that
  the peer has not sent yet.

The earlier reading of the same run — "dwell 0, park 54 µs, so nothing is queued" — was
taken from the *first* 120 seconds and averaged over a phase whose parking is genuinely
tiny. It was the refusals, not the dwell, that carried the cost, and they were not in that
comparison at all.

**So the arrival's slowness in the first stage is credit pressure from the bulk spine**, and
`OPEN_RESERVE` — the byte reserve that exists to keep an opening writable — is a *byte*
reserve against a *connection-wide* credit, which is exactly the wrong shape for this. The
next round's change is small and measurable: make the reserve big enough that one opening
flight always fits, and watch `refused credit` at minute one and the first stage's
`paired_p50` together.

### The reserve works, and the first stage is not refused anything (2026-11-04)

A refusal now says whether the stream that was refused was still opening. A 0.3-scale
timeline, refused writes per second by bound:

```
credit=478   window=12 296   aged=128   opening=0
credit=2 915 window=10 317   aged=973   opening=0
credit=2 222 window=8 327    aged=0     opening=0
```

**`opening=0` in every window.** The eight-payload reserve does exactly what it was built
to do — an opening finds room against a window the bulk spine has filled — so the first
stage's slowness is **not** a refused arrival, and the hypothesis that motivated the counter
is dead.

Also worth reading: the bound that refuses most often on a shaped run is the **congestion
window**, not credit and not the age gate — a different picture from the minute-one window
of the run compared earlier, which is why per-window numbers and not per-run totals are the
only ones this file should quote.

**So what is left for the first stage?** Not establishment (46 ms), not credit (0 refused
openings), not the send buffer (dwell 0 µs), not a lock (46 µs). What remains is the
handshake's own round trips spent while the host is busy — and the one measurement that ever
moved it was the worker count, 26 501 µs at four workers against 12 970 at eight, on a cell
whose run-to-run range is wider than the effect.

That makes the next step an A/B, not a change: **eight workers against sixteen, both
binaries in one campaign, interleaved**, judged on the first stage's p99 and `bulk-n`. If
sixteen wins outside the paired noise, the default moves and the first stage improves; if it
does not, the first stage is host-bound and this file says so.

### In flight: eight workers against sixteen (2026-11-04)

**Result — refuted by its own A/A twin.** Channel-open `open_p50` in each arm's first ninety
seconds, half scale, three arms in one campaign:

| arm | workers | first 90 s | last 90 s |
|---|---|---|---|
| A | 8 | 18 108 µs | 1 151 |
| **B** | **16** | **14 298 µs** | 1 149 |
| A/A twin | 8 (same binary as A) | **15 742 µs** | 1 163 |

**The two copies of the same binary differ by 15 %, and sixteen workers is 9 % below one of
them and 21 % below the other.** The effect is inside the run's own noise, so it is not a
result and the default stays at eight. The last ninety seconds agree to within 1 % across
all three arms, which is the other half of the answer: the steady state does not care about
the worker count at all.

**So the first stage is host-bound, as the pre-registration said it would be if the lever
did not win.** The lever that removes the cost is the one that removes the round trips — the
parked sealing design — and nothing in the scheduling or thread count moves it. That is now
measured three ways: establishment (46 ms), refused openings (0), and worker count (inside
noise).

The A/B the previous entry calls for is running as this entry is written: two binaries
built from the same tree, differing only in `DEFAULT_WORKERS` (8 and 16, both `min`-clamped
to the core count), run as three arms in one campaign so the A/A twin measures the same
run's noise. Scenario: full-scale `timeline`. Result file when it lands:
`~/tmp/m61-workers.json`.

The binaries are saved so the comparison can be repeated without rebuilding:
`~/tmp/molehill-8w` and `~/tmp/molehill-16w` (distinct, verified by hash).

**What decides it**: the first `clean` stage's p99 against the run's own A/A floor, with
`bulk-n` inside 15 % as the throughput guard. If sixteen does not win outside that floor,
the first stage is host-bound and the entry above stays as it is — the honest answer, and
the one that has to be written rather than a change made anyway.

### There is no channel reuse in the pool — the third wrong mechanism in a row (2026-11-04)

`StreamLease` holds `stream: CarrierStream`: a **stream**, not a cached connection. Its
`Drop` decrements a counter and wakes the capacity waiters; nothing goes back on an idle
list, because there is no idle list. `open_stream` builds a new stream every time.

So the entry two rounds ago — "the pool takes a channel back after its visitor ends and
hands it to the next visitor" — **is wrong and is withdrawn**, and with it the contract
that entry proposed. That is the third mechanism in this area to be refuted by reading the
code after being written down as a finding (the validation stream, the false ack, and now
reuse). The pattern is clear enough to state as a rule for this file: **an instrument
finding names a symptom; only the code names a mechanism.**

What the evidence actually says, with the reuse story gone: the server retires stream 1
because its own group ended — `pair_striped_group` hands `read` and `write` to
`spawn_group_tagged`, and when both directions finish the group's channels are dropped, each
`StreamIo` calling `close_stream`. The client, still holding its side of that stream, writes
into a stream whose peer is gone, and every such datagram is dropped with a debug line. So
the question is not who *reused* the channel but **which side ended it first, and whether it
was entitled to** — and that is a question about the stripe layer's two directions, which
the frame and byte counters are already placed to answer.

**No code change this round.** The next attempt starts from that corrected question, and it
does not get to skip it: three wrong mechanisms in one area is enough evidence that the
next sentence written here about the sealed path must quote a line of code.

### What the corrected question is, precisely (2026-11-04)

With the reuse story gone, the failing run's numbers read as follows, and all three lines
are from the same run:

- the **client** wrote all 8 388 608 bytes of response into its four stripe streams, and
  every one of them closed with `written == next == acked` — the carrier put them on the
  wire and the peer acknowledged them;
- the **server's** receive direction delivered **167 of 257 frames** and then ended,
  `missing = 0`, which means it delivered every frame it ever *saw*;
- the visitor received 5 472 256 bytes, which is 167 frames, and the arithmetic closes.

So the bytes were sent, acknowledged, and not read by the layer that was supposed to read
them. A FIN cannot overtake data here: `fin_sent` is only set once `unacked == 0`, and
`poll_transmit` offers stream data before fins. That leaves **the server's stripe readers
ending before their streams were drained**, and the instrument for that is per-stripe at
`recv_loop`'s end: bytes read against the stream's `written` and `acked`, one line per
stripe.

If those agree, the layer read everything and its frames went elsewhere. If they do not, a
reader stopped with bytes still available — and that is a reader-park or a wake problem in
`lane.rs`, not in the carrier's close path. Either way the next entry quotes a line.

### `just check` cannot run while the advisory database is unreachable (2026-11-04)

`just check` stops at its first step: `cargo audit` fetches
`github.com/RustSec/advisory-db` and the fetch fails. The audit itself passes offline —
`cargo audit --no-fetch` loads 1 403 advisories from the cached copy and finds nothing — and
the rest of the chain was run by hand: `cargo fmt --check` clean, `cargo clippy
--all-targets` with no warnings, the library suite 344 passed, the tree clean.

So the tree is sound and the gate is blocked by the network, which is what this file has
recorded before. Nothing about the carrier depends on it, but no commit should be described
as "gate-green" today without saying which half of the gate ran.

### The request crosses and the response never starts (2026-11-04)

The trace could not be read until the group id carried the **role** — both processes number
their groups from one, and the parked patch had lost the top bit the client/server split
needs. Restored, one failing run reads:

```
server group=4294967297  send direction finished, frames and bytes seq=257 bytes=8388608
client group=2           receive direction finished frames=257 seen=257 written=8388608
Error: echo service closed after 450560/8388608 bytes
```

- the **server read all 8 388 608 bytes** from the visitor and put them on the stripes (257
  frames);
- the **client read all 257 frames** and wrote 8 388 608 bytes into its backend;
- **the response direction never appears at all**: no `send direction finished` for the
  client's second group, and the server's receive direction never logs either.

So the request crosses in full, both ends agreeing. The response — the backend's echo back to
the visitor — never starts. Ten seconds later the group's grace expires, drops the visitor's
write half, and the test sees a clean EOF after 450 560 bytes.

**That is the whole defect, and it is in the response direction's *start*, not its end.** The
client's `send_loop` for that group is alive (its `recv_loop` for the same group finished) but
nothing it reads is reaching the stripes; no `stripe source read waited` and no `stripe write
waited` line appears, so neither its source read nor its stripe write parked for twenty
milliseconds — which is only possible if it is not being polled at all.

**The next instrument is therefore one line in `send_loop`**: log the frame index and the
elapsed time on its first iteration, and again whenever it goes a second without a frame. A
direction that never logs its first iteration is a task that was never polled, and that is a
different fix from one that reads and writes and loses the bytes.

### The sealed path has more than one remaining defect, and that is the answer to "how far" (2026-11-04)

The first-frame line answers the question it was built for, and the answer is *both*:

```
stripe send: first frame read group=4294967297 bytes=32768   <- request direction starts
stripe send: first frame read group=2 bytes=32768            <- response direction starts
Error: echo service closed after 3844608/8388608 bytes
```

In this run the response direction **did** start, read 32 KiB and carried 3.8 MB before the
visitor's socket closed — where the previous run showed no response line at all. So the sealed
path's remaining failure is not one defect: there is at least one in which a direction never
starts and one in which it starts and stops early, and they appear in different runs of the
same build.

**That settles how far this arc is from landing**: not one fix away, and not measurable as a
single rate until each shape is separated. The instrument to separate them exists now (role-
tagged group ids, first-frame lines, per-direction frames/bytes, the grace's residue), and the
work of chasing each shape is its own arc — one this cycle has already spent twenty-five
rounds on and cannot finish by continuing to patch the same test.

**The decision this file records, and why**: the sealed design stays parked. What it proved is
real (handshake 1 771 µs → 14, `churn-16` p99 −63 %), its cost is real (bulk −62 %), and its
blocker is a channel-lifecycle problem with at least two shapes. Landing it needs its own
cycle with the instruments now in place, not another patch inside this one.

**What this cycle leaves behind instead** is on the current tree, all measured and all
independent of sealing: the credit-wedge fix (five stages restored, `clean` 78 → 3.5 ms), the
grace fix, the per-stream sealing rule and prologue that made the diagnosis possible, the
worker default, four harness fixes, and a 0.3-scale sweep that passes `bench gate`.

### In flight: does the tool still answer when a stage's instrument fails? (2026-11-04)

The retry re-measures a stage whose `iperf3` client died, and in several runs the retry failed
the same way — which says the path was still broken seconds later but not *what* was broken.
The harness now asks, before the retry changes anything, whether the tool still answers on the
port the stage failed to carry on, and records it in the sample's evidence: reachable means a
carrier that stopped forwarding, unreachable means a listener that went away.

A full-scale sweep is running with that in place (`~/tmp/m63-reach.json`). It is the only
remaining gate failure that does not depend on the parked sealing design, so it is the one worth
finishing first: every lost stage is a cell the gate reports as missing, and until the harness
can say which of the two it is, each loss costs a round to investigate.

### Two bench chains at once destroys both, and a clean 0.3 sweep passes (2026-11-04)

The full-scale sweep launched last round lost **every** stage — `iperf3 exit -9: no output` on
all eight, the new reachability field absent, and in its log `Cannot open network namespace
"mhbsrv"`. The cause is not the carrier and not the harness code: the worker-count A/B from the
round before was **still running**, and its teardown deleted the namespaces the sweep was using.
Two chains overlap, both die, and the one that survives writes a record that means nothing.

That rule is already in this file ("do not run two bench chains at once"). This round broke it,
and the cost was a 22-minute run and a round. The lesson is not new, only re-earned: **check for
a running chain before launching one**, and treat `Cannot open network namespace` as proof that
*another* run owns the host, not as a reason to retry.

On a clean host, immediately after: a 0.3-scale sweep with the new probe in place —

| stage | p50 | p99 |
|---|---|---|
| `clean` | **2.74 ms** | **44.4 ms** |
| `rtt100` | 402 | 423 |
| `loss1` | 42.4 | 1 053 |
| `loss5` | 402 | 1 670 |
| `rate100` | 84.9 | 106 |
| `rate20` | 170 | 217 |
| `jitter` | 81.5 | 109 |
| `clean#2` | **2.20 ms** | **22.4 ms** |

**`bench gate: PASS`** — every stage measured, both clean stages inside the SLO with room, and
this is the second independent pass at this scale. The reachability probe did not fire because
nothing failed, which is the correct way for it to be idle.

**So the harness is sound and the carrier passes at 0.3 scale.** What remains is the full-scale
run, and it must be launched when nothing else is running.

### The probe's answer was thrown away with the cell it was recorded on (2026-11-04)

A full-scale sweep on a verified clean host reproduced the familiar shape — `clean` 58.1 / 281.1
ms, then **five stages lost from `loss5` onward**, each retried and each failing again — and the
new reachability field was **absent on every one of them**, even though `retried_after` was set.

That is an instrument bug, not a measurement: `_one_stage` recorded the answer on the first
attempt's cell and then **returned the retried cell**, discarding the first one and its evidence
with it. The facts are now copied across before the retry's cell is returned.

**So the round's datum is still missing, but the shape is not**: five consecutive stages after
`loss5` fail, and their retry fails the same way, on a host where nothing else was running. That
is deterministic and it is the last gate failure that does not depend on the parked sealing
design.

### One gate failure left, and the first stage passes at full scale (2026-11-04)

A full-scale sweep on a clean host, with the evidence fix in place:

| stage | p50 | p99 |
|---|---|---|
| `clean` | **2.27 ms** | **20.0 ms** |
| `rtt100` | 402 | 404 |
| `loss1` | 42.4 | 1 068 |
| `loss5` | 403 | 2 477 |
| `rate100` | 177 | 199 |
| `rate20` | 360 | 402 |
| `jitter` | 82.1 | 108 |
| `clean#2` | — | — (lost) |

**`bench gate`: 1 failure** — the coverage of `clean#2` and nothing else. The SLO passes at full
scale, the first stage reads 2.27 / 20.0 ms against a 50 ms limit, and every shaped stage is
measured. That is the best gate state of this whole cycle: five failures, then two, now one.

**And the probe answered**: the lost stage records `reachable_after_failure = True`. The tool
still accepts on the port it just failed to carry on — so it is not a dead listener, it is a
carrier that stopped forwarding for one stage and then kept accepting connections. The retry
re-measured and failed the same way, so the state lasts longer than one stage.

**That is the last gate item, and it now has an address**: a lane that stops carrying after
roughly twenty minutes of run while its socket still answers. The instruments for it exist — the
credit refusals, the age gate, the reader parks, and the close line — and the one counter this
run can name is that the failure comes *late*, in the stage that follows the last shaped one.

### The lost stage is a bulk-only silence, with every instrument idle (2026-11-04)

The counters from the run whose only failure was `clean#2`, over the last 160 seconds — the
stage that was lost:

| counter | server | client |
|---|---|---|
| writes refused by credit | **0** | — |
| writes refused by the age gate | **0** | — |
| writes refused by the window | 24 / s median | — |
| `mhd stalled` lines | **0** | **0** |
| `credit_blocked=true` | **0** | **0** |
| reader park p99 | 46 930 µs (last 447 743) | 48 484 µs (last 560 943) |
| send-buffer dwell | 9 µs | — |
| pairing p50 | **1 357 µs** | — |
| visitors whose forwarding read ran / returned bytes | **5 760 / 5 758** | — |

So nothing in the carrier is stuck: no refusal, no stall, no credit block, dwell of
microseconds, visitors pairing in 1.4 ms, and *every* visitor's first read returning bytes.

**And the bulk transfer carried nothing for the whole stage.** The churn probe kept working
through it — thirty-six arrivals a second, paired in a millisecond — so the tool was serving
small connections perfectly while one bulk flow moved zero bytes. Both ends' readers park on
empty because there is nothing to read: the bulk data never entered the tool.

That is the shape to chase, and it is narrower than any hypothesis this file has carried: **not
a lane that stops, but a flow that never starts, on a tool that is otherwise healthy.** The next
instrument is a per-visitor byte count with the worst one reported per window — `fed` says a
visitor's first read returned *something*, and the question is now how much, and for how long it
stops growing.

### The starved-group counter names iperf3's control connection, not the defect (2026-11-04)

The counter found its thirteen groups and each now names itself:

```
mhd-stats: a group ended having carried almost nothing bytes=972 alive_ms=152079
mhd-stats: a group ended having carried almost nothing bytes=963 alive_ms=123809
mhd-stats: a group ended having carried almost nothing bytes=966 alive_ms=122305
```

**972 bytes over two and a half minutes is `iperf3`'s control connection**: the cookie
handshake, and then an idle socket for the length of the test while the data flows on four
other connections. So the counter is counting a connection that is *idle by design*, and its
"13" was a coincidence of arithmetic, not the defect. **It is withdrawn as a defect signal**;
it stays as a lifecycle count, which is all it ever was.

Two more readings from the same run, and both matter more than the counter:

- **Zero** `stripe write waited`, **zero** `stripe source read waited`, zero `stripe receive
  stalled` — and that is not evidence of health. Those lines are logged *after* a read or write
  returns, so a loop parked for ever logs nothing at all. A silent instrument and a healthy
  path look identical in a log read this way, which is the fourth time that has cost a round.
- The gate is at **one or two failures depending on the run**: `clean#2` was lost again, and
  `clean`'s p99 read **261.7 ms** on this run against 20.0 on the previous one, same binary.
  The first stage's p99 across runs is now known to span **20 to 262 ms** on this host, so the
  SLO verdict is a property of the host's moment, not of the carrier.

**What the file should do next, and does not need another sweep to say**: `clean#2` is the
*last* stage and has failed in nearly every full run. A stage that fails because it is last is
not a stage-level defect; it is something about the run's end, and the only instrument that can
see it is one placed at the *stage boundary* — the pool's state and a single served visitor —
rather than inside the carrier, where every counter reads clean.

### The boundary now asks the tool to serve a visitor (2026-11-04)

Five rounds of instruments inside the carrier have each ended the same way: the counters read
clean while the stage carries nothing, because a loop parked for ever logs nothing and a stage
that never gets data never moves a counter. So the question moved outside the tool.

Before every stage's condition and before its own traffic, the harness now connects to the
**echo** service through the tool, sends one byte, reads the echo back, closes, and records the
outcome and the time in the sample's evidence (`echo_probe`). One connection, three seconds at
most, no effect on the stage it precedes.

That is the instrument the last round's entry asked for: at the boundary, `ok` in milliseconds
means the tool was serving visitors and the stage's own client is what failed; `fail` or `eof`
means the tool had stopped, and then the carrier's silence is the thing to explain. **`clean#2`
is where it will be read first**, because that stage fails for being last and nothing inside it
can say why.

### The tool serves a new visitor while one flow stops delivering (2026-11-04)

The boundary probe answered on its first full run, and the answer is not the one the exit was
built for:

| stage | outcome | boundary probe | stage |
|---|---|---|---|
| `clean` | measured | **ok, 5.84 ms** | 3.45 / 56.2 ms |
| `rtt100` | measured | ok, 406 ms | 402 / 403 |
| `loss1` | measured | ok, 45.4 ms | 42.1 / 1 092 |
| `loss5` | measured | ok, 406 ms | 403 / 2 448 |
| `rate100` | **lost** | **ok, 85.8 ms** | — |
| `rate20` | **lost** | **ok, 167 ms** | — |
| `jitter` | **lost** | **ok, 91.5 ms** | — |
| `clean#2` | **lost** | **ok, 5.49 ms** | — |

**Every failing stage answered a complete visitor round trip at its own boundary** — `clean#2` in
5.5 ms on a clean path — and then carried nothing. So the tool was not wedged, its pool was not
empty, and its listeners were not gone: **it served a fresh connection perfectly and then failed
to carry one bulk flow.**

That kills the last shape this file was carrying ("a lane that stops") and replaces it with a
narrower one: **a flow whose source stops delivering while the tool keeps serving everyone else.**
The send loop's own log could never say so, because every "read waited" line is written after the
call returns — a read that never returns prints nothing. It now reads under a two-second deadline
and records a park with the group, its frames and its bytes, so the next lost stage will name the
flow instead of the silence.

**Where the gate stands**: `clean` 3.45 / 56.2 ms (inside the SLO at the median and just outside
at p99), four stages lost. The four are the ones after `loss5`, which is the run's shape whenever
it goes wrong — and now the instrument to read them is in place.

### Eight of eight measured, and the gate is down to the SLO alone (2026-11-04)

A full-scale run with the read deadline in place:

| stage | p50 | p99 |
|---|---|---|
| `clean` | 6.06 ms | **134.1 ms** |
| `rtt100` | 402 | 462 |
| `loss1` | 42.2 | 1 073 |
| `loss5` | 402 | 1 700 |
| `rate100` | 84.8 | 96.6 |
| `rate20` | 170 | 192 |
| `jitter` | 80.8 | 107 |
| `clean#2` | **1.89 ms** | **3.91 ms** |

**`bench gate`: 1 failure — the SLO on `clean`'s p99, and nothing else.** Every stage measured,
the coverage complete, and the last stage reading 1.89 / 3.91 ms. That is the first run this
cycle in which the gate's only complaint is the number the plan says may not be relaxed.

**And the read-park instrument is honest about its own limit**: 579 parks across **13 groups**,
every one of them at `bytes = 166` and `frames = 2–3`, each repeated 61–75 times over minutes. A
connection that carries 166 bytes and then waits is `iperf3`'s control connection, idle by
design — the same thirteen the starved counter found. So the park line names *a* flow that stops
delivering and cannot, on its own, say whether that flow *should* be delivering: an idle
connection and a wedged one look identical to a carrier.

**That fixes what the last mile actually is.** Not "a lane that stops" (the boundary probe refuted
it), not "a flow that stalls" (idle flows park the same way), but **the first stage's p99**, which
spans 20 to 262 ms across runs of one binary on this host and is therefore a property of the
moment, not of the carrier — and the one thing that would remove it for good is the round trips
that the parked sealing design deletes.

### The first stage's own noise is bigger than the SLO (2026-11-04)

Two rounds of one campaign, one arm, one binary, half scale — the closest thing to an A/A this
harness can do for the timeline:

| stage | round 0 | round 1 |
|---|---|---|
| `clean` p50 | **5.25 ms** | **65.97 ms** |
| `clean` p99 | **171.5 ms** | **223.0 ms** |
| `clean#2` p50 | 2.18 ms | 2.17 ms |
| `clean#2` p99 | 3.46 ms | 22.53 ms |

**The first stage's median moves by a factor of twelve between two consecutive rounds of the same
binary**, and its p99 never comes near the 50 ms limit in either. Across every full-scale run this
cycle has collected, `clean`'s p99 spans **20 to 281 ms** — a factor of fourteen — with the same
code.

**So the SLO cannot be decided by one run on this host, and that is a measurement, not an excuse.**
A cell whose own within-campaign spread exceeds its threshold will pass or fail according to which
minute it was measured in. Two consequences the plan has to face:

1. **A defensible release needs the distribution moved, not the tail clipped.** The only lever
   measured to move it is the round trips per arrival — the parked sealing design — because
   everything else (establishment, credit reserve, worker count, scheduling) has been tested and
   refused by its own numbers.
2. **Until then, the honest record is what the gate says on a given run, plus this floor.** Quoting
   a green SLO from one run, or a red one from another, would both be wrong: the file now carries
   the spread, which is the only number that makes either verdict readable.

`clean#2`, for its part, is stable to 1 % at the median across the two rounds — so the *last*
stage is not the noisy one. The noise belongs to the stage that starts a run.

### The sealed failure is the *backend* stopping, not the carrier (2026-11-04)

With the read deadline in place, the sealed striped test's failure finally names its own shape:

```
stripe send: first frame read group=2 bytes=32768
stripe source read has not returned for two seconds group=2 frames=119 bytes=3844608   (x3)
Error: echo service closed after 3844608/8388608 bytes
```

Two things match exactly: **the group read 119 frames / 3 844 608 bytes and then its source stopped
delivering**, and **the visitor received 3 844 608 bytes**. The direction is the *client's* response
path, whose source is the local **echo backend** — the bench's own Python service.

So the flow did not stall inside the carrier: the carrier delivered every byte it was given, and
then waited for a source that never sent the rest. The request side had already crossed in full
(8 388 608 bytes, both counters agreeing), so the backend had eight megabytes to echo and stopped
after 3.8.

**That reframes the whole remaining investigation.** The "flow that never starts" the boundary
probe left open is, at least in these runs, **a bench backend that stops echoing under load** —
and the same backend serves the churn probe, whose thirty-six arrivals a second kept working
because each connection is small and quick. A bulk echo through one core, with the run's own load
on the same host, is a different matter.

**The test that decides it is one line of grep away**: if `stripe write waited` never fires in the
same failure, the client was not blocked writing into the stripes and was genuinely waiting for its
backend; if it does fire, the frames had nowhere to go and the carrier is implicated again. That is
the first thing the next round does, and it costs one nine-second run rather than a sweep.

### The grep answered: the client was waiting for its backend (2026-11-04)

The one-line test the previous entry asked for, on a failure caught in four runs of the rebased
sealed tree:

```
stripe write waited                                  count: 0
stripe send: first frame read group=2 bytes=32768
Error: echo service closed after 793600/8388608 bytes
```

**Zero.** The client's send direction never blocked writing into the stripes, so the frames had
somewhere to go and the loop was genuinely waiting for its **source** — the bench's local echo
backend. That confirms the previous entry's reading rather than the carrier's.

**So the sealed striped test's remaining failures are, in these runs, the bench backend stopping
under load**, not the carrier failing to carry. That does not make the sealed design landable — it
still needs the lifecycle shapes separated and its 8/8 — but it does mean the last several rounds
were measuring the load generator.

> **Retracted below.** The next entry measures the backend and finds it cannot have
> blocked: it accepts 19 MiB with the peer reading nothing. The observation this
> paragraph rests on stands; the inference from it does not.

**PAUSED HERE at the user's request (round 72).** State as left:

- branch `feat/v1.0.0`, tree clean, `just check` green, HEAD `810f913`;
- the parked sealed tree is `~/tmp/stage3-sealing-parked.patch` and applies to today's HEAD with
  one conflict in `stripe.rs`'s send tail, resolved by keeping `note_group` plus the group id;
- the gate's last full-scale run: **eight of eight stages measured**, `clean` 6.06 / 134.1 ms,
  `clean#2` 1.89 / 3.91 ms, **one gate failure — the SLO on `clean`'s p99** — with that cell's
  within-campaign spread measured at **12× at the median**;
- the next step, in one sentence: **give the bench's echo backend enough capacity that it is not
  the load, then re-run the sealed test to see whether it is eight of eight.** *(Done, and it
  refuted the premise: see the next entry.)*

### The echo backend is decoupled, and it cannot have been the stall (2026-11-04)

The harness's echo service ran one thread per connection, alternating `recv` and
`sendall`. That shape *couples its two directions* — it cannot accept a byte
while it is blocked writing — and the previous entry read the sealed test's stall
as exactly that. **The coupling is real; the attribution was not.** Measured on
this host, both arms from their parsed source, one run each:

| | bytes it accepts while the peer reads nothing (3 s) | full-duplex 8 MiB |
|---|---|---|
| old: one thread, `recv`/`sendall` | **16–19 MiB** | completes, 0.01 s |
| new: reader → bounded queue (64 × 64 KiB) → writer | **22–27 MiB** | completes, 0.01 s |

The number moves ±5 MiB between runs because it is socket autotuning, so the
honest reading is the *floor*: **every run absorbed more than an 8 MiB transfer
needs, in both shapes**. A backend that could have accepted the whole echo
cannot have been blocked writing it, so "the backend stopped delivering because
it was blocked" is refuted for this transfer — and with it the sentence that
three rounds of carrier work had been spent on the load generator. What the
sealed trace actually says is narrower: the client's response direction was
*reading*, and its source returned nothing for two seconds. Why a source with
that much room had nothing to give is open again.

Three things are kept, each on its own merit rather than on that story:

- the **decoupling**: the queue is worth 4–8 MiB of headroom in every run, and a
  load generator that stalls when its peer is slow is one whose stall will
  eventually be blamed on the carrier;
- an **`ECHO DONE read=… wrote=…` line** for any connection over 64 KiB, so a
  stalled transfer can be attributed by the backend's own counters instead of by
  inference. Small connections stay silent: at four thousand arrivals a second a
  line per connection would make the log the load;
- a **re-runnable check**, `bench/src/bench/utils/echo_check.py` (outside the
  gate, seconds, no root): full-duplex byte equality, the coupling bound, and
  the presence of the announcement.

Its first version reported a log-formatting defect that did not exist: it sliced
`ECHO_SRC` out of the file's raw text instead of taking the parsed constant, so
the child received `\\n` and printed a literal backslash. Fixed, and the reason
is written into the function — it is the same mistake as measuring a copy of the
thing instead of the thing.

Neither is a claim that the sealed path is fixed. The check that would settle it
is on the carrier's side and is still the next round's: for the failing direction,
count bytes *read from the source* against bytes *queued into the group*, so a
source that has nothing and a group that will not take it stop looking alike.

### Stage 4 — RACK plus PTO, instead of one timer for everything (latency)

Split what one `MIN_RTO` decides today, following the shape MsQuic documents
([loss_detection.c](https://github.com/microsoft/msquic/blob/21a4d9c4affa8da13b7f3758ed02c6180e567629/src/core/loss_detection.c)):
a **RACK** deadline for packets with later acknowledgement evidence, a **PTO**
that sends one or two probes instead of declaring a flight lost, and the existing
peer-silence rule for a dead path. Retire the global "oldest data packet older
than the RTO blocks every non-opening write" gate; the opening reserve stays as
anti-starvation, not as a mask.

Take the ack-timing rules from a reference that has them
([quic-go's received-packet tracker](https://github.com/quic-go/quic-go/blob/aa265c3fc6489e3bfa8071b333be95e0e887a51d/internal/ackhandler/received_packet_tracker.go)):
ack a new stream's first bytes, a packet previously reported missing, and a new
gap immediately; aggregate only within a bounded delay.

- **Gate**: a single lost opening recovers without a wide-timer step; `bulk-n`
  does not storm; `churn-16`, `rr-16`, both clean stages and `loss1`/`loss5`/
  `rtt100` each judged on their own cell.

### Stage 5 — latency-aware internal scheduling (latency)

With the arrival path cheap, the remaining queue is the one bulk makes. Replace
the flat per-stream round robin with classes: control and repair; a stream still
opening; interactive; bulk. Borrow the queueing shape from Quinn's
[priority plus recency round-robin](https://github.com/quinn-rs/quinn/blob/2c5750139fa09aab588bf0f939c83f521d658baf/quinn-proto/src/connection/pacing.rs)
and quic-go's urgency queues, but with **no new configuration key** — the class
is derived from the stream's own state.

- **Gate**: arrivals under bulk are not slowed; `bulk-n` unmoved; a burst of new
  streams cannot starve a mature one (bounded quantum per class).

### Stage 6 — a rate- and BDP-driven window (throughput, third)

The current window grows additively and is never cut, `slow_start` latches off at
the first loss forever, and a flight may outlive its own timer. Add delivery rate
and `app_limited` sampling (the shape in
[MsQuic's BBR](https://github.com/microsoft/msquic/blob/21a4d9c4affa8da13b7f3758ed02c6180e567629/src/core/bbr.c)),
then a simplified Startup/Drain/ProbeBW with a token-bucket pacer whose burst is
bounded by RTT and MTU.

- **Gate**: `bulk-n` and `rtt100` improve or hold; `rate20`/`rate100` do not
  collapse; RSS and CPU are compared only after latency and throughput hold.
- **Revert**: the controller alone, keeping stages 2–5.

### Stage 7 — memory, then processor

Only after the two axes above hold: reclaim windows and blocks from the real peak
flight, keep the pooled receive blocks and the sliced send payload, and give the
recovery table bounded, low-allocation storage. Do not retry region pinning, a
piece per datagram, or a chunk queue without a cursor — each was built, measured
and rejected.

### The first stage is the carrier's, and its tail is the per-stream handshake queueing (2026-11-04)

One campaign, full-scale `sweep` schedule, one round, `control` and `molehill` in
the same run, same binary, `MOLEHILL_MHD_STATS=1`. This is the row that decides
whether the first stage is the run or the carrier:

| arm | stage | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|---|
| control (no tool) | `clean` (first) | 3 035 | **1.58** | 2.03 | **2.57** | 5.41 |
| control | `clean#2` (last) | 3 034 | 1.57 | 1.90 | 2.28 | 3.32 |
| molehill | `clean` (first) | 2 934 | 2.94 | **15.15** | **112.85** | 244.72 |

**The tool-free path's first stage is 2.6 ms at p99 and does not degrade for
being first.** molehill's is 112.85 — 44× the control on the same stage, 38× its
own p50. So the first-stage failure is a property of the **carrier's arrival
path**, not of the harness, the host, or the position in the schedule. And it is
not "the first stage warms up": its first ten-second bucket already reads p99
**37.7 ms** and every bucket after it reads 80–209, with a flat p50 near 3 ms and
a p90 of 7–21. A stage that began fast and slowed would look nothing like this.
*(The earlier "first two windows read 1 111 and 1 094 µs, then it slows" was
measured on the server's own `paired_p50`, a different instrument on a different
run; this cell's probe disagrees with it and the older sentence is not a reading
of this one.)*

The shape says which part of the arrival to look at, and the carrier's own
counters name it. In the same second the probe reads 112.85 ms, the server's
`visitor arrival, split at the pairing` window reads `paired_p50_us 1 901` and
`paired_p99_us 112 454` — the probe and the pairing agree to 0.4 %, so the tail
is **accept → a channel in hand**, and nothing after it.

Then the split, over every slow (`≥ 5 ms`) opening the client logged in that arm
(1 141 of them):

| field | p50 | p90 | p99 | max |
|---|---|---|---|---|
| the whole open (`us`) | 11.50 ms | 41.22 | 128.72 | 371.74 |
| **the Noise KK inside it (`hs_us`)** | **11.50** | **41.22** | **128.71** | **371.74** |
| the acknowledgement (`ack_us`) | 0.00 | 0.00 | 0.00 | 0.00 |

**The handshake is the entire open — a share of 1.000 at p50 and at p90** — and
the server says the same from its side: of the openings slow enough to log,
`sel_us ≈ 0`, `hello_us ≈ 0`, and `hs_us` is `total_us` to the microsecond. Its
own bound snapshot says this is not a budget: `credit` 16.5 MB, `buffer` at its
32 MiB ceiling, `queued_streams` 0, `retransmits` ≤ 9.

So the cost is **not** the KK's cryptography. On an idle lane the same handshake
measures 1 771 µs. Here its **median is 11.5 ms**, which is the two small records
it exchanges **waiting on a lane that is carrying bulk** — the one thing the
five-millisecond-or-finer instruments inside each stage could never see, because
what they measured was the CPU, and what the arrival pays is the queue.

Two consequences for the plan, and this is the whole point of the row:

1. **sealing (stage C) attacks exactly this**, and now for a measured reason
   rather than a design preference: it deletes the two records and the round trip
   they wait on. The parked patch's own end-to-end A/B put `churn-16` p99 at 3.59
   against 9.74 ms with the handshake at 14 µs;
2. **the scheduler (stage F1) is not made redundant by it** and is not
   speculative either: the measured defect is a small record queueing behind a
   full run, which is what a class-aware transmit order exists to prevent. C
   first, because it removes the work; F1 after, because the remaining small
   records (selector, hello, prologue, ack) will still queue.

**And the harness still eats four stages.** Everything after `loss5` failed with
`iperf3 error: control socket has closed unexpectedly` — `rate100`, `rate20`,
`jitter` and `clean#2`, i.e. no `clean#2` for molehill at all, which is why the
table above has no fourth row. That is the release gate's coverage problem and it
is *not* the carrier: it must be fixed before any sweep can be published.

### The lane root is the handshake's secret, and the transcript only salts it (2026-11-04)

The parked sealing design derived each stream's key from the lane's **handshake
hash**. That is wrong, and it is wrong in the direction that matters: the
handshake hash is a digest of the messages both ends exchanged. Anyone who
watched the handshake — or who holds either static key and can replay the
exchange — recomputes it. A key schedule built on it would move forward secrecy
from per-stream to per-lane and then hand it away, which is worse than the
per-stream handshake it exists to replace, because that handshake was at least
secret.

So [`LaneRoot`](src/transport/mhd/cipher.rs) takes two inputs and documents
why neither alone will do:

- the **raw split** — Noise's own secret output, the exact material
  `into_transport_mode` installs, reached through snow's `risky-raw-split`
  feature (`dangerously_get_raw_split`). Named "risky" by the crate because it
  exports key material; that is precisely what a derivation root needs, and it
  is the reason the feature is the right tool here rather than a re-implementation
  of the state machine;
- the **handshake hash**, as the HKDF **salt** — a public value that binds the
  root to one handshake without ever being derivable from it.

`HKDF-SHA256(salt = transcript, ikm = split[0] ‖ split[1], info = "molehill mhd
lane root v1")` gives the root; `StreamCipher::derive` then derives one
(direction, stream) key from it as before. A test pins both halves: changing
the secret while holding the transcript fixed changes every stream key, and so
does changing the transcript while holding the secret fixed — the regression
that would ship a transcript-only root while still passing every "both ends
agree" test.

Two consequences written down rather than discovered later:
- `from_handshake` answers `Option`, and `None` means **do not seal** — the
  caller falls back to the per-stream handshake instead of sealing under a root
  nobody computed;
- the nonce is the packet number, which is `u32` per connection. A wrap would
  reuse a nonce with the same key, so the sealing path must refuse near the
  wrap (or re-establish the lane) rather than wrap silently. 2³² datagrams is
  ~6 TB, which no test will reach — which is exactly why it has to be an
  explicit refusal and not an assumption.

### The sealed failure is not the harness, and its signature was not what it looked like (2026-11-05)

Two corrections, both from measuring with a detector that cannot be fooled by a
debug line:

- **`mhd transport selector timed out` is not the failure signature.** It appears in
  *passing* runs too — four streams whose selector never arrives while the group still
  gathers enough channels and carries the whole transfer. An earlier entry read it as
  the server waiting on the streams that mattered; it does not say that.
- **the test's echo is not the cause.** With a strict detector (`test result: ok`), the
  sealed striped test passes **3 of 8** with the coupled `io::copy` echo and **2 of 8**
  with the decoupled reader/queue/writer one. Reverted, for the second time and now
  with a detector worth trusting.

So the sealed path fails about five times in eight and nothing about the harness
explains it. The trace of a failing run is consistent from run to run: the client's
response direction — whose source is the local echo — reads a variable number of frames
(123 / 3 975 680 bytes in one run; 455 KiB, 440 KiB, 826 KiB in others) and then
`stripe source read has not returned for two seconds`, forever, while the visitor reads
EOF at that byte. The number is not stable across runs (the three appearances of
465 920 were one batch, not a law), so it is a stopping point rather than a boundary.

What is now excluded for this path: the two never-sent packet numbers (fixed), the
repair path itself (a sealed stream repairs a dropped packet correctly in a unit test),
the harness's echo (measured twice), and the control plane and every arrival stage
(measured in the rounds above). What is left is the stripe layer's own accounting over
sealed streams — where `MAX_SEALED_PAYLOAD` clamps the packet but the frame is 32 KiB,
and the reorder loop's `written` is the byte count the visitor's EOF is measured
against.

### A second never-sent packet number, and the deadlock gets better but not gone (2026-11-05)

The first leak was a refused packet consuming a number. There was a **second**, one
level further in and reachable only on the sealed path, which is why sealing alone
deadlocked stripes: `seal_stream_payload` can fail after `encode_stream` has already
written the header and after the number was taken, and the `?` there abandoned the
packet with its number consumed — a hole in the peer's cumulative frontier that can
never be filled, and every later acknowledgement re-declares everything above it lost.

A lane that cannot seal is now **closed**, where the pool can rebuild it, instead of
continuing with a hole: `note_no_key` counts it and `closed` is set with a reason of its
own. A silent repair loop becomes one lane restart.

Two more things were measured while looking:

- a sealed stream **does** repair a dropped packet. `a_sealed_stream_repairs_one_dropped_packet`
  drops one stream packet off the wire, fires the timer, and asserts every byte lands —
  it passes, so the loop is not in the repair path itself;
- with the fix, the sealed striped test improves from two-or-three of six to **four of
  six**. Not cured. Two of the failures stopped at **465 920 bytes**, which is the same
  number two runs reached earlier in the cycle — three times now for the same value, on
  a transfer whose frame size is 32 KiB, so it is not a frame boundary and it is not yet
  explained.

So sealing is still off. What has changed is that a lane with a hole now says so
instead of looking healthy, and the sealed path's failure rate is halved by a defect
whose mechanism is understood rather than by luck.

### Sealing is wired, tested, and OFF: a striped group deadlocks with it (2026-11-04)

The derivation is now plumbed end to end — `NoiseStream::session_roots`, a lane
root installed on `Conn`, per-stream ciphers, the frame flag, the sealed
`StreamState`, and `DERIVED_SELECTOR` — and it is **disabled by default** behind
`MOLEHILL_MHD_SEAL`, because on the one topology that shares a lane between
several streams at once it hangs. The A/B is six runs of the same test on one
host, the only difference being whether the root is installed:

| `striped_data_channels` (4 stripes, 8 MiB echo) | runs passed |
|---|---|
| sealing **off** (`io::copy` echo, per-stream KK) | **6 of 6** |
| sealing **on** | **3 of 6** |

What the sealed failures look like, from the carrier's own lines:

```
mhd stalled: bytes waiting unsent_ms=212 unacked_ms=213 credit_blocked=false
  backlog_streams=0 worst_stream=5 worst_stream_unsent=950569
  worst_stream_retransmits=0 worst_stream_next=117312 worst_stream_written=950570
mhd window lane=3 cwnd=76893 flight=0
```

`written` 950 570 against `next` 117 312 — 833 KB queued behind a send cursor
that stops moving — with the peer's credit untouched, **no retransmission ever
recorded**, and a congestion window of 63–77 KB against a flight of ~0. The
server, meanwhile, logs four `mhd transport selector timed out` half a minute
later: the streams it had accepted never received their first byte. Nothing was
refused (`unsealed_tags=0`, `no_key=0`, and the two new refusal counters this
round added — `unsealed_no_root`, `unsealed_no_tag`, `unsealed_no_key` — all
read zero on the next capture), so the bytes were **not sent**, not lost.

**That missing counter was itself the defect in the instrument.** A sealed packet
this end cannot open was dropped in total silence: `unsealed_tags` counts *tag
failures*, so a refusal for "no root here" left every counter reading healthy
while the peer waited out a thirty-second deadline. It is now counted apart and
logged once with the stream id, so the next round starts from a measurement
instead of from a timeout.

Two experiments were spent ruling out the neighbours, and both are recorded
because they cost the time:

- **the echo is not the cause.** Decoupling `tests/common`'s echo — the same
  reader/queue/writer shape the bench backend got — moved the failure rate from
  4 of 5 to 3 of 6, i.e. not at all, and the change was reverted;
- **the in-process stripe harness is not an instrument.** The parked patch's
  `a_sealed_stripe_group_carries_a_transfer` stalls at **zero** bytes without
  wake-ups, at **128** bytes with them, and at **384** bytes with sealing
  *disabled* — so it never carried a transfer in any configuration, and it was
  deleted rather than left red. (Its lesson is worth keeping: a harness that
  steps `Conn` by hand must call `wake_readers`/`wake_writers` after every
  datagram, because in production the pump task does, and a missing wake-up reads
  exactly like a stalled layer.)

So the arrival path's largest measured lever is built, proved on the arithmetic
(one handshake per lane instead of one per visitor), and **not shipped**, because
the striped configuration deadlocks and stripe count grows under backpressure in
production. The next round's first job is the deadlock, with the switch as its
reproduction, not a re-derivation of the cipher.

### The arrival's chain is closed, and it lands on the per-stream handshake (2026-11-05)

Three stamps on one wall clock, joined by group id — one `SystemTime` read per event,
gated on the stats switch, which is the only way to time an interval that crosses two
processes. 1 201 groups of one staged cell, each with exactly one request:

| interval | p50 | p90 | p99 |
|---|---|---|---|
| **control hop**: server sends the request → client's loop reads it | **0.056 ms** | 0.107 | **0.219** |
| **hand-off**: client's loop reads it → server pairs the visitor | 1.611 ms | 8.376 | **47.513 ms** |

And the server's own four stamps for the 188 pairings it calls slow:

| phase | p50 | p99 |
|---|---|---|
| `accept → pairing started` | **0.058 ms** | 0.123 |
| `pairing started → request enqueued` | 0.001 | 0.005 |
| **`request enqueued → first channel taken`** | **13.933 ms** | **122.455** |
| `first channel → forwarding command written` | 0.004 | 13.636 |
| total, `accept → paired` | 14.206 | 122.481 |

The two instruments agree to 0.4 % once the populations are matched — the cross-process
hand-off on those same 187 groups reads p50 **13.875**, p99 **122.430**. (The first
version of this entry called that a contradiction; it was comparing the hand-off's
median over *all* pairings with the server's median over the *slow* ones, which is the
population mistake this file keeps warning about and kept making.)

So the whole chain, in one column, for arrivals that are slow:

| | p50 |
|---|---|
| accept → pairing started | 0.058 ms |
| → the request is on the wire | 0.001 |
| → the client's control loop reads it | 0.056 |
| **→ the client has opened the stream and written its prologue** | **11.5 ms** |
| → the server takes the channel (prologue transit + push) | ~2 |
| → the pairing is woken and takes it | **0.004** |
| total | 14.2 |

**Every term is now measured, and the 11.5 ms is the per-stream mhd handshake** — the
same interval that reads `us == hs_us` with a share of 1.000, and the same one that
sealing deletes. The wake is 4 µs, the control plane 56 µs, the accept 58 µs, the
forwarding write 4 µs: none of them can be the tail any more, and no scheduling change
can touch a term that is four microseconds.

That closes the question this cycle spent four rounds on. The arrival is slow because
it performs a Noise handshake per stream, on a lane that is carrying bulk, and every
other explanation is now excluded by its own measurement. **The lever is sealing, and
its only blocker is the striped deadlock** — 3 of 6 runs, a repair loop with
`retransmits=1532`, narrowed but not yet cured.

### The tail is the pairing wait, and the arithmetic finally closes (2026-11-05)

The probe now splits its own round trip at the send. 555 arrivals of one staged cell:

| | p50 | p90 | p99 | max |
|---|---|---|---|---|
| the whole round trip | 14.44 ms | 72.78 | **228.56** | 457.28 |
| connect + send | 0.193 ms | 0.271 | 0.452 | 1.403 |
| **waiting for the echo** | **14.19 ms** | 72.65 | **228.40** | 457.05 |

So the tool's accept path is fast and the tail is the wait for the reply. The first
version of this entry guessed the *kernel accept queue* and was refuted in one
experiment: `ss -ltn` inside the server's namespace, sampled 134 times across a run of
the same cell, read `Recv-Q = 0` **every time**. Nothing was piling up.

The same run's server-side windows then close the arithmetic exactly:

| phase (34 one-second windows, median of the window p50s) | |
|---|---|
| `accept → paired` | **10.232 ms** (p99 42.2) |
| `paired → first byte` | 0.022 ms |
| the echo (first byte → reply byte written) | 3.296 ms |
| **sum** | **13.550 ms** |
| the probe's own "waiting for the echo" | **14.19 ms** |

**The pairing wait is the tail** — 10.2 ms of the 14.2 — and everything after it is
microseconds plus a three-millisecond echo. That is the same conclusion the first stage
reached from a different direction (`paired_p99` 112 ms in the full-scale run); what is
new is that it is now the *whole* explanation of a warm cell, not a fragment of a
cold one.

So the arrival's cost is: server accepts, **asks the client for a channel**, the client
opens an mhd stream, the server pairs. The client's own phases are known — dequeue
~0, `spawn → first poll` 4–12 µs, handshake 1.34–1.38 ms — so of the 10.2 ms, about
1.4 is the open and the rest is the **control-plane round trip plus the pairing
hand-off**: the request crossing to the client's control loop, and the channel's
arrival waking the pairing.

That is the last interval in an arrival that has never been stamped, and it is
pre-registered for the next round: emit the server's send time and the client's receive
time for the same `CreateDataChannelFor` group id — both ends are on one host, so epoch
milliseconds are comparable — and split the 8.8 ms between the control hop and the
pairing hand-off. Whichever it is decides between the two remaining structural
candidates, a server-initiated stream (which deletes the request entirely) and a
direct hand-off (which deletes the wake).

### The carrier's whole round trip is fast, and the tail is outside it (2026-11-05)

The interval every previous instrument stopped short of now has a distribution: from
the request byte the server's forwarding loop read to the first byte it wrote back to
that same visitor — the carrier's two directions plus the client and the backend.

| | p50 | p99 | max |
|---|---|---|---|
| a visitor's echo after its own request | **538–708 µs** | 1.2–23.3 ms | 23.3 ms |

In the same rounds the cell reads **p99 104.3, 37.7, 116.5 and 7.6 ms**. The echo's
p99 never reaches the cell's, and its *maximum* in a second is below the cell's p99
over thirty seconds — so for these runs **the carrier's round trip is not where the
tail is**, and everything this cycle built for the arrival is confirmed as fast from
the other side.

What is left is the two boundaries the server cannot see, and they are the probe's own
view of the tool:

- **before the request byte**: the visitor's `connect()` and its `send`, i.e. the
  *tool's* accept path. `accepted_at` is stamped after `accept()` returns, so a
  connection that sat in the kernel's accept queue is invisible to every counter here;
- **after the reply byte**: the probe's own read, which on this host is a Python
  selector and a syscall.

That is a smaller remaining question than the ones this file has been asking, and it is
the first time the answer has been *outside* the carrier for a population this large.
The next instrument follows from it: stamp the listener's accept boundary — the time
between a connection being ready and this end taking it — and compare it with the
probe's own measured round trip. If the accept path accounts for the rest, the SLO is
being decided by the server's accept loop and not by mhd at all, which changes what
the release claim can honestly say.

### Every stage of the arrival is microseconds, and a second population is not (2026-11-05)

The dispatcher's admission of a new lane — registry lock, admission table, `Conn`, a
`Shared`, a spawned sender task, two insertions — costs **p50 27 µs, p99 39 µs**. That
was the last hypothesis about the receiving end, and with it every interval the
arrival crosses now has a distribution:

| interval | p50 | p99 |
|---|---|---|
| a datagram's wait for its stream's accept | 7–8 µs | 12–90 µs |
| a lane admitted inside the receive callback | 27 µs | 39 µs |
| a stream's first byte waiting for the wire (dwell) | 2–11 µs | 0.2–0.5 ms |
| the pump's post to the socket | 10–25 µs | 20–104 µs |
| the client's spawn → first poll (`started_us`) | **4–12 µs** | — |
| the client's dequeue → prologue (`cmd_us − ready_us`) | ~0 | — |
| the per-stream handshake, every open | 1.34–1.38 ms | 1.78 ms |
| the server's pairing (`paired`) | 1.5–1.9 ms | 2–6 ms |
| the server's first forwarded byte (`first`) | 19–22 µs | 37–46 µs |

Add the medians and an arrival costs about **three milliseconds**. The cell it was
measured in reports **p99 44.5, 50.7, 53.9 ms** in its three warm rounds (and 219 ms in
the round that starts the process) — ten to a hundred times the sum of every stage.

**So in these runs the tail is not in the arrival.** It is in what the probe does
after its request has been forwarded: the 64-byte echo has to come back through the client, the
backend and the carrier before the probe can stop its clock, and the probe's sample is
the whole round trip. The instruments above stop at "the server forwarded the first
byte" — which is why a cell can read tens of milliseconds while every one of them
reads microseconds.

That also reconciles the population sizes that have been nagging this file: the slow
*opens* are rare (13 in four minutes, about one arrival in four hundred, at 5–83 ms),
while the probe's p99 is slow in *every* round, so the two cannot be the same
phenomenon. One is a rare handshake, the other a common return path.

It is a statement about the runs that were measured, not about the carrier in general:
an earlier full-scale run had a server-side `paired_p99` of **112 ms**, i.e. a tail
inside the arrival. The two runs disagree about *where* the tail is because the tail
moves — which is the same lottery this file keeps meeting, seen from the other side.

**Pre-registered next instrument**: split the probe's own round trip at the boundary
this cycle has never timed — from the forwarding of the request to the visitor's echo
being read back. On the server that is `first_*` plus the return; on the client it is
the response direction's own path. The gate is the same as always: the interval's
distribution, then whatever it names, judged on its own A/B.

### The tail is a transport delay, and the timer only marks its boundary (2026-11-05)

The accept queue is not the wait: a stream waits **p50 7 µs, p99 12–14 µs** between
the datagram that names it and the loop that takes it (new instrument, reported as
`a stream waiting for its accept`). So the second of the two intervals this carrier
could not see is also fast.

The first version of this entry read one log line as "a fresh lane pays the
small-flight timer" — `window=13490` is `INITIAL_CWND`, so the lane had carried
nothing. **That inference is wrong**: `window` is `cwnd − flight`, and a lane whose
congestion window has stayed near its initial value reads the same number after a
megabyte. What the whole population says is better, and is the reason this entry was
rewritten rather than extended. Every slow open in one run, in order:

| open | remaining window | retransmissions |
|---|---|---|
| 5.5 / 6.0 / 10.9 ms | 21 584 / 13 490 / 22 933 | **0** |
| 15.2 / 15.8 / 16.7 / 20.6 ms | 13 490 each | **0** |
| 23.1 / 23.4 / 23.4 / 23.6 ms | 13 490 each | **1** |
| 27.6 / 33.4 / 39.7 / 45.6 / 66.4 / 83.4 ms | 13 404–20 144 | 1–3 |

Read down the middle: the opens climb from 5.5 to **20.6 ms with no retransmission at
all**, and only then does a cluster appear at 23+ with exactly one. So the
retransmission is not the cause of the tail — it is the *marker* of the 20 ms
`SMALL_FLIGHT_INITIAL_RTO` boundary, adding a duplicate and three milliseconds to
opens that were already late. **The delay itself is in the handshake's transport, and
it happens before any timer fires.**

That is a smaller claim than the one it replaces and it is the one the numbers
support: between 0.5 and 20 ms an open is slow with nothing lost and nothing refused
(`credit` 16.7 MB, `queued_streams=0`, `retransmits=0`), while every instrument the
carrier owns — dwell, post, accept, lock, park — reads microseconds.

**Pre-registered next experiment**: the remaining candidate is the dispatcher's own
batch. A datagram for a lane the listener has never seen is handled *inside* the
receive callback (registry lock, admission check, `Conn::new`, a `Shared`, a sender
task spawned, two map insertions) before the next `recv` of that batch, so bulk
traffic and a new lane's first packet share one task's critical section. Measure the
batch's processing time with its datagram count; if it reaches tens of milliseconds,
move the new-lane setup off the receive path and re-measure. Gate: the batch time
stays in microseconds and the arrival's p99 on the `cost` cell falls below the 20 ms
line that the timer cluster marks.

### Fewer wake-ups help, and do not touch the first stage (2026-11-05)

A lane carries four or five streams and a receive batch usually delivers bytes for
one, so the pump was waking every parked reader on the lane for each batch. It now
records which streams changed (`Conn::woke`) and wakes one reader per id; the
blanket wake stays for the events that really are connection-wide.

**On a staged cell with six bulk streams** (interleaved, three rounds, `--aa`):

| metric | change |
|---|---|
| `rtt_p99_ms` | **−79.35 %** |
| `rtt_p50_ms` | −51.70 % |
| `rtt_worst_1s_ms` | −75.52 % |
| `rtt_max_ms` | −66.27 % |
| `throughput_gbps` | **+16.79 %** (CI 1.28..2.08, entirely beyond its floor) |

Every latency metric moves the same way and throughput moves with them, which is
what fewer spurious wake-ups should look like — but the verdict is `directional`,
not `claim`, because at three rounds the cell's own A/A scatter is 48 % on p99.

**On the cell that decides the release** it does not fix the first stage. Full
scale, one arm, host quiet (load 0.24 / 0.75):

| stage | n | p50 | p99 | max |
|---|---|---|---|---|
| `clean` (first) | 1 964 | **36.47** | **302.13** | 509.68 |
| `clean#2` (last) | 2 396 | **2.48** | **22.54** | 24.77 |

The last stage is the best it has ever read (22.54 against 23.60 and 23.65 in the
two previous runs), so the change is worth keeping. The first stage is the worst it
has ever read — and that is the finding: across four runs of near-identical code
the first clean stage has now read **25.79, 112.85, 146.03 and 302.13 ms** while the
last has read **22.5–23.7** every time. The *last* stage is a measurement; the
first is a lottery, and every lever tried against it so far (credit reserve, worker
count, scheduler order, wake scope) has left it a lottery.

Two things the four numbers do establish, and they are not the same problem:

- **the warm stage is 10× the control** — 22.5 ms against 2.28 — so even with no
  cold start there is a tail: about one arrival in a hundred pays twenty
  milliseconds inside a handshake whose own median is 1.0–1.45 ms;
- **the first stage adds a further factor of one to thirteen**, and which one is
  decided by something outside the arrival path: the same binary, the same host
  class, four different answers.

The intervals excluded by measurement so far are the send buffers (dwell p50 2–4 µs
client, 9 µs server), the pump's post (p50 11–20 µs, p99 20–35 µs), the server's
handshake processing (under 5 ms in 1 141 of 1 191 slow opens), the wire itself (the
asymmetry above), the credit reserve, the worker count, and the scheduler's order.
**What is left uninstrumented is the datagram's wait between the socket and the
dispatcher, and the spawn-to-first-poll of the per-stream task** — the server's own
clock starts *after* the stream exists, so neither interval appears in any counter
this cycle built.

**A discipline this cycle learned the hard way**, and it now costs a run each time
it is forgotten: a bench record is only comparable with one whose host was as idle.
The run that read 285.99 ms had a five-minute load average of 4.74 against 0.84 in
its reference; the one killed before it started had 5.71 and would have been the
same story. Check `meta.provenance.host.loadavg` before comparing two files, and
before believing that a change moved a first stage.

### The arrival's tail is not in the carrier's send path (2026-11-05)

Every instrument the carrier owns now reads microseconds at full scale, and the
arrival still reads a hundred:

| what | client | server |
|---|---|---|
| a stream's first byte waiting for the wire (dwell) | p50 **2–4 µs**, p99 ~0.5 ms | p50 **9 µs**, p99 0.14 ms, max 12–70 ms |
| the pump's post to the socket | p50 **11 µs**, p99 20 µs, max 0.18 ms | p50 20 µs, p99 **35 µs**, max 0.35 ms |
| a stream's reader parked for bytes | p50 131 µs, p99 **46 ms**, max **2 s** | p50 64 µs, p99 45 ms, max 2 s |
| the per-stream Noise handshake, every open | p50 **1.0–1.45 ms**, p99 **23–107 ms** | *(not kept on this side)* |

Read together with the earlier attribution — `us == hs_us` for every slow open,
share 1.000 — the handshake is still the whole of an arrival's cost, and its p99
(107 ms) is the arrival's p99 (112.85) to within a few percent. What is new is
that **the handshake's time is not being spent in either end's send path**: the
buffers drain in microseconds and the pump's post never exceeds a third of a
millisecond. So the wait is the interval between a handshake record being written
and the peer's reply being *read* — two task crossings per message, and the reader
parked across them (p99 46 ms, and a 2 s tail that is the keepalive interval to the
microsecond, i.e. a reader that missed its wake-up and waited for the pump's own
timer).

The same asymmetry says it is not the wire: of 1 191 slow client opens, only **50**
were slow on the server's own clock (its `selector + handshake + hello`, under 5 ms
in the other 1 141). Both directions would be slow if the path were; one side is.

Which makes the lever *fewer task crossings per arrival*, not a faster stage — and
that is what sealing does (it deletes both handshake messages and the round trip
they wait on), and why the scheduler work is secondary: a class-aware order cannot
help a packet that is already on the wire while its reader sleeps.

**And the second failed experiment is recorded.** The opening reserve, taken off
every bound, was measured interleaved against the build without it on a quiet host
(load 0.32): `clean` read **5.98 / 146.03 ms** with it and **2.35 / 25.79** without,
while `clean#2` was identical to 0.3 %. The comparison is order-confounded — one
round, two arms run one after the other, on a cell this file has measured at 25.79
and 112.85 for the *same* binary — so it is not a claim about the mechanism. It was
enough to revert, because a change with no demonstrated benefit and a large
apparent harm does not get to stay while someone looks for a better measurement.

### The sealed deadlock is a repair loop, and the instrument was lying twice (2026-11-04)

Two instrument defects came out of following the stall, and each of them had been
hiding it:

- **`note_rejected` collapsed every sealing refusal into `unknown`.** The
  label-to-counter map recognised `malformed`, `malformed_decode`,
  `malformed_lane` and `over_window`, and sent *everything else* — including
  `unsealed`, `unsealed_no_root`, `unsealed_no_tag`, `unsealed_no_key` — to the
  **unknown-stream** counter. So a run whose sealing was failing printed either
  `datagrams refused: unknown=N`, which reads as a routing defect, or (when only
  the sealing labels fired) nothing at all, because the print condition counted
  the four statics that cannot receive them. Sealing now has its own counter,
  matched by prefix so a fourth site cannot fall back to "unknown" again.
- **the stall line could not say *why* a stream with unsent bytes was passed
  over.** It now carries `worst_stream_reset` and `worst_stream_repair`: those
  two are the only ways the scheduler skips such a stream without building a
  packet for it.

With them, the sealed failure names itself:

```
mhd stalled: unsent_ms=0 unacked_ms=569 credit_blocked=false
  worst_stream=5 worst_stream_unsent=1136478 worst_stream_retransmits=1130
  worst_stream_next=2119826 worst_stream_written=2119826
  worst_stream_reset=false worst_stream_repair=true
```

`next == written` — that stream has put **every byte on the wire** — and it is
sitting on a **repair that is re-declared lost and re-sent 1 130 times**, with
no write ever refused (`credit_blocked=false`, `unsent_ms=0`). So the lane is not
stuck for want of room or for want of data: it is stuck spending its flight on
one range it will never see acknowledged, and the four streams whose selectors
the server timed out never got a turn behind it.

That is much narrower than "sealing plus stripes", and it is testable without
sockets: the question is *why a range that the peer has already read is declared
lost again*, which lives in the acknowledgement-to-frontier path, not in the
cipher. The next round starts there — and the first thing it needs is the same
line again, which now exists.

## Negative results: do not retry these blind

Every row was built (or measured) and reverted with its numbers. The archive has
the reasoning; this table exists so a future round does not spend it again.

| mechanism | result |
|---|---|
| yamux over an mhd lane | arrival p99 21.5 → 3.77 ms, but `bulk-n` −23.75 % and CPU/bit +29.37 % |
| a Noise record layer above the carrier | arrival p99 −74 % at three record sizes, and the bulk cell failed at all three |
| pre-opened channel stock | 6 400 arrivals; the stock of 4 is consumed by the first 4 |
| more lanes (4/8/16) | flat within the scatter; the tail is not lane queueing |
| larger `INITIAL_CWND` | first stage **doubled** (79.4 → 153.9 ms p99) |
| smaller `MIN_RTO` (200 → 20 ms) | every bulk cell failed |
| `MIN_RTO_SMALL_FLIGHT` / `SMALL_FLIGHT_INITIAL_RTO` at 5 ms | p99 22.6 / 22.1 against twins 22.3 / 20.3 |
| `TOKIO_WORKER_THREADS` 8 or 16 | monotone the wrong way; the lock is contention, not starvation |
| timer armed from the oldest packet (any kind) | `churn-16` p50 2.44 → 16.07 ms |
| timer armed from the oldest **data** packet | `bulk-n` −60 %; the flight outlives the RTO on this host |
| hole recovery by distance | neutral on every cell |
| eager credit advertisement | `churn-16` p99 +1 068 %; credit frames preempt data |
| `describe_hole` (send table for a receive hole) | answers about the wrong direction |
| per-datagram lock batching | +0.35 %, inside a 9.81 % floor |
| receive-region ownership / pinned pieces | RSS 376 MiB, but throughput 2.76 Gbit/s |
| chunk queue without a read cursor | `cpu_s_per_gbit` 46.7 |
| widening the pool for `soak` | 448 slots, same four stage failures |
| `warmup_rounds` to cure the first stage | a round's first stage is cold again; the probes restart per round |

## Environment

- **20 cores, 23 GB, kernel 6.12.** `/etc/machine-id` is absent, so `host_id`
  hashes CPU model + core count only; the hostname changes without moving it.
- **The cgroup is 13 GiB**, and `oom_kill` is a counter to read per run. Build
  with `CARGO_INCREMENTAL=0`, prefer one release build over many debug feature
  permutations, and check `ps -eo rss,comm --sort=-rss` before a long run — a
  `rustc` for another project competes for the same memory.
- **The host is shared.** Its load swung between 1 and 87 in one afternoon, and
  `bulk-n` swings 3 ⇄ 11 Gbit/s and `capacity` 2 ⇄ 8 streams with no tree
  change. Every run records the load average; §10's "do not conclude across
  variance" is the operative rule.
- **`sudo -n` works, and it drops the environment.** The stats switches need
  `sudo -n env MOLEHILL_MHD_STATS=1 just bench …`; a run whose `server.log` has
  no diagnostic lines is a run with the switch off.
- **Do not run two chains or two bench runs at once.** The suites bind fixed
  ports and the bench holds network namespaces; a second run produces
  `ip netns exec` failures that look like product failures.
- **`/tmp` is periodically wiped.** Keep artifacts under `target/evidence/` or
  `~/tmp/`.
- **`timeout N` orphans the run**: it signals `uv run`, not the python child,
  which keeps going and holds the bench lock.
- **`iperf3` is installed at runtime**, so a container restart drops it
  (`command -v iperf3` first). It is version-sensitive in the fields the model
  reads.
- **Peers and the interop binary are cached** (`~/tmp/bench-peers`,
  `~/tmp/interop`), so a normal run needs no network. `just bench peers fetch`
  re-resolves each peer to its latest GitHub release and records the versions in
  the results meta. `just bench doctor` states what this host can and cannot
  measure before a run has to find out.
- **A baseline-less gate is the norm here.** Every stored baseline either has no
  `host_id` or predates the current method, so a fresh sweep is gated on its own
  absolute checks — coverage, the endpoint invariant, the SLO, drift, wedge and
  the ramp — and the gate names what it could not verify instead of assuming it.
- **A `sudo` bench run can leave root-owned files** in `.git/index` and
  `~/.cache/molehill-bench/venv`; both are repaired with `chown -R dsh:dsh` on
  the path named in the error.

## The archive

[HANDOFF-archive.md](HANDOFF-archive.md) holds every closed record, with a
contents list at the top. The section that matters most is
[the v1.0.0 cycle](HANDOFF-archive.md#the-v100-cycle-the-mhd-carrier-moved-2026-11-03):
the carrier swap, the cost investigation, all ~80 rounds of the arrival-tail
search, and the release ritual as it was run.

**No number in the archive may be quoted as a measurement of the current code**,
compared against a newer run, or used to gate anything. The live numbers are in
this page's "What is measured" section, and the method's home is
[docs/benchmarks.md](docs/benchmarks.md).
