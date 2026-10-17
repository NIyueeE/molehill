# HANDOFF: Working State & Future Work

> **State as of 2026-10-17.** **v0.11.0 is being released from `main`; the
> human authorized the agent to merge and tag it.** The transparent (L3)
> client, the mandatory-Noise control channel, the bench model as the one
> measurement standard, and the two-lane default for an L3 claim are on `main`
> (PR #5, merged as `a9d66b4`). `version = "0.11.0"` is committed, the
> CHANGELOG section is dated `2026-10-17`, and the release sweep is measured at
> `d0f22b9` (`just bench-gate` PASS — see "RESOLVED: the release sweep is
> measured" below); the record, the chart set and both READMEs' numbers are
> the release's own. The release matrix failed three times — a musl compile
> error and two timing flakes (windows, aarch64-musl) — each fix invalidated
> the sweep, and the tag was deleted and is re-pushed on the release commit.
>
> What is left is the repository-settings items and the open threads below.
>
> **This file owns the working state**: what is open, what was decided and why,
> and the release checklist. Per AGENTS.md §3 it is a contributor page —
> user-facing facts belong in the docs pages, and anything released belongs in
> CHANGELOG.md. The closed records — incidents, superseded sweeps, measurement
> records, retracted diagnoses, and this cycle's whole investigation — live in
> [HANDOFF-archive.md](HANDOFF-archive.md), which this page indexes.
>
> Shipped work: [CHANGELOG.md](CHANGELOG.md). Design:
> [docs/internals.md](docs/internals.md). Method and how to read the numbers:
> [docs/benchmarks.md](docs/benchmarks.md).


## Where this stands

- **v0.11.0 is on `main` and the release is in its tag step.** PR #5 merged as
  `a9d66b4`, and the human authorized the agent to tag. The release matrix has
  failed three times (a musl compile error and two timing flakes, on windows
  and aarch64-musl), none in the product code (the list is in "RESOLVED: the
  release sweep is measured" below); every src fix invalidated the previous
  sweep, because `githooks/pre-tag` compares the record's revision against
  `src/`. The published record is the sweep at
  `d0f22b9` — 90 samples, `just bench-gate` PASS — and the tag is re-pushed on
  the release commit that carries it (docs and assets after `d0f22b9` are
  allowed by that check).

- **The cycle being released**: transparent (L3) services, wire **v5**, based on
  `main` at `cf01091`. Nothing of it is in `v0.10.0` (verified: the tag's tree
  has no transparent code at all).

- **The first `bench`-model release has no baseline.** `benches/records/` holds
  only the two *soak* files, and `analysis.comparability` refuses any file
  whose `meta.model != "bench"` — so v0.11.0 is gated on the absolute checks
  (coverage, the endpoint invariant, the SLO, drift, wedge, ramp) and becomes
  the baseline for v0.12. Worth remembering when the next release's gate is
  read.

- **The branch was squashed four times: 187 commits to 13 (2026-10-14), 36 to
  29 (2026-10-15, after the wedge cycle), then 39 to 35 (2026-10-16, before the
  release sweep), then 44 to 42 (2026-10-16, before the re-run sweep — the
  offload cycle grew nine commits; two pairs folded: the musl fix with the
  deployment doc that stated its consequence, and the offload fix with its own
  refactor).** Each commit is one topic, and each was a state that passed
  the gate chain when first committed; every time the tree came out
  byte-identical to the tip it replaced (`git diff` against the backup, empty).
  Backups: `backup/l3-pre-squash-2026-10-14`, `backup/l3-pre-squash2-2026-10-15`,
  `backup/pre-squash3-2026-10-15`, `backup/pre-squash4-2026-10-16` and
  `backup/pre-squash5-2026-10-16`.
  - **The third squash is the smallest, and that is the honest shape of the
    history rather than a failure to compress.** It folded three commits that
    were one investigation's instruments into one, the shaper's instrument and
    its queue statement into one, and one docs half-commit into the code commit
    it belonged to. What it did *not* fold is the eight `docs(handoff)` record
    entries: they are interleaved with code commits and each edits the same
    file, so folding them means either reordering (which breaks the later
    patches and resolves as conflicts) or dropping the chain's diffs — and the
    records themselves are append-only history by this repository's own rule
    (HANDOFF-archive.md). Their *content* is in the archive now regardless. A squash changes every SHA, which is why the
  sweep is re-measured rather than carried over: `githooks/pre-tag` checks the
  results file's recorded revision against the commit being tagged, and an
  artifact citing a revision nobody can check out is not provenance.
  - The second squash folded only what was genuinely one topic or genuinely
    superseded: the three fixups that followed on the shaper's instrument, the
    tail-dropping limits, the re-measured release artifacts, and the four
    wrong-turn HANDOFF records — which became one commit whose subject does not
    assert the conclusion they had reached. `433a60e`'s subject claimed the
    wedge *was* bufferbloat; it is reworded to claim only what survived, which
    is that the loss is a queue and the product's own is clear. The bufferbloat
    reading turned out to be right; the attribution is what it could not make.
  - The first squash's re-run was not merely a re-stamp, and the reason is
    worth recording: the first sweep's method record carried
    `engine_hash a08ba5c5cfd439dc`, and the `analysis.py` coverage fix landed
    *after* it, so the new run's was `1771ce182f09d566`. Same numbers' worth of
    code, different engine — which is exactly what the engine hash exists to
    make visible, and exactly why a file measured under the old one could not
    have gated that commit.
  - That re-run's agreement with the one before it is the reading the README
    published: molehill's clean bulk 9.209 → 8.495, frp 6.106 → 6.040, rathole
    28.986 → 25.026, nps 0.135 → 0.134.
- **RESOLVED: the release gate failed on one axis, and the failure was the
  harness, not the tool.** `l3` used to go silent 6 times on the stage schedule
  where L4 never did. The cycle that chased it proposed seven mechanisms and
  refuted every one of them by measurement before the eighth reading held — the
  whole sequence, wrong turns included, is in
  [HANDOFF-archive.md](HANDOFF-archive.md), "The L3 cycle's investigation
  records". Two harness defects were behind it, both fixed:
  - **The shaped leg's netem queue was left on its implicit 1 000-packet
    default**, which caps a 100 ms leg at 10 000 packets/s against this arm's
    37 600, so the shaper shed 73 % of the return stream while every device
    counter read zero (`/proc/net/dev` cannot see a qdisc's drops). Every shaped
    class states its queue now, and each stage's qdisc drops and standing
    backlog travel in its evidence beside the devices'.
  - **The wedge axis measured gaps between samples**, and on a serial probe a
    gap is the next attempt's own duration; with the threshold at exactly the
    probe's attempt budget, one unanswered attempt was a "silence" by
    construction — two 5.1–5.5 s *successful* answers read as L4 going silent.
    It measures runs of unanswered attempts now, and the attempt budget bounds
    the whole attempt (per-socket-operation it could be spent three times).
  **The gate passes on the sweep that ran at `bfb1ef4`**: 90 measured cells, the
  endpoint invariant, the SLO on the clean stages, drift, the wedge axis with
  **every arm at zero**, and the ramp. The one L3-specific penalty that survives
  is `rtt100`'s p99 (1 426 against 402 ms), and its cost is in the record rather
  than argued: the same stage shows the shaped leg shedding **259 149 packets for
  L3 and none for L4**, because L3 offers ~5× the return packets per second for
  less than half the data (12 KB per acknowledgement against L4's 65, since the
  client's kernel receives the bulk through its TUN in 1 400-byte units where
  L4's server GRO-coalesces it). The lever for that is GSO/TSO on the TUN — a
  feature, and a wire question rather than a flag.
- **An L3 claim's lane budget is a live tuning surface, and its default is two.**
  The throughput ceiling is a serialized per-packet path, so a claim's throughput
  follows its lanes; four lanes buys 121 % of bulk throughput and costs the
  interactive stream its SLO headroom, so the default is the largest value that
  keeps both. The measurement, the cost in connections and the upgrade path are
  owned by [`DEFAULT_CLAIM_LANES`](src/common/constants.rs) and the configuration
  page; the full record — including the first version of that constant, which
  chose four from the bulk-only table — is in the archive.
- **Lending is the one mechanism this leaves unmeasured.** `lend` is on by
  default and moves an *idle* lane to the claim with more flows than lanes, and
  the new default now gives it slack to move. Its own benefit is still
  unestablished: the 7-second `bulk-n` cell cannot show it (lending needs ticks
  and confirmations, and each `dev` round restarts the processes), and the one
  run that did show it converging was voided by the shaper bug above. A
  long-hold scenario is what would settle it; the arms already exist
  (`l3-budget10-lending` against `l3-budget10-nolend`).

- **The v0.11.0 pre-release checks that are not in the chain, all green**: the
  interop matrix (`just interop`, 3 cases — both directions refuse the other's
  dialect and keep serving a peer of their own version, against the released
  v0.10.0 binary) and the L3 acceptance harness (`just l3-accept`, 15
  assertions including the negative policy half). Both were broken by the
  mandatory-Noise change and both were repaired with it; neither would have
  been caught by `just check`.

- **The release audit found a blocker that would have failed the tag, and it is
  fixed** (`2d4cc0f`). The transparent code did not compile for musl: the TUN
  attach passed `TUNSETIFF` typed for glibc's `ioctl` signature, and
  `release.yml` builds `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`
  with `transparent` — the two artifacts the GHCR image is built from — so the
  tag would have failed at the build step. The CI job whose comment claims it
  builds "exactly this target and feature set" had drifted to five sixths of it
  when `transparent` joined `release.yml` alone, so **no musl build had ever
  compiled the L3 code the release ships**; `just container` had the same gap.
  The cast is an identity cast on glibc — the release binary's `.text` is
  byte-identical with and without it — and `githooks/check-docs` now fails any of
  the three copies of that list that is not `Cargo.toml`'s default set.

- **The L3-versus-L4 throughput gap is the architecture, not a defect — and
  then it was fixed.** Four TUN syscalls per data packet — the visitor's data
  read and written, plus its acknowledgement read and written, because the
  *client's* kernel terminates the connection — put ~4.6 of the daemons' 7.7
  cores in system time. It is neither a lock nor one saturated stage: over 22
  threads, run ≈ 600 % against wait ≈ 1 %. Three candidate fixes were built or
  measured and refused (a serialized reader, the routing-table mutex, and
  multi-queue TUN readers, which the A/B showed buy nothing and which would
  have stranded every pre-v0.11.0 binary, since a `multi_queue` device refuses
  a non-multi-queue attach).
  **The fourth fix is the one that worked** (this session): the kernel was
  segmenting every packet at the TUN boundary because the device never
  declared the offloads its reader can take. `IFF_VNET_HDR` +
  `TUNSETOFFLOAD(CSUM|TSO4|TSO6|ECN|USO4|USO6)` changes the unit of the whole
  data path from one 1 400-byte packet to one ~64 KiB super-frame — measured
  on a routed forward path, 3.85 GB of bulk TCP: 576 071 reads of 1 500 bytes
  became 59 160 reads averaging 65 212 bytes. The frame travels the lane
  whole (vnet header included, `NEEDS_CSUM` intact), the routing decision is
  one lookup per super-frame, and the wire framing's length prefix moved from
  `u16` to `u32` to carry a whole super-frame. Attach-time compatibility was
  verified on this host: a VNET_HDR attach to a pre-created plain device
  succeeds (unlike the `multi_queue` trap), so no operator recipe changes.
  What did **not** work: `IFF_VNET_HDR` alone (the kernel still segments —
  the declaration is the lever), and the UDP-GSO read side segments even with
  the declaration on this kernel (UDP offload is TX-only here); TCP TSO is
  the win. The probe is in-repo:
  [`benches/scripts/l3/tcp_tso_census.py`](benches/scripts/l3/tcp_tso_census.py)
  (method:
  two netns, one routed TUN hop, a userspace shuttle that relays frames
  whole; the UDP control (`gso_probe5.py`, kept out of the repo — it needs a
  UDP_SEGMENT sender and only rules out a UDP-shaped win)).

- **RESOLVED: the release sweep is measured.** The published run landed on
  2026-10-17 at `d0f22b9` (the release commit, tree clean, binary
  `871521c51fc4da88`): 90 samples over the five arms (molehill, l3, frp,
  rathole, nps), timeline + capacity, 102 minutes, zero failed samples.
  `just bench-gate` PASSES — the endpoint invariant, the clean-stage SLOs,
  zero wedges on every arm, every arm sustaining the ramp to its own ceiling
  (nps reached 8 this run; its path-limited ceiling has read 1/2/5/8 across
  runs, its own recorded behavior). Four earlier attempts each paid for a
  lesson now recorded here: the first (at `26ebb13`) was green but its tag's
  release build failed on aarch64-musl — `c_char`'s signedness is a target
  property the x86_64-only CI musl job never sees, fixed in `e4afa24`; the
  second lost one cell to a probe timeout while this host was busy with the
  cross builds for that very fix — a self-inflicted measurement contamination
  the gate's coverage rule correctly refused; the third was clean but its
  tag's release run tripped the windows flake in the multiplex pool test,
  fixed in `6222e3c` (the repair loop owns the CI deadline); the fourth tag
  (`9ad3362`) tripped a second aarch64-musl timing flake — the handover lease
  test raced the pool's own 50 ms maintenance tick and had only been winning
  that race on a fast runner, fixed in `7059440` (both tunnels carry a lease,
  and the wait covers the pool size and the handover state). The pre-offload
  record (`1c2dcd9`) reads L3 clean bulk 2.811 → **3.881 Gbit/s** and clean
  interactive p99 2.3 → **0.9 ms**; L4 9.10 → 8.90. **The L3 clean cell is the
  release's least stable number**: the two same-path sweeps before this one
  read 5.950 and 6.014 Gbit/s, the run's own replicate agrees to 0.7 %, and
  the arm's `offered_gbps` tracks the carried number — the staged cell is
  sender/path-state sensitive, not a tool ceiling. Both READMEs and the
  benchmarks postscript state the spread and lean on the paired smoke A/B
  (2.0 → 3.4) for the offload's size. The charts and both READMEs carry these
  numbers, and the L3-vs-L4 reading on them is "the two architectures split
  the workloads" rather than "one trades throughput for latency".

- **The repository state**: `version = "0.11.0"`, `CHANGELOG.md` carries
  `## [0.11.0] - 2026-10-17` with `[Unreleased]` empty; `main` is at the
  release commit and the tag is re-pushed there. The previous tag pointed at
  `9ad3362`, whose release run failed; it was deleted, not moved.
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
| L12 | `tunnels` is one quantity in both models — **how many carrier connections this client holds for that carrier**: L4 spends them on a pool that carries unbounded per-visitor streams, L3 hands them to claims as lanes |
| L13 | The data plane's shape is **derived from the service type**, not configured: a forwarding service always multiplexes, a transparent claim never does. `mode` is deleted, and a config that still carries it is refused by name |
| L14 | A claim's lane count is `max(1, floor(tunnels / claims on that carrier))`, and `tunnels` unset means **`DEFAULT_CLAIM_LANES` per claim** (two — the largest value that keeps both the bulk ceiling and the interactive SLO, measured); a budget below the claim count is refused with the number to write |
| L15 | A lane **is** a connection, so reclamation moves **idle lanes only** — at most one per tick, never below one per claim: taking a busy lane would drop that claim's packets while its replacement was dialled anyway |
| L16 | Flow placement is a **per-flow table**, not a hash modulus: assignment happens once, into a *live* slot; established flows never move; entries expire after silence in either direction, so a lane change cannot reorder a flow |
| L17 | `[server.data].max_tunnels_per_client` counts a client's **carrier connections**: a transparent claim's direct lane registers against it, a per-visitor channel (the no-`multiplex` build's shape) does not |
| L18 | The mux×1-versus-direct gap is attributed with the model before it is optimised, and each optimisation lands only if it clears that run's own noise floor. No reorder buffer, no mid-stream framing switch, no compact single-stream mode |

## Open threads for the next cycle

- **A config the product refuses was a config every check accepted
  (2026-10-13, found while preparing v0.11.0).** The mandatory-Noise control
  channel was enforced in `run_instance` alone, so everything that *parses* a
  config was blind to it: 24 documented examples, every bench arm's
  `server.toml`/`client.toml`, both ends of the L3 acceptance harness and all
  three interop cases refused to start, while `just check` and 620 selfcheck
  assertions stayed green. Two guards now exist (the doc-example test covers
  the deployment pages and `from_str` carries the rule; `bench.py selfcheck`
  asks the built binary to load each arm's config). **The general version of
  this is still open**: any rule the daemon enforces at *runtime* rather than
  at parse time has the same hole, and there is no check that names them. The
  rule now lives in `from_str`, which is the right place for this one; a
  future one should start there.
- **Build times were decomposed (2026-10-13, host with 20 cores, rustc
  1.99); the workspace split is measured and declined for now.** The daily
  inner loop is already fast (check 1.3 s, clippy pass 1 9 s, the lib test
  binary 4.5 s; rustc 1.99's thin debuginfo makes a `debug = 1` dev-profile
  override worthless — measured, no change). The felt slowness is the
  **release build: 71 s, user ≈ wall**, which is single-crate serial
  codegen under fat LTO (`codegen-units = 1`); LTO-off is 61 s and thin LTO
  is 68 s wall (3.8 min CPU), so fat LTO costs only ~10 s and `mold` is a
    at the final link over the whole program, so any crate change pays the
    full 70 s regardless of how the tree is partitioned. What a split buys
    is finer check/test incrementality — which currently costs 1–4.5 s.
    Revisit when check times reach minutes or an architectural boundary
    needs enforcing; the dependency direction is already clean for it
    (`mux` ← `transport` ← `core` ← `main`, `mux` a zero-reverse-dep leaf).
  - The gate chain's benches-only classification now covers both hooks
    (`f9c0187`), which is where the actually-claimable latency lived.
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
- **`iperf3` is version-boundary-sensitive, and this session crossed one.**
  The carrier/KCP numbers of this cycle were measured with an `iperf3` that was
  no longer on PATH when this session began (a container restart); it was
  reinstalled and is **3.18** now. A version can change the UDP summary fields
  the model reads (`end.sum_received`, the loss percent, the interval shape), so
  a number from the other side of that boundary is context, not a comparable
  baseline — re-measure the baseline in the same session whenever the pair
  matters. The model should record the version with the run rather than leaving
  it to be rediscovered.
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
gate anything.** The live numbers are
[the release sweep record](benches/records/results-bench-v0.11.0.json), whose
method's home is [docs/benchmarks.md](docs/benchmarks.md).
