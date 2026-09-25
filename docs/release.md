# Releases


Releases are **tag-driven**. The only trigger of a release is pushing a `v*`
tag; `.github/workflows/release.yml` owns the whole flow and no other path
publishes a release.

## Versioning

molehill's version line began at v0.6.0 — the point where it forked from
[rathole](https://github.com/rapiz1/rathole); upstream's last release was
v0.5.0 — and has been numbered independently since.
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) applies within
that line.

## Release notes: CHANGELOG.md is the single source

`CHANGELOG.md` is maintained in
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format.

- During development, record notable changes under `## [Unreleased]`.
- Before tagging, move that content into a dated section:
  `## [x.y.z] - YYYY-MM-DD` (the git tag is the same version with a `v`
  prefix, e.g. `v0.7.1`).
- A missing or empty changelog section **fails the release** — the workflow
  errors out before building anything. Fix: add the section, delete the tag,
  re-push. Never hand-edit release notes on GitHub.

## Tag-push policy: no casual release pushes

Commits are always allowed — the fast gates guard them and they trigger
nothing public. Pushing a `v*` tag is a deliberate release act; the five
preconditions (explicit human request, `Cargo.toml` version match, dated
changelog section, green `just check`, green benchmark gate — see below) are
the repository rule stated in [AGENTS.md §5](../AGENTS.md) — the release
workflow enforces the version and changelog ones mechanically, and the local
tag review (`githooks/pre-tag`, next section) checks most of them before the
tag exists.

Re-tagging is allowed only to fix a failed release (delete the tag, fix,
re-push). For verifying a commit without releasing, use CD test builds.

## Tag review: `githooks/pre-tag`

Releases are published **directly** — there is no draft stage — so the
human checkpoint is the tag push itself, and a light release review runs at
the two tag moments (responsibilities of the three hooks are split in
[checks.md](checks.md)):

- **Locally**: `just tag` runs the review against HEAD, then creates the
  annotated tag for `Cargo.toml`'s version. `just tag-check` runs only the
  review.
- **On tag push**: git has no native tag hook, so `githooks/pre-push` re-runs
  the review (against the tag's commit) for every `v*` tag in the push,
  before the heavy gates — a violation fails the push.

The review mechanically verifies tag↔version match (including `Cargo.lock`),
a dated non-empty changelog section, the committed bench results/chart, and
the container job structure (GHCR job, image tags, `--help` smoke test),
then prints an advisory checklist — CHANGELOG and docs audit, container
build review, benchmark gate, deliberate-release confirmation — that only a
human/agent review can clear. Bench assets are required at tag creation; on
a re-push of a historical tag they degrade to a note (the ritual postdates
older tags, and re-pushing one to fix a failed release must stay possible).
It is deliberately light: heavy gates
(audit/deny/outdated/test) belong to pre-push and CI, and release.yml
re-enforces the version and changelog invariants remotely.

## Benchmarks: per-tag ritual

Every tag refreshes the Soak benchmark (design and tools live in
`benches/scripts/soak/`; peers: frp, rathole (upstream), nps — each fetched as
the **latest GitHub release** binary, never built from source, with the
resolved versions recorded in the results meta). The model measures a
**workload under staged network conditions as time series**: one interactive
stream (the SLO instrument), N bulk TCP streams, C short connections per
second and one UDP session, while the path changes on a stage schedule
(clean → rtt100 → loss1 → loss5 → rate100 → rate20 → jitter → clean), applied
in place so the tool's session is never rebuilt. Everything measured is
externally observable, so the peers are driven by exactly the same workload
and appear beside molehill in every chart. All entries are PEP 723 python
scripts run via `uv run`; a run refuses to start while another one holds the
lock, and a killed run (Ctrl-C or SIGTERM) still writes the tests it
completed.

1. `just soak-peers` — fetch/refresh the peer binaries (cached per release).
2. `just soak` — run the sweep (one tool or a batch of them, per its own
   shaped path in one HTB class each, so concurrent tools never share a
   shaper; the batch size comes from the host's CPU budget). Writes
   `benches/scripts/soak/results-soak-vX.Y.Z.json` (the default `--out`
   derives from `Cargo.toml`'s version). Test types: `capacity` (ramp the
   load until the interactive stream breaks the SLO), `rrul` (saturate and
   watch the interactive stream's RTT distribution over time), `soak` (a
   long rotating-path drift/leak run), `cost` (CPU-seconds per carried
   Gbit/s at a fixed operating point), `screen` (a fast development A/B —
   see below).
3. `just soak-plot` — renders the chart set (`assets/soak-vX.Y.Z.png`: the
   interactive stream over the stage schedule with the SLO line;
   `soak-*-capacity.png`: the response-time-vs-load curve; `soak-*-udp.png`;
   `soak-*-drift.png`) and prints the markdown tables; update the README
   Benchmarks section with them, then delete the previous tag's charts from
   `assets/`.
4. `just soak-check` — regression gate against the previous tag's results
   file. **A tool must not lose capacity, must not break its SLO earlier and
   must not drift**; a violation blocks the tag until fixed or explicitly
   waived (record the waiver in `HANDOFF.md`). On the first soak release
   there is no same-model baseline: the gate is the absolute SLO, and that
   is stated in the README.

The gate runs locally before tagging, never in CI: shared runners are too
noisy for performance numbers. Weak-network loss cells need `CAP_NET_ADMIN`
(netem); without it the run aborts — there is no userspace fallback, because
a fallback path is a second measurement method. The gate is only meaningful
between same-model results; the retired matrix model's numbers (v0.8.0 and
earlier, in git history) are a different instrument and are never a
regression signal. `benches/scripts/soak/soak_check.py` is the companion
that applies the per-type claim rules and the screen verdict.

### Comparing two builds (development screening)

Outside the release ritual, the question is usually *"is this direction
worth pursuing?"* — and the answer must be minutes, not hours. That is the
`screen` test type: one test type, one path class, one configuration pair,
the two builds **interleaved inside every load step** (the pair runs in the
same batch, so both sample the same machine state — sequential
before/after runs are defeated by epoch drift), with a sequential decision:

```bash
# build both, then one interleaved screen run
just soak --test=screen --path=loss1 --streams-max=8 \
     --ab /path/to/bin-parent,/path/to/bin-head --out results-screen.json
just soak-check --screen results-screen.json    # per-step verdict
```

The verdict prints per-step medians for both builds, the effect size, and a
CLAIM only where every step agrees in sign and exceeds the threshold;
anything else is reported as *directional* with its effect size. A screen
verdict is **domain-scoped**: it says whether to pursue the direction on
that path class, never whether the change may ship — the sweep and the gate
decide that. `bench.py`'s retired `--ab` mode did the same job for the old
matrix; the screen is its successor and adds the SLO instrument as the
second measured axis.

## What the release workflow does

1. **pre-release checks**: Cargo.toml version ↔ tag version, CHANGELOG
   section present, `cargo audit`.
2. **build matrix** (9 targets): linux gnu + musl, aarch64 musl (via cross),
   arm/armv7 musl (`embedded` feature), macOS x86_64 + aarch64, Windows
   msvc. musl artifacts build with the full feature set
   (`server,client,noise,hot-reload,multiplex`); linux artifacts are
   UPX-compressed. Tests run inside the matrix for native and cross targets.
3. **GitHub Release**: published **directly** (no draft stage — the human
   checkpoint is the tag push itself, guarded locally by the pre-tag
   review), with notes extracted from `CHANGELOG.md`, all archives, and a
   `SHA256SUMS`. Never edit the notes by hand.
4. **GHCR**: publishes the multi-arch scratch image
   (`ghcr.io/niyueee/molehill:<tag>` and `:latest`) from the musl artifacts,
   then smoke-tests it.
5. **crates.io**: publishes `molehill-rathole` using the `CRATES_IO_API_TOKEN`
   secret.

## CD test builds: per-commit, per-platform artifacts

`.github/workflows/test-build.yml` builds **test artifacts** from any commit
without creating a release: dispatch it manually from the Actions tab, choose
a `ref` (commit SHA, branch, or tag) and `targets` (`linux`, `macos`,
`windows`).

- Artifacts are ephemeral (7-day retention) and are never a Release — do not
  hand out release links for them, and do not reference them in the
  changelog.
- Typical uses: verifying that a specific commit compiles on all platforms
  before tagging, and reproducing platform-specific issues on an exact
  commit.
