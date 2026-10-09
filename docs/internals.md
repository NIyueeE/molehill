# Internals

## Concepts

- **Service**: the entity whose traffic needs forwarding (e.g. an SSH server)
- **Server**: the publicly accessible host running molehill in server mode
- **Client**: the host behind NAT running molehill in client mode; it holds the services to be exposed
- **Visitor**: someone who connects to a service through the server
- **Control session**: an authenticated connection between the server and one client endpoint, carrying the control commands of *every* service the client registers there (protocol v5). Two services share it when they declare the same endpoint and the same transport — a connection speaks one transport, so a service that overrides `transport` gets a session of its own
- **Data channel**: one stream of forwarded traffic between the server and the client — either a dedicated transport connection, or (with `multiplex`) a yamux stream inside the tunnel. It names the service it carries with a four-byte prologue (see "Forwarding")
- **Tunnel** (`multiplex` feature): an extra connection, upgraded to a yamux session, that belongs to a control session and carries many data channels as streams
- **Tunnel pool**: the set of tunnels one client keeps for one key — the whole session, or one service, depending on `[client.data].shared_pool` (see "The tunnel pool")
- **Stripe group**: a set of `K` data channels that carry one visitor connection together (see "Data-channel striping")

## Startup and registration

In client mode, molehill groups its configured services by endpoint: a service's own `remote_addr` (`[client.services.<name>]`) when it declares one, else the client-wide `[client.control].default_remote_addr`. Every group dials **one control session**, authenticates once with `[client].default_token`, and registers its services on it. The server owns no service configuration: each `Register` carries the service's name, type and public endpoint (`remote_bind_addr`), and the server validates it against its policy before exposing anything — `allow_ports` whitelist, the declared data-plane carrier, and port conflicts (which surface as precise rejections because the bind precedes the ack). The user-facing rules and error messages are in [Configuration](configuration.md).

On success the server binds the endpoint and starts serving visitors. On failure it answers `RegisterRejected` for **that registration alone**: the client reports the exact reason once and stops that service, while the session and the services beside it keep running. A registration carries no pool size — the channel pool is the client's own concern (see "Forwarding").

A session reconnects on its own after a retryable failure (a dead connection, a timeout), drops the tunnel pools it had built on the old connection (their tunnels carry that session's nonce, so the new server refuses them as stale) and re-registers everything it carries, each service building the pool it needs. A graceful server shutdown ends the sessions it was serving — service listeners, pools and tunnels together — rather than leaving them bound until the process exits. Two failures are terminal for the session instead, because only a human can fix them: the server is older than the client's protocol version (it closes the connection without answering the hello, or answers in another dialect), or it refused the session token.

## Authentication

A session uses two credentials, which is what lets one connection carry services that are trusted differently.

**Session credential.** The server sends a random 32-byte nonce in its hello. The client answers `sha256(default_token || nonce)`, derived from `[client].default_token`. The server compares it against the digest of its own `[server].default_token` and answers `Ack::SessionOk { heartbeat_interval_secs }`; a wrong token gets the bare one-byte `Ack::AuthFailed` and the connection closes — terminal for that session, not retried, because a retry cannot change either token. The nonce is also the data plane's credential: a data channel or tunnel authenticates by sending it back.

**Service credential.** Every `Register` carries `sha256(service_token || nonce)`, where `service_token` is the service's own `token` when it declares one, else the client's `default_token`. The server owns no per-service token table — it compares the digest against the session key it derived above — so a service whose token differs from the server's default is refused **on its own**: one denied service costs that service, not the session, and the reason names the credential.

### Protocol versions

The protocol version rides in every hello, and this build speaks and serves one
dialect: **v5**. v5 adds one service type (`ServiceType::Transparent`) and one
data-channel command (`DataChannelCmd::StartForwardTransparent`), nothing else:
the selector byte, the session handshake, the service prologue, the striping
frames and the UDP framing are v4's. (0.8–0.9 spoke v3 and 0.10 v4; each
dialect was dropped by the release that followed it.) The rule — a
dialect is defined by a release, its number moves with the tag that introduces
it, and no two tags are compatible — lives in AGENTS.md §5; what it means on the
wire is here: a server that does not serve the client's version closes the
connection instead of answering, and the client turns that into a terminal error
naming the likely cause (a server older than the client) rather than a retry
loop. A *client* older than the server is refused the same way, so upgrading
means upgrading both ends, in either order, with one refused connection telling
you which side is behind.

## Forwarding

When a visitor connects to a registered service's endpoint, the server asks the session for one more channel for that service — `CreateDataChannelFor(service_id)`, which names the service because a session carries several. The client opens a data channel back (a fresh connection, or a yamux stream inside a tunnel), and the server marks it with a `StartForwardTcp`/`StartForwardUdp` command; the pair then copies the visitor's bytes bidirectionally.

**The service prologue.** A data channel opens with the service's four-byte id before anything else: the session nonce identifies the *session*, not a service, so a channel has to say which one it is for. A direct channel writes it right after its hello; a stream of a multiplexed tunnel writes it right after the stream is opened, because a tunnel belongs to the session. Routing is per **stream**: one tunnel may carry streams of several services (which is what a shared pool produces), and each stream's prologue is what decides the queue it lands in. The server reads the prologue before routing, so a stream naming a service that is not registered on that session is dropped on its own, without touching the tunnel or the session.

A registration carries no channel count, so the client owns its channels: a UDP service opens its configured `udp_workers` (default 2) as soon as the registration is accepted, a TCP service opens none, and either opens one more for every `CreateDataChannelFor` — or, for a striped visitor, for every `CreateDataChannelForStripe`, which names the stripe group and the stripe's place in it. New channels are also requested on demand: per visitor for TCP, and whenever a UDP channel dies so the worker set keeps its size (a replacement never grows the set past the configured count; see "The tunnel pool").

**Pairing is per visitor, never serial.** The server's accept loop hands every visitor to its own pairing task: one visitor's wait for a data channel therefore costs that visitor and nothing else, and the accept loop keeps accepting. The pairing wait is a budget that re-requests rather than giving up, and a visitor the client refuses for the whole budget is shed — its socket closed, one failed request — while the service and its other visitors keep going. The number of pairings in flight at once is bounded (`MAX_CONCURRENT_VISITORS`, 128), which is what keeps a wedged service from growing unbounded tasks: each in-flight pairing owns one visitor socket and one data channel, and the listeners' backlog keeps the rest. A visitor whose channel request the client cannot answer — the pool at its placement ceiling — used to hold the accept loop for the whole 25-second budget, so every visitor behind it queued unanswered and the k-th one was shed a full budget after the first; measured, that is a service parked behind one visitor (`tests/pool_test.rs`, `one_unanswerable_visitor_does_not_park_the_service`). A stripe group's gather is the one pairing that stays atomic: its K channels are consumed all-or-none under a lock, so concurrent visitors cannot interleave two gathers' channels.

A data-channel open the client's pool refuses (every tunnel at the stream ceiling, or a growth the server's valve refuses) fails that one visitor, and the condition is reported once per process on the client — INFO the first time, then DEBUG — so an operator can see a pool that is out of capacity instead of a stream of indistinguishable per-connection failures.

For UDP, the server maintains a per-service **session-affinity table**: a single reader task accepts datagrams from the service socket and routes every peer address to one data channel for the entry's lifetime (TTL-evicted after 300 s of inactivity). Routing every peer to a fixed channel — instead of letting all workers race on the socket — is what keeps one peer's packets on one path; the pool shards *distinct peers*, not packets.

### Data-channel striping

With `[server.data]stripe_count = K` (default `1`), the server pairs every visitor connection with `K` data channels instead of one and labels each with a `StartForwardStripedTcp(group, index, K)` command. The pair then forwards the connection over the group (`src/stripe.rs`):

- Each direction numbers its chunks (`[u64 seq][u16 len][payload]` frames, 32 KiB payloads) and spreads them round-robin over the group's channels.
- The receiving side reassembles by sequence number: out-of-order chunks wait in a bounded reorder map, contiguous ones are written to the destination. A frame always travels whole on one channel (a half-written frame cannot move — it would corrupt that channel's framing); a channel that refuses a frame *before* any byte of it is committed is skipped for that frame, so one backpressured channel does not stall the group.
- A channel that ends mid-frame (not at a frame boundary) breaks the group instead of leaving the reassembler waiting for a sequence number that will never arrive.

The gather asks for its channels *before* its first wait, because a
registration opens none itself: the tunnel pool starts cold, so a gather that
waited for channels nobody was told to open would simply time out. What it asks *with* is the
whole of the stripe-group change: instead of K plain `CreateDataChannelFor`
requests (which the client cannot tell from K unrelated visitors, so their
placement is whatever the pool happens to have) it sends one
`CreateDataChannelForStripe(service, group, index, count)` per stripe, so the
client knows which opens belong together and reserves one tunnel per stripe —
growing the pool to the group's own count first, bounded by `max_tunnels`, and
falling back to sharing when it cannot reach that many. That is what makes D24
structural rather than a property of arrival timing. The wait is a budget that
re-requests only the stripes still missing, and a gather the client refuses for
the whole budget is shed like any other visitor, which is the one pairing that
stays atomic: its K channels are consumed all-or-none under a lock, so
concurrent visitors cannot interleave two gathers' channels.

Three things follow from the arithmetic: the visitor's throughput ceiling is the sum of its channels' ceilings (a single stream is no longer capped by one tunnel), its in-flight window is the sum of the channels' windows, and each channel's framing work is driven by its own task. The cost is the reorder buffering (bounded by the engine's per-stream window plus one reorder queue per direction) and one data-channel wire addition — the striped command rides *after* the unchanged `StartForward*` commands, and channels that do not carry it keep the unstriped path's bytes, so the yamux wire format is untouched. Striping applies to TCP services; UDP keeps its one-channel-per-peer shape, where session affinity is the stronger constraint.

### The tunnel pool

A pool is the client's unit of connection management. Its **ownership** is one setting:

| `[client.data].shared_pool` | Pool key | Who uses it |
|---|---|---|
| `false` (default) | `service:<id>/<carrier>:<data-endpoint>` | one service alone — today's shape |
| `true` | `session/<carrier>:<data-endpoint>` | every service of that session on that carrier and data endpoint |

Both are the same code path; only the key differs, and the session owns the pools (a shared pool must outlive any single service). A service joins the pool its key names, creating it if this is the first service of that key to become active, and dropping a service never tears the pool down. The carrier and the data endpoint are part of the key because two carriers cannot share a physical connection and two endpoints cannot share a tunnel.

**Placement** is least-loaded-first: each open takes the tunnel with the fewest established streams plus reserved-but-unfinished opens, and the round-robin cursor only breaks ties. A stripe group's channels arrive as back-to-back requests that *name the group*: the client grows the pool to the group's count first (bounded by `max_tunnels`) and then reserves, for each stripe, a tunnel the group does not already hold — a preference with a floor, so a pool that cannot spread still forwards. A weighted score was deliberately not used: placement is a scheduling decision inside one client, and the S1 telemetry below is what would justify a smarter rule.

**Growth and shrink** are the client's own decisions; the server is not consulted. The pool grows by one tunnel when

- it is cold (no tunnel at all) and a service asks for a stream — this one is synchronous, so a cold pool answers its first open instead of failing;
- any one tunnel reaches 12 % of the stream cap — **7 concurrent streams of 64** — or the pool's total usage passes the same fraction of `size × streams-per-tunnel`. The number is about how much one shared TCP tunnel should carry, not about how close it is to the engine's cap: past roughly seven streams an interactive or control stream starts queueing behind bulk traffic, which is the head-of-line blocking the pool exists to avoid (a 100 ms path measured 20-stream bulk leaving `iperf3`'s control channel to time out; see HANDOFF.md). `[client.data.tcp|kcp].max_tunnels` bounds the result;
- an open has been waiting longer than 100 ms;
- the UDP-derived floor is above the current size — the floor is `max over active UDP services of max(udp_workers, ceil(udp_workers / streams-per-tunnel))`, capped by `max_tunnels`, and it is what keeps a UDP service's configured workers from being stacked onto fewer tunnels than they need (it survives a tunnel's death, because it describes the service, not the pool).

The two load rules are read in **both** places: on the 50 ms maintenance tick, and in the open path itself. The in-path read is what makes a burst spread *while it is placed*: a K-open burst (a 20-stream bulk test opens its streams back to back) finishes long before the first tick would see it, so without the in-path growth every stream of the burst lands on the same tunnel and queues that tunnel's interactive and control streams behind the bulk — the tick can only fix the *next* burst. An open whose chosen tunnel is already at the threshold therefore grows first and places second, and every guard keeps that a no-op when growing is wrong: a growth in flight, a refused growth still holding the pool back (D14), the pool at its own `max_tunnels`, or a cold pool (which the first bullet grows already). The dial the in-path rule may start is the same dial the cold path pays.

**Reaching the engine's stream cap costs one stream, not the tunnel.** The mux engine carries at most `DEFAULT_MUX_MAX_STREAMS` (64) streams; a 65th is answered with a **reset of that stream**, which fails the open that asked for it and leaves every other stream on the connection running. (The first v0.10.0 build answered it with a session-terminating goaway instead, which took the whole tunnel and every visitor on it down — that is what a shaped sweep measured; see HANDOFF.md.) Placement still refuses a tunnel at `TUNNEL_STREAM_CEILING` (56 of the 64, counting reserved-but-unfinished opens as well as established ones), because refusing a stream is worse than placing it elsewhere — but the ceiling is now a preference, not the only thing standing between a burst and a dead tunnel. It sits well above the growth point (7), so a pool that *can* grow always grows long before placement refuses; when every tunnel is at the ceiling and the pool cannot grow — its own `max_tunnels`, or the server's valve — an open waits briefly for a stream to retire and is then refused with a typed error.

**A forward that moves nothing is reaped.** How the ceiling gets tested is a stalled connection: under loss a visitor's socket stops draining, so its copy task blocks on the write, stops polling its reader, and the peer's flow-control window closes behind it — and nothing in TCP ends that, because the application waiting for a reply has no timeout of its own. Both copy sites therefore run `copy_bidirectional_with_idle`, which closes a connection that has moved no bytes in *either* direction for `FORWARD_IDLE_TIMEOUT` (5 minutes) and reports it. Activity, not direction or age, is what the deadline measures: a slow trickle is never reaped however long it lives.

Growth never exceeds `[client.data.tcp|kcp].max_tunnels`, and at most one tunnel is added per growth — whether the growth was asked for by a maintenance tick, or in the open path by an arriving burst. A growth that *fails* — the server's `[server.data].max_tunnels_per_client` valve refusing the tunnel, or a dial that could not be established — stops growth for a cooldown instead of being retried on the next 50 ms tick, which is what keeps a capped client from dialing the server's accept path twenty times a second. A tunnel dying, or the pool shrinking, releases that hold: a retry is meaningful again exactly then. The pool shrinks by one tunnel when the **whole** pool has no streams and no pending opens, no peer is pinned to the tunnel being considered, it has been idle for `[client.data].idle_timeout` seconds, and its size is still above the UDP floor. Hysteresis is what keeps that from flapping: a minimum warm period after a growth and a cooldown after a shrink. The constants are internal until the S1 observation has measured them; they live in one place (`src/transport/pool.rs`) and are the *policy* only — the pool's runtime is in `src/transport/multiplex.rs`.

Three UDP invariants ride along (they are the reason shrink is this conservative):

- **A peer pins its tunnel.** The server's affinity table points a peer at one data channel; the client's own route table points the same peer at the channel its traffic arrives on, which is a stream of one tunnel. A tunnel with pinned peers is never shrunk (`pinned_peers`), so a live UDP session is not re-sharded onto another tunnel — which would change the local source port the service sees. A tunnel that dies anyway ends those peers' sessions, UDP semantics.
- **A new source never creates a channel.** The worker set of a UDP service is exactly the channel count the client opened for it; a datagram from an address with no affinity entry is assigned to a live worker (or dropped when none is ready), never answered by dialing a new channel.
- **A channel with live routes is not idle.** Shrink reads streams and pins, so a UDP channel carrying peers keeps its tunnel.

The **S1 observation** is opt-in and aggregated, so a normal run stays silent and a measured run stays readable:

- `MOLEHILL_POOL_STATS=1` — one INFO line per live pool per second: the pool's key, carrier, size, cap, UDP floor, live streams, pinned peers, the per-tunnel `streams/pending/pinned` triple, and the timeline of size changes with their reason.
- `MOLEHILL_PLACEMENT_STATS=1` — one INFO line per second per process aggregating that interval's placements: how many, how many fell back, the candidate and chosen load sums, `mean_spread` (the average gap between the best and the worst candidate — what a smarter rule could have won, and the number the S2 decision reads), and the open latency's mean and maximum. INFO like every other switch in the family: the switch is the consent, so no extra log level is needed.

`MOLEHILL_MUX_STATS=1` (the framing counters) is unchanged and independent.

### Multiplexing

With the `multiplex` feature (part of the default feature set) and `mode = "multiplex"` (the default), a registered service runs over an elastic tunnel pool that starts **cold**: the first open dials a connection (each announced with a distinct hello so the server upgrades it to a yamux session), and later growth is the client's own decision up to `max_tunnels`. Data-channel opens take the least-loaded tunnel, and a dead tunnel is skipped transparently. The pool's size is elastic and its ownership depends on `[client.data].shared_pool`; both are described in "The tunnel pool" below. The tunnels dial the service's data endpoint (`[client.services.<name>].remote_addr` when set, else `[client.data].default_data_addr`, else the control endpoint) and are accepted by the server's data listener — the control listener itself when the addresses match, otherwise `[server.data].bind_addr`. From then on:

- `CreateDataChannelFor` no longer dials a fresh TCP(+Noise) connection; the client simply opens a new stream on the tunnel, and the stream's prologue names the service it carries.
- The server feeds accepted streams into the same pool/pairing logic used for plain channels.
- Per-stream framing is identical to the plain path (the service prologue, then the `StartForward*` command), which keeps both modes testable against each other.
- The framing engine is maintained in-repo (`src/mux/`, vendored from rust-yamux 0.14 — wire-identical with the yamux specification; the vendoring rationale and its per-lever outcomes are recorded in HANDOFF.md "What landed" / "Optimization route"). It is tokio-native (tokio IO traits, no compatibility shim on the data path) and auto-tunes each stream's receive window towards the bandwidth-delay product, avoiding the fixed-small-window throttling known from stock yamux deployments.
- yamux opens outbound streams lazily (the SYN flag rides on the first outbound frame). Because this protocol is server-speaks-first, the client driver kicks each fresh stream with a zero-length write so a read-only pooled stream is announced immediately.
- A stream's two users park on the connection's per-stream command channel independently, so each has its own waker slot: a *reader* waiting to queue a window update and a *writer* waiting for send credit would otherwise share one slot, and the later park would erase the earlier one's waker — a writer that then never wakes until an unrelated resize happens to notify, which stalls every visitor on that tunnel. The reader's park lives in its own slot (`Shared::reader_park`) and the connection wakes both when a command leaves the channel.

`mode = "direct"` restores the one-connection-per-channel behavior: every data channel is its own transport connection, so nothing is multiplexed and each visitor connection pays the full connection setup (TCP connect plus, with `noise`, the Noise handshake). That is a design trade-off, not a performance claim — the measured comparison lives in [Benchmarks](benchmarks.md#what-each-configuration-choice-costs-per-decision-measurements), and the choice between the two is the decision tree in [Configuration](configuration.md).

### Transparent (L3) services

A transparent service reverses the ownership of the public endpoint. Its
registration declares `ServiceType::Transparent`, and its `bind_addr` is a
public `ip:port` the **client claims** rather than a listener the server binds:
`bind_service_endpoint` returns `BoundEndpoint::Transparent` (nothing to bind),
and the address's uniqueness is kept by a server-wide claim — `Registered::claim`,
a `Claim` value held for the lifetime of the registration — so a second client
claiming the same endpoint is rejected with a precise reason instead of silently
stealing the first one's visitors.

The data path is **one channel per claimed endpoint**:

- The client opens **one** channel the moment its registration is accepted (a
  TCP service opens none and waits to be asked; a transparent service's packets
  all ride this one). It opens with the same four-byte service prologue as any
  other data channel, so the server knows which service — and therefore which
  endpoint — it carries before the first packet.
- The server answers it with `DataChannelCmd::StartForwardTransparent` (tag 3,
  a unit variant like the other fixed-size data commands), and from then on the
  channel carries whole IP packets, framed `[u16 length][packet]` in both
  directions by `IpTraffic` (`src/protocol.rs`). The packet travels verbatim —
  there is no address tag, because the addresses are inside it — and a
  zero-length frame is a protocol error rather than an empty packet.
- When the channel ends the server asks for a replacement (the same
  `DataChannelRequest` path a UDP worker's replacement takes) and keeps the
  claim while it waits. From the second channel on, it waits 250 ms
  (`TRANSPARENT_REPLACE_BACKOFF`) before sending the start command, so a peer
  that cannot serve it is not polled in a tight loop.

**Which end of a packet the claim is.** Both ends run the same hub
(`src/transparent/hub.rs`): one reader per TUN device, a bounded queue per
endpoint, and an endpoint table that decides which packet belongs to which
service. They differ in the end they look at, which is the heart of the design:

- the **server** routes by **destination**: its host routes the claimed address
  into its device, so the packet the kernel hands it is one whose *destination*
  the claimed endpoint names (`Direction::Destination`);
- the **client** recognises its return traffic by **source**: its host *is* the
  claimed address, so what it reads from its device is what its own kernel
  emitted *from* that address (`Direction::Source`).

A packet that carries a port matches the exact `(ip, port)` entry. A packet with
no port to route by — ICMP, and the fragments after the first, which carry no
transport header — matches on the address alone, and only when exactly one
service claims that address: with two services on one address there is nothing
to choose by, so it is dropped rather than guessed. A packet the device produced
that belongs to nobody is counted (`unclaimed`) and dropped; nothing is logged
per packet.

**The claim is re-checked on the way in, in both directions.** What comes off a
channel is parsed and matched against the endpoint that channel carries before
it is injected into the local kernel, so a compromised or buggy peer cannot
steer traffic into an arbitrary local address. On the client that check doubles
as an allow-list, because the client is the end that owns local addresses.

**What the daemon does not do.** It never creates, addresses or routes the
device — `src/transparent/check.rs` is the whole of its network knowledge —
which is what keeps the crate free of netlink and of shelling out to `ip`. What
it does instead is verify the parts it depends on and refuse with the exact
command to run: the device must exist on both ends; on the client, every claimed
address must be one the host carries and `rp_filter` (the device's and `all`)
must read 0; on the server, a registration is rejected if its device is missing.
The recipes are in [Deployment](deployment.md#transparent-services).

**Limits worth stating.** IPv4 only: a packet whose version is not 4 is dropped
and counted (`not_ipv4`), and there is no IPv6 path. One channel carries every
flow of one claimed endpoint, so a retransmit for one flow can delay another
flow sharing the channel — per-flow channels are not in this version. And the
network stays the operator's: nothing here installs a route, a rule or a
netfilter rule. `MOLEHILL_L3_STATS=1` prints the data path's cumulative
counters (`forwarded`, `dropped(not_ipv4, malformed, unclaimed, no_channel)`,
`channel_errors`) once a second per data path, which is how a run is observed
without logging per packet.

## UDP

The visitor-facing socket has **one reader**: a single task takes one datagram per
`recv_from` and the affinity table picks the channel, "so a single reader also
means one slow worker can never stall other peers". Its capacity is therefore a
property of the pool, not of the worker set — measured, `udp_workers` at 1, 2 and
4 carried 1.14, 1.00 and 0.98 Gbit/s of 1400-byte datagrams, with 16 or 64
visitors alike, and the datagrams beyond that are dropped rather than queued
without bound (`MOLEHILL_UDP_STATS` counts them; the measurement is recorded in
HANDOFF.md, "D27's evidence, measured"). `transport::udp_batch`'s `recvmmsg`
batching — used by KCP's socket loop — is the lever if that ceiling ever needs to
move; this reader takes one datagram per syscall.

UDP services are forwarded over the same data channels, framed with a small header (source address + length). On the server side, each peer is pinned to one data channel by the session-affinity table above. On the client side, a per-service hub maps every peer address to exactly one local forwarder socket for the peer's whole session — the `(ip, port)` tuple the local service sees stays stable across channel re-sharding and channel loss — and pins the peer's outbound traffic to the channel its inbound traffic arrives on, falling back to any live channel when that one died. Idle forwarders are cleaned up after `udp_idle_timeout` seconds (default 60); re-binding after that changes the local source port, which stateful protocols notice as a new session. Datagrams larger than the service's `udp_buffer_size` are dropped in-stream while the channel stays usable. All queues enqueue with `try_send` and drop on overflow: UDP semantics, and a single slow peer can never stall others sharing the channel.

### UDP drop counters (`MOLEHILL_UDP_STATS`)

The visitor-datagram reader never blocks: a full worker queue drops the
datagram (what UDP peers already tolerate) rather than head-of-line blocking
every other visitor, and the reader separates "no data channel is ready yet" —
the registration/reconnect window — from queue pressure. Both drops were
previously visible only as `debug!` lines, so "is the queue depth right?" could
not be answered with evidence.

With `MOLEHILL_UDP_STATS=1` the server logs one
`udp-stats: affinity table and per-worker pinned peers` line a second per live
pool, carrying the service endpoint, the affinity table's live size
(`affinity`), its cumulative TTL evictions (`evictions`), the live worker count
and each worker's `pinned` peers (the peers whose affinity entry points at it)
as `worker:peers` pairs, plus the cumulative `queue_full` and `no_worker` drop
counters separately; being cumulative, per-second rates come from consecutive
lines. The pinned counts live on this line rather than a line of their own:
they *are* the affinity table, cut by the channel each peer is pinned to. The switch is independent of
`MOLEHILL_KCP_STATS` because the default carrier is TCP — a run can exercise
the UDP path with no KCP session in existence. The runner records whichever
`MOLEHILL_*` switches a run inherited in its results meta (`instrumentation`),
so an instrumented run is never mistaken for a clean one.

### KCP datagram size follows the path MTU

A KCP datagram is one UDP packet, and UDP does not negotiate a path MTU the way
TCP does: Linux's default `IP_MTU_DISCOVER` for UDP fragments an oversized
datagram instead of reporting an error, so a 1400-byte KCP datagram on a
1280-byte path becomes two fragments and **one lost fragment costs the whole
datagram** — a 1 % fragment loss becomes ~2 % datagram loss, which at the
carrier's ARQ cost is the difference between working and not (measured on
`loss1_mtu1280`: the KCP arm goes from 0.37 Gbit/s to zero while the TCP arm is
unaffected, because the kernel does this arithmetic for TCP).

Each session therefore reads the kernel's path MTU (`getsockopt(IP_MTU)` on a
throwaway socket connected to the peer, or `getsockopt(IPV6_MTU)` for an IPv6
one — an option `nix` does not name, so the crate declares `Ipv6Mtu` itself with
that crate's `sockopt_impl!` macro and no `unsafe`) and shrinks its datagram
size to fit, before the pump drains any application data and again once a second,
because a session outlives the path it started on. The size is **shrink-only**: a
later probe reporting a larger path is ignored, so a route change cannot
oscillate the segment size, and a session that needs a bigger datagram starts a
new session.

Two limits worth knowing: the probe answers only on Linux, the one platform
where the dependency it needs is declared — elsewhere a session keeps the
previous behaviour and relies on kernel fragmentation, which is also what an
oversized datagram does on any path whose MTU cannot be read; and the size is
never *grown*, so `mtu` in the config's sense does not exist: the engine's
1400-byte default is the ceiling.

## Heartbeat

The server **declares** its cadence: `Ack::SessionOk` carries `[server.control].heartbeat_interval` seconds, and it sends one `HeartBeat` command per session on that cadence (`0` means it sends none).

The client **derives** its timeout from that number — `max(10 s, 2 × interval + 5 s)`, two missed beats plus slack — and there is one timer per session, not per service. A configured `[client.control].default_heartbeat_timeout` is an explicit constraint: a value at or above the derived floor is honored, a value below it is refused with both numbers, because it would declare a healthy server dead and reconnect on a loop. `0` disables the check; a session with neither a declared cadence nor a constraint has no timeout.

## Hot reload

When the config file changes, the watcher compares the old and new configs: general changes (transport, addresses, tokens, data-plane settings) trigger a full restart of the instance. Client service-level changes are applied without restarting the session:

- an **added** service (or one whose service block changed) is registered on the session that owns its endpoint — the session is created if this is the first service to dial it, and a re-registration on the same session keeps the service's id so the server takes the previous endpoint over;
- a **removed** service is deregistered (`Deregister(service_id)`): the server drops that service's listener and releases its public port, while the session and its other services are untouched;
- a service whose **`remote_addr` changed** is deregistered from the session it left and registered on the session that owns the new endpoint.

A service the server reported dropped (`ServiceDropped(service_id)` — its listener could not be served any more) is re-registered on the same session, which rebuilds its channel pool.
