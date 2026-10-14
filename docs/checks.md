# Checks

Three layered gates guard the repository, split by moment and weight:

| Moment | Gate | Weight | What it guards |
|--------|------|--------|----------------|
| every commit | `githooks/pre-commit` | fast (~1 min) | code quality: fmt, secrets, machete, docs sync, ruff lint + format, bench-model self-check, clippy ×2 |
| every push | `githooks/pre-push` | heavy (minutes) | security & dependency policy & freshness & tests (audit, deny, outdated, test) |
| every release tag | `githooks/pre-tag` | light (seconds) | release state: tag↔version, changelog section, bench assets, container-job greps + advisory checklist |

The split is deliberate: pre-commit is the fast per-commit quality loop,
pre-push is the heavyweight loop every push pays (tag pushes included —
releasing is a deliberate act), and pre-tag is a light release-readiness
review. git has no native tag hook, so pre-tag fires at the two tag moments:
`just tag` runs it before creating the tag, and pre-push runs it for every
`v*` tag in a push (before the heavy gates, so a violation fails fast). It is
not part of `just check` — it needs a release state (dated changelog section,
committed bench results) and would fail on ordinary development commits.
release.yml re-enforces the version and changelog invariants remotely. CI runs
pre-commit + pre-push via `just check` on every pull request and on pushes to
`main`/`dev` **that touch code**. A change limited to markdown, `docs/` or
`assets/` cannot move the Rust gates, so `ci.yml` filters those paths out and
a second workflow, `docs.yml`, runs the doc gates for them instead — the
docs-alignment check, which is precisely what a docs edit can break. A commit
mixing docs and code paths runs both workflows.

Beyond that chain, CI runs jobs this page does not cover — the feature powerset,
the alternative feature-set test matrix, the cross-platform builds and the musl
check of what `release.yml` ships. Their roster, and what each one guards, is
owned by the "Automation" table in [structure.md](structure.md).

## Tools

The cargo gates need `cargo-machete`, `cargo-audit`, `cargo-outdated` and
`cargo-deny`, and the python gates need `uv`/`uvx`; `just setup` installs what is
missing and activates the git hooks. The setup routine — which tools must be on
PATH, and `cargo-hack` for `just powerset` — is in [AGENTS.md §1](../AGENTS.md).

## The docs-only path (both fast and heavy gates)

A change whose paths are all markdown, `docs/**` or `assets/**` cannot move
the code gates, so both hooks classify it with `githooks/docs-only` before
running anything and take a two-gate path instead:

| Where | Runs | Skips |
|---|---|---|
| `githooks/pre-commit` | `githooks/check-secrets`, `githooks/check-docs` | fmt, machete, both ruff gates, the bench-model self-check, both clippy passes |
| `githooks/pre-push` | `githooks/check-docs` | audit, deny, outdated, tests |

Three properties make this safe rather than a bypass:

- the classifier reads the **actual change set** — the staged paths for a
  commit, the pushed commit range for a push — and the three patterns are
  exactly `ci.yml`'s `paths-ignore` / `docs.yml`'s `paths`, so the hooks and CI
  can never disagree about what "docs-only" means;
- the two gates a docs-only change *can* still break always run: the secret
  scan (a leaked key in a README is still a leaked key) and the docs-alignment
  check (which is what such a change moves);
- anything ambiguous falls back to the full chain: an empty change set, a new
  branch with no remote to diff against, a **tag push** (never docs-only, the
  release review runs first), and a commit that mixes docs with code paths.

`githooks/docs-only` is the classifier itself: paths on stdin, exit 0 when all
of them are docs. Deleting it makes both hooks run the full chain again, which
is the safe direction.

Lines that must carry a secret-shaped string (e.g. key-format documentation)
take a `security-scan:allow` marker with a reason; `check-secrets` skips them.

## On every commit — `githooks/pre-commit`

| # | Gate | Command | Purpose |
|---|------|---------|---------|
| 1 | fmt | `cargo fmt --all -- --check` | code style |
| 2 | secrets | `githooks/check-secrets` | secret scan on staged changes |
| 3 | machete | `cargo machete` | unused dependencies |
| 4 | docs | `githooks/check-docs` | docs ↔ code alignment |
| 5 | python lint | `uvx ruff check benches/scripts/` | python bench/test entries (ruff.toml) |
| 6 | python format | `uvx ruff format --check benches/scripts/` | python formatting (auto-fix: `just py-fmt`) |
| 7 | bench model | `uv run benches/scripts/bench/bench.py selfcheck` | the performance model's own rules: metric registry, verdicts, comparability (see "The performance model", below) |
| 8 | clippy | `cargo clippy --all-targets -- -D warnings` | strict lints, default features |
| 9 | clippy (gates) | `cargo clippy --all-targets --no-default-features --features server,client -- -D warnings` | feature-gated code paths |

Note the template difference: clippy runs twice (default features, then
`server,client` only) instead of once with `--all-features`, because the
second pass covers the minimal no-default-features build that the
default-feature pass never compiles. `just check` runs the identical chain.

## On every push — `githooks/pre-push`

| # | Gate | Command | Purpose |
|---|------|---------|---------|
| 10 | audit | `cargo audit` | RustSec security advisories |
| 11 | deny | `cargo deny check` | licenses / bans / advisories policy (deny.toml) |
| 12 | outdated | `cargo outdated --root-deps-only` | outdated direct dependencies |
| 13 | test | `cargo test --quiet -- --test-threads=1` | test suite (serial by design) |

Tests run **serially** (`--test-threads=1`): the integration suite spawns real
server/client pairs on fixed ports; parallel execution races on them.

A push that carries `v*` tags additionally runs the release review
(`githooks/pre-tag`, below) before these heavy gates — releasing is a
deliberate act, and a tag review violation fails the whole push.

## On every tag — `githooks/pre-tag`

A light release review for the next `v*` tag (AGENTS.md §5). Fires at tag
creation (`just tag`) and again on tag push (inside pre-push, evaluated
against the tag's commit — so re-pushing an old tag reviews that tag's state,
not the working tree). All checks are seconds-fast greps against the reviewed
commit; nothing here is a heavy gate.

| # | Check | Purpose |
|---|-------|---------|
| 14 | tag name ↔ `Cargo.toml` version; `Cargo.lock` in sync | release identity |
| 15 | exactly one dated `## [x.y.z] - YYYY-MM-DD` section in `CHANGELOG.md`, with prose in it and an empty `## [Unreleased]` | release notes single source |
| 16 | `benches/records/results-bench-vX.Y.Z.json` + `assets/bench-vX.Y.Z.png` committed, **and the results file's recorded revision has no code change between it and the reviewed commit** | benchmark ritual deliverables, and numbers that describe the state being released (AGENTS.md §10, "prove provenance") |
| 17 | `Containerfile` + release.yml GHCR job / image tags / `--help` smoke test | container build review (mechanical part) |
| 18 | advisory checklist: CHANGELOG & docs audit, container review, benchmark gate, deliberate-release confirm | human/agent review items |

At tag creation (local mode only) it additionally requires a clean working
tree and that the tag does not exist yet. On a tag push (evaluated against a
commit), missing bench assets are a note instead of a failure — a historical
tag legitimately predates the ritual, and re-pushing one to fix a failed
release must stay possible. The advisory checklist is printed but not
enforced mechanically — no gate can review prose.

## One-shot run

```bash
just check   # identical to hooks + CI (pre-commit + pre-push)
just tag     # release review (githooks/pre-tag) + create the local v* tag
just tag-check   # run only the release review, without tagging
```

Other recipes: `just fmt` / `just py-fmt` (auto-fix), `just test` (the full
serial suite), `just test-fast` (lib tests + the core integration subset,
~1 min), `just powerset` (feature powerset via cargo-hack, CI's `features`
job), `just py-lint` (both ruff gates, also in the pre-commit gate),
`just bench-selfcheck` (the performance model's own checks, also in the
pre-commit gate), `just bench-deps` (iperf3 + iproute2 on the benchmark host),
`just bench-peers` (fetch the peer tools' latest release binaries),
`just bench-plot` (charts + markdown tables from a results file),
`just bench-gate` (the release gate: coverage, the endpoint invariant, the SLO,
drift, wedges, the ramp, and the regression half against a baseline),
`just container` (scratch image), `just interop` (the interop matrix, below).
The performance model's own commands are in the section below.

## Outside the chain: the interop matrix (`just interop`)

Every other test compiles **both** ends from the working tree, so no gate in
the chain can see a wire-format break: a change that stops this build from
talking to the previous release passes all of them. `just interop` closes that
hole by running a real server and a real client as subprocesses with one of
them the *released asset* — a binary built from today's tree cannot test
yesterday's protocol.

`benches/scripts/interop/fetch_old.py` downloads the newest release (asset for
this host, cached under `~/tmp/interop`; `MOLEHILL_OLD_TAG=vX.Y.Z` skips the
release listing), prints `MOLEHILL_OLD_BIN=…`, and the recipe hands that to
`cargo test --test interop_test`. Three cases, all of them about a refusal
being *local*: an old server refuses the new client's dialect, a new server
refuses the old client's dialect (v0.10 serves v4 only), and an old server
refuses an unknown dialect — each on that connection alone, while the same
process keeps serving a client of its own version.

It is **not** part of `just check` or CI: it needs network access and a GitHub
release asset, and a CI runner has neither the previous release nor a reason to
trust one. It is a local and pre-release step — `docs/release.md` lists it in
the tag ritual, and `tests/interop_test.rs` **skips loudly** (never silently)
when `MOLEHILL_OLD_BIN` is unset, so a plain `cargo test` stays honest about
what it did not check.

### Transparent-L3 acceptance (`just l3-accept`)

`just l3-accept` runs `benches/scripts/l3/run.sh`: three network namespaces, a
real `molehill` server and client, and a visitor whose connection is owned by
the **client** namespace — the server routes the packets into its TUN while
holding no socket and no conntrack entry for the flow, and the backend sees the
visitor's real address.

It also measures: each arm is bracketed by `/proc/net/dev` samples inside the
namespaces, and `benches/scripts/l3/wire_report.py` turns them into carried
packet sizes and the ceiling a header compressor could reach. The method and the
numbers it produced belong to [benchmarks.md](benchmarks.md#the-transparent-l3-wire-question-the-acceptance-harness);
its verdict was that compression is not worth building.

The same run also covers the server's own switch, negatively: it restarts the
server without `[server.transparent]` **after deleting that namespace's TUN
device**, so the refusal it asserts on must be the policy one — a server that
checked the interface first would answer with the missing-device recipe
instead. That ordering is the point: a client's registration must never be what
makes a server reach for `/dev/net/tun`.

It is Linux-only (a TUN device) and root-only (`CAP_NET_ADMIN`) for its veth
pairs, TUN devices, routes and `ip rule`, so it is **not** part of `just check`
or CI. Without root it **skips loudly**: it prints `SKIP` with the exact `sudo`
command and exits 77, never a silent pass. The daemon configures no network
itself; the harness is the operator.

### The performance model (`just bench`)

`benches/scripts/bench/` is the repository's measurement standard and its only
runner: a declared metric registry, one topology for every arm (the product's
configurations, the reference tools and a control with no tool in the path),
scenarios that each state the claim they support, conditions and timelines that
change the path in place, and verdicts that clear the run's own measured noise
floor before they claim anything. Its method is owned by
[benchmarks.md](benchmarks.md#the-bench-model-the-measurement-standard).

Two of its commands are in the check chain, and the rest are measurements:

| Command | In the chain? | What it is |
|---|---|---|
| `just bench-selfcheck` | **yes** — a fast gate in `githooks/pre-commit` | the model's own checks: the metric registry is consistent, the verdict rules do what they say, comparability refuses what it must, the probes compile. No root, no topology, about a second. |
| `just bench` | no | a campaign (`--profile smoke\|dev\|full\|stage\|soak\|screen\|sweep`): root-only (three network namespaces), minutes to hours, results outside the tree unless `--out` says otherwise |
| `just bench-doctor` | no | what this host can measure, and what it cannot (root, TUN, iperf3, the peer binaries) |
| `just bench-list` | no | the registry: every metric, condition, timeline, arm and profile |
| `just bench-report` / `just bench-compare` / `just bench-gate` | no | render a stored run; A/B two of them (or refuse); gate a release run |
| `just bench-plot` | no | the charts and markdown tables of a results file |
| `just bench-peers` | no | fetch the peer tools' release binaries into `~/tmp/bench-peers` |

The model asserts nothing about the tool and gates nothing in CI: it produces
numbers and verdicts, the gate decides whether a *release* run may be published,
and `docs/release.md` owns the ritual. Keeping its self-check in the fast gate
is what keeps the *rules* enforced even when nobody is measuring — a metric
definition that contradicts the code, or a comparability rule that stopped
refusing, fails a commit rather than a campaign.

## When a gate blocks you

Fix the code first; a waiver is the last resort. The discipline that governs one
— code-level only, minimal scope, with a reason — is owned by
[Lint policy](lint-policy.md) and [AGENTS.md](../AGENTS.md).
