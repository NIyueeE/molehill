//! The shared pinned pool (M2a): one pool per `(session, carrier)` when
//! `[client.data].shared_pool` is on, per service when it is off, and the S1
//! observation that makes the policy falsifiable.
//!
//! The pool is **pinned**: it establishes `[client.data.tcp|kcp].tunnels`
//! (default 4, raised to the services' UDP-derived floor) at service start and
//! keeps them for the service's lifetime. It never grows for load and never
//! shrinks when idle; repairing a dead tunnel until the count is met again is
//! the only establishment after service start, and placement — 56 streams per
//! tunnel — is the only oversubscription protection.
//!
//! The pool's own state is what these tests assert on
//! ([`molehill_rathole::live_pools`]): a snapshot is exactly what the
//! `MOLEHILL_POOL_STATS` line renders, so a passing test and a readable
//! telemetry line cannot drift apart. That registry is process-global, so
//! **one client per test process**: each scenario here drives an in-process
//! client and the suite runs serially (`--test-threads=1`, like every other
//! integration binary in this repository).
//!
//! The instrumentation itself is measured on the real binary, not here: see
//! [`the_pool_and_placement_lines_are_opt_in`], which runs the compiled
//! `molehill` with `MOLEHILL_POOL_STATS=1` / `MOLEHILL_PLACEMENT_STATS=1` and
//! reads what the operator would see, and
//! [`a_refused_establishment_is_reported_once`], which reads the refusal a
//! valve produces in a fresh process.
#![cfg(all(feature = "multiplex", feature = "client", feature = "server"))]
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "integration tests unwrap and assert on values they just produced"
)]

use anyhow::{Ok, Result, anyhow};
use common::{PING, run_molehill_client, run_molehill_server};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::broadcast,
    time,
};
use tracing_subscriber::EnvFilter;

mod common;

// Ports for the shared-pool scenarios (`tests/for_tcp/shared_pool.toml`): its
// control port is 2380, and these are the two exposed ones.
const POOL_ALPHA: &str = "127.0.0.1:2381";
const POOL_BETA: &str = "127.0.0.1:2382";
const POOL_BACKEND_A: &str = "127.0.0.1:8090";
const POOL_BACKEND_B: &str = "127.0.0.1:8091";

// Ports for the UDP pool scenario (`tests/for_udp/shared_pool_udp.toml`).
const UDP_POOL_EXPOSED: &str = "127.0.0.1:2386";
// The pool suite owns every port it binds: the integration suite owns
// 8080/8081/8082, and `cargo test` may run both binaries at once.
const UDP_POOL_LOCAL: &str = "127.0.0.1:8083";

// Ports and paths for the idle-pool scenario (`tests/for_tcp/idle_pool.toml`).
const IDLE_POOL_EXPOSED: &str = "127.0.0.1:2396";

// Ports for the operator's-valve scenario (`tests/for_tcp/tunnel_valve.toml`).
const VALVE_EXPOSED: &str = "127.0.0.1:2371";

/// Enough load to keep the valve scenario's one tunnel busy while leaving it
/// room to keep working. Holding the full `LOAD_STREAMS` there would also put
/// that single tunnel at its placement ceiling, and the visitors it then
/// carries would be refused for capacity rather than forwarded — a different
/// scenario, covered by `a_valve_held_pool_still_never_reaches_the_stream_cap`.
const VALVE_LOAD: usize = 8;

// The binary-run telemetry scenario writes its own config on ports the OS
// picks at run time: a stale child from an interrupted run cannot make the
// scenario fail, and the file can say exactly what it measured.
const STATS_BACKEND: &str = POOL_BACKEND_A;

/// Free localhost ports, asked of the OS and **held**: the listeners stay alive
/// for as long as the caller keeps them, so a second reservation cannot take
/// the same port back between the two binary runs of the scenario.
fn free_ports<const N: usize>() -> ([u16; N], Vec<std::net::TcpListener>) {
    let mut ports = [0u16; N];
    let mut held = Vec::with_capacity(N);
    for slot in &mut ports {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        *slot = l.local_addr().unwrap().port();
        held.push(l);
    }
    (ports, held)
}

fn init() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::from("info")),
        )
        .try_init();
}

async fn settle(secs: f64) {
    time::sleep(Duration::from_secs_f64(secs)).await;
}

/// One echo round trip through the exposed endpoint, with a cap: the
/// readiness probe every scenario polls instead of guessing at startup timing.
async fn probe_tcp(addr: &str) -> Result<()> {
    let attempt = async {
        let mut conn = TcpStream::connect(addr).await?;
        conn.write_all(PING.as_bytes()).await?;
        let mut rd = [0u8; 4];
        conn.read_exact(&mut rd).await?;
        anyhow::ensure!(rd == *PING.as_bytes(), "unexpected echo reply");
        Ok(())
    };
    time::timeout(Duration::from_millis(500), attempt).await??;
    Ok(())
}

async fn wait_for_tcp(addr: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if probe_tcp(addr).await.is_ok() {
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("the service at {addr} did not become ready within 15 s");
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

/// One round trip on an existing UDP socket: the readiness probe for the
/// scenario that must keep a single source port — a dedicated probe socket
/// would add a second source port and trip the very invariant under test.
async fn probe_udp(conn: &UdpSocket) -> Result<()> {
    let attempt = async {
        conn.send(PING.as_bytes()).await?;
        let mut rd = [0u8; 4];
        conn.recv(&mut rd).await?;
        anyhow::ensure!(rd == *PING.as_bytes(), "unexpected echo reply");
        Ok(())
    };
    time::timeout(Duration::from_millis(500), attempt).await??;
    Ok(())
}

async fn wait_for_udp(conn: &UdpSocket) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if probe_udp(conn).await.is_ok() {
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("the UDP service did not become ready within 15 s");
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

/// The pool scenarios' own echo backends, on their own ports: the
/// integration suite owns 8080/8081, and the two test binaries may run
/// concurrently.
fn spawn_tcp_backends() {
    for addr in [POOL_BACKEND_A, POOL_BACKEND_B] {
        tokio::spawn(async move {
            if let Err(e) = common::tcp::echo_server(addr).await {
                panic!("Failed to run the echo server at {addr} for testing: {e:?}");
            }
        });
    }
}

/// `[client.data].shared_pool = true`: two services of one client are served
/// by **one** pool, and it carries both services' channels. Both services
/// forward, or a shared pool would only prove that a broken service shares a
/// broken pool.
#[tokio::test]
async fn a_shared_pool_serves_two_services_of_one_session() -> Result<()> {
    init();
    spawn_tcp_backends();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/shared_pool.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server("tests/for_tcp/shared_pool.toml", server_shutdown_rx)
            .await
            .unwrap();
    });

    // `alpha` echoes, `beta` ping-pongs: both on one session, so one pool must
    // serve the two of them.
    wait_for_tcp(POOL_ALPHA).await?;
    wait_for_tcp(POOL_BETA).await?;

    let pools = molehill_rathole::live_pools();
    assert_eq!(
        pools.len(),
        1,
        "two services with `shared_pool = true` must share one pool, got {pools:?}"
    );
    let pool = &pools[0];
    assert_eq!(
        pool.key, "session/tcp:127.0.0.1:2380",
        "a shared pool is keyed by the session, its carrier and its data endpoint"
    );
    assert!(
        pool.size >= 1 && pool.streams() > 0,
        "the shared pool must be serving both services' channels: {pool:?}"
    );
    assert_eq!(
        pool.size, pool.count,
        "a pinned shared pool holds exactly the count it was configured with: {pool:?}"
    );

    // Still one pool, and still serving, after both services have taken
    // traffic through it.
    wait_for_tcp(POOL_ALPHA).await?;
    wait_for_tcp(POOL_BETA).await?;
    let after = molehill_rathole::live_pools();
    assert_eq!(after.len(), 1, "the pools must not multiply under traffic");
    assert!(
        after[0].streams() > 0,
        "a serving shared pool must report its live streams: {:?}",
        after[0]
    );

    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}

/// `[client.data].shared_pool = false` (the default): every service keeps its
/// own pool. The control for the test above — the same fixture, one key
/// different.
#[tokio::test]
async fn without_shared_pool_every_service_keeps_its_own() -> Result<()> {
    init();
    spawn_tcp_backends();

    let variant = write_client_variant("tests/for_tcp/shared_pool.toml", "false")?;
    let path = variant.to_string_lossy().into_owned();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let client_path = path.clone();
    let client = tokio::spawn(async move {
        run_molehill_client(&client_path, client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    let server_path = path.clone();
    let server = tokio::spawn(async move {
        run_molehill_server(&server_path, server_shutdown_rx)
            .await
            .unwrap();
    });

    wait_for_tcp(POOL_ALPHA).await?;
    wait_for_tcp(POOL_BETA).await?;

    let pools = molehill_rathole::live_pools();
    assert_eq!(
        pools.len(),
        2,
        "without `shared_pool` every service keeps its own pool: {pools:?}"
    );
    for pool in &pools {
        assert!(
            pool.key.starts_with("service:"),
            "a per-service pool is keyed by the service: {pool:?}"
        );
    }

    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    let _ = std::fs::remove_file(&variant);
    Ok(())
}

/// Materialize a copy of `config_path` with `shared_pool` forced to `value`.
fn write_client_variant(config_path: &str, value: &str) -> Result<PathBuf> {
    let source = std::path::Path::new(config_path);
    let contents = std::fs::read_to_string(source)?;
    let mut doc: toml::Value = toml::from_str(&contents)?;
    let data = doc
        .get_mut("client")
        .and_then(|c| c.get_mut("data"))
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| anyhow::anyhow!("{config_path} has no [client.data] table"))?;
    data.insert(
        "shared_pool".to_owned(),
        toml::Value::Boolean(value == "true"),
    );
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("pool");
    let variant = std::env::temp_dir().join(format!(
        "molehill_pool_{stem}_{}_{value}.toml",
        std::process::id()
    ));
    std::fs::write(&variant, toml::to_string(&doc)?)?;
    Ok(variant)
}

/// The UDP invariant a pinned pool has to hold (D30): the pool is established
/// at its configured count — one the UDP-derived floor of its service's
/// workers must fit inside — and keeps it, so the tunnel the visitor's peer is
/// pinned to is never given back. A resize that removed that tunnel would drop
/// the peer's local socket and change the source port the local service sees.
///
/// An elastic pool grew to this floor and shrank back to it once the tunnels
/// went idle; a pinned one starts there and stays, which is what the
/// keep-talking window below measures: the peer stays pinned and the pool
/// neither grows nor shrinks across it.
#[tokio::test]
async fn udp_source_port_survives_a_pinned_pool() -> Result<()> {
    init();

    // A stateful "game server": it records every source address a datagram
    // arrives from, and echoes back to it.
    let seen_srcs = Arc::new(Mutex::new(HashSet::new()));
    let server_seen = Arc::clone(&seen_srcs);
    tokio::spawn(async move {
        if let Err(e) = sticky_echo_server(server_seen).await {
            panic!("Failed to run the sticky echo server for testing: {e:?}");
        }
    });

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_udp/shared_pool_udp.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server("tests/for_udp/shared_pool_udp.toml", server_shutdown_rx)
            .await
            .unwrap();
    });

    let conn = UdpSocket::bind("127.0.0.1:0").await?;
    conn.connect(UDP_POOL_EXPOSED).await?;
    wait_for_udp(&conn).await?;

    // The pool is complete — at its count, with the peer pinned to the tunnel
    // its channel lives on — without any resize having been needed to get
    // there.
    let mut complete = None;
    for _ in 0..60 {
        probe_udp(&conn).await?;
        settle(0.4).await;
        if let Some(pool) = molehill_rathole::live_pools().first()
            && pool.size == pool.count
            && pool.pinned() > 0
        {
            complete = Some(pool.clone());
            break;
        }
    }
    let pools = molehill_rathole::live_pools();
    let pool = complete.unwrap_or_else(|| {
        panic!("the UDP service's pinned pool must come up at its count: {pools:?}")
    });
    assert_eq!(
        pool.count, 3,
        "the fixture's `tunnels` is the starting width: {pool:?}"
    );
    assert!(
        pool.count >= pool.udp_floor,
        "the configured count must cover the UDP-derived floor of its workers: {pool:?}"
    );
    assert_eq!(
        pool.udp_floor, 2,
        "the fixture's two UDP workers are the pool's floor: {pool:?}"
    );

    // Keep the peer talking across the window in which the pool used to shrink
    // once the tunnels went idle: the pinned tunnel must stay, the pool must
    // stay at its count, and neither rule may resize it.
    for _ in 0..4 {
        time::sleep(Duration::from_secs(3)).await;
        probe_udp(&conn).await?;
        let pools = molehill_rathole::live_pools();
        let Some(pool) = pools.first() else {
            panic!("the UDP service's shared pool must exist: {pools:?}");
        };
        assert!(
            pool.pinned() > 0,
            "a live peer must keep its tunnel pinned: {pool:?}"
        );
        assert_eq!(
            pool.size, pool.count,
            "a pinned pool must not resize around the pinned peer: {pool:?}"
        );
        assert!(
            pool.size >= pool.udp_floor,
            "the pool must never drop below the floor its workers need: {pool:?}"
        );
        assert_eq!(
            (pool.grows, pool.shrinks),
            (0, 0),
            "a healthy pinned pool neither grows nor shrinks: {pool:?}"
        );
    }

    let (src_count, srcs) = {
        let seen = seen_srcs.lock().unwrap();
        (seen.len(), seen.clone())
    };
    assert_eq!(
        src_count, 1,
        "the peer's session was split across source ports across the observation window: {srcs:?}"
    );

    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(server, client);
    Ok(())
}

/// Concurrent visitors the pinned-pool load scenario holds at once: far past
/// the growth threshold an elastic pool used to react to (12 % of a tunnel's
/// stream capacity) and at the placement ceiling (56), which is exactly the
/// load that rule existed for.
const LOAD_STREAMS: usize = 56;

/// The engine's per-tunnel stream cap (`DEFAULT_MUX_MAX_STREAMS`), repeated
/// here because the assertions below are about crossing it.
const ENGINE_STREAM_CAP: usize = 64;

/// The pool's own per-tunnel placement ceiling
/// (`transport::pool::TUNNEL_STREAM_CEILING`), repeated for the same reason:
/// the burst scenarios drive the pool up to it, and the two are deliberately
/// different numbers.
const POOL_STREAM_CEILING: usize = 56;

/// A burst past what the pool can place must never cost a tunnel (D14's hard
/// half).
///
/// A pinned pool has exactly one protection against oversubscription:
/// placement. Its two tunnels carry at most [`POOL_STREAM_CEILING`] (56)
/// streams each, so the pool places 112 concurrent visitors and refuses the
/// ones past that — such an open waits `CAPACITY_WAIT` (250 ms) for a stream
/// to retire and then fails *that visitor* — instead of pushing a tunnel to
/// the engine's 64-stream cap, where a session-terminating goaway used to take
/// the whole connection and every visitor on it down (the vendored engine logs
/// an unguarded `error!` first).
///
/// So the contract is inverted from the elastic pool's: the burst is not
/// absorbed by growing, it is refused at the ceiling while every tunnel — and
/// every visitor already placed on one — survives.
#[tokio::test]
async fn a_burst_past_the_stream_cap_never_costs_a_tunnel() -> Result<()> {
    init();
    spawn_tcp_backends();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/idle_pool.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server("tests/for_tcp/idle_pool.toml", server_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_tcp(IDLE_POOL_EXPOSED).await?;

    // Past what both tunnels of the pinned pool may place: the visitors beyond
    // `count x ceiling` have nowhere to go, and the pool may not grow to give
    // them one.
    let capacity = POOL_STREAM_CEILING * 2;
    let burst = capacity + 8;
    let mut held = Vec::with_capacity(burst);
    for _ in 0..burst {
        held.push(TcpStream::connect(IDLE_POOL_EXPOSED).await?);
    }
    for conn in &mut held {
        conn.write_all(PING.as_bytes()).await?;
    }
    // Three outcomes, and only the first is an answer: a placed visitor echoes,
    // a refused one has its socket closed by the server (the read *ends* — its
    // buffer was never written, which is why the read's own result is checked
    // and not only the timeout's), and a refused visitor still inside the
    // server's pairing budget is the third: its request failed on the client,
    // and the server sheds the connection when its budget runs out.
    let mut answered = 0;
    let mut refused = 0;
    let mut waiting = 0;
    for conn in &mut held {
        let mut rd = [0u8; 4];
        match time::timeout(Duration::from_secs(2), conn.read_exact(&mut rd)).await {
            std::result::Result::Ok(std::result::Result::Ok(_)) => {
                assert_eq!(&rd, PING.as_bytes(), "a visitor was answered with garbage");
                answered += 1;
            }
            std::result::Result::Ok(std::result::Result::Err(_)) => refused += 1,
            std::result::Result::Err(_) => waiting += 1,
        }
    }
    assert!(
        answered > 0,
        "the burst cost the tunnels: not one of {burst} visitors was served"
    );
    // Two tunnels at the placement ceiling place exactly `capacity` streams,
    // and the pool may not grow to place more.
    assert!(
        answered <= capacity,
        "the pool placed {answered} streams, but its two tunnels carry at most \
         {capacity}: {refused} refused, {waiting} unanswered after 2 s"
    );
    assert!(
        refused + waiting > 0,
        "all {burst} visitors were served by {capacity} placements: the ones beyond \
         the pool's ceiling must be refused, not absorbed"
    );

    let pools = molehill_rathole::live_pools();
    let pool = pools
        .first()
        .unwrap_or_else(|| panic!("the pool exists once its service is up"));
    assert_eq!(
        pool.size, pool.count,
        "the burst must not have grown the pinned pool: {pool:?}"
    );
    assert_eq!(
        pool.grows, 0,
        "a pinned pool grows for nothing, a burst included: {pool:?}"
    );
    assert!(
        pool.tunnels
            .iter()
            .all(|(streams, _, _)| *streams <= POOL_STREAM_CEILING),
        "the placement ceiling must hold per tunnel: {pool:?}"
    );

    drop(held);
    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}

/// A pinned pool's whole life, measured, in one scenario: it is at its
/// configured count from the moment its service is up — before any visitor —
/// load does not move it, and an idle stretch does not shrink it.
///
/// The elastic sequence this replaces (cold pool, synchronously grown by its
/// first visitor, grown again past the load threshold, shrunk one tunnel after
/// `idle_timeout`) has no meaning here: `default_count`, `count`, `max_tunnels`
/// and `[client.data].idle_timeout` are all gone, and what is left is
/// establishment at service start plus repair.
#[tokio::test]
async fn a_pinned_pool_starts_at_its_count_and_stays_there() -> Result<()> {
    init();
    spawn_tcp_backends();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/idle_pool.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server("tests/for_tcp/idle_pool.toml", server_shutdown_rx)
            .await
            .unwrap();
    });

    // The pool is complete before anything asks it to carry a byte: the
    // tunnels are dialed while the service registers, not by the first open.
    let mut started = None;
    for _ in 0..50 {
        settle(0.2).await;
        if let Some(pool) = molehill_rathole::live_pools().first() {
            started = Some(pool.clone());
            break;
        }
    }
    let pools = molehill_rathole::live_pools();
    let pool =
        started.unwrap_or_else(|| panic!("the pool must exist once its service is up: {pools:?}"));
    assert_eq!(
        pool.count, 2,
        "the fixture's `tunnels` is the pool's width: {pool:?}"
    );
    assert_eq!(
        pool.size, pool.count,
        "the pool must be established at its count before the first visitor: {pool:?}"
    );
    assert_eq!(
        pool.grows, 0,
        "establishment at service start is not a growth event: {pool:?}"
    );

    // The first visitor is served by the pool that is already there, and the
    // load an elastic pool would have grown for does not move it.
    let visit = Instant::now();
    wait_for_tcp(IDLE_POOL_EXPOSED).await?;
    println!(
        "first visitor on the established pool: {:?}",
        visit.elapsed()
    );
    let mut held = Vec::with_capacity(LOAD_STREAMS);
    for _ in 0..LOAD_STREAMS {
        held.push(TcpStream::connect(IDLE_POOL_EXPOSED).await?);
    }
    for conn in &mut held {
        conn.write_all(PING.as_bytes()).await?;
    }
    for conn in &mut held {
        let mut rd = [0u8; 4];
        conn.read_exact(&mut rd).await?;
        assert_eq!(&rd, PING.as_bytes(), "a visitor was lost under load");
    }
    settle(1.0).await;
    let pools = molehill_rathole::live_pools();
    let pool = pools
        .first()
        .unwrap_or_else(|| panic!("the pool exists once its service is up"));
    assert_eq!(
        pool.size, pool.count,
        "{LOAD_STREAMS} concurrent visitors must not move a pinned pool: {pool:?}"
    );
    assert_eq!(
        pool.grows, 0,
        "a pinned pool grows nothing, under load included: {pool:?}"
    );

    // Drop the visitors: the pool is idle, and there is no shrink clock any
    // more. The fixture used to set `idle_timeout = 2`; eight seconds of
    // silence is four times that and must still change nothing.
    drop(held);
    for _ in 0..8 {
        settle(1.0).await;
        let pools = molehill_rathole::live_pools();
        let pool = pools
            .first()
            .unwrap_or_else(|| panic!("the pool exists for its service's lifetime"));
        assert_eq!(
            pool.size, pool.count,
            "an idle pinned pool must not shrink: {pool:?}"
        );
        assert_eq!(
            pool.shrinks, 0,
            "a pinned pool has no shrink rule to fire: {pool:?}"
        );
    }

    // And it still forwards: the visitor after the idle stretch is served by
    // the same tunnels the pool started with.
    let visit = Instant::now();
    wait_for_tcp(IDLE_POOL_EXPOSED).await?;
    println!(
        "first visitor after the idle stretch: {:?}",
        visit.elapsed()
    );

    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}

/// The operator's valve (D14), end to end: the server's
/// `[server.data].max_tunnels_per_client = 1` refuses one of the two tunnels
/// the client's own count (`tunnels = 2`) asks for when the service starts, and
/// the refusal is non-fatal — the tunnel the client got keeps carrying its
/// visitors, the session is never closed, and the pool keeps trying for the
/// second.
///
/// The valve now bites at establishment rather than at a growth: with a pinned
/// pool there is no growth to refuse. What makes the pool's state the evidence
/// is the gap — `count = 2` (what the client asked for) against `size = 1`
/// (what the valve admitted) — and the fact that the pool stays there instead
/// of dialing its way to two.
#[tokio::test]
async fn the_server_tunnel_valve_refuses_a_startup_establishment() -> Result<()> {
    init();
    spawn_tcp_backends();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/tunnel_valve.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server("tests/for_tcp/tunnel_valve.toml", server_shutdown_rx)
            .await
            .unwrap();
    });

    // The client came up before the server, so the service is only exposed
    // once the control session has reconnected and registered it.
    wait_for_tcp(VALVE_EXPOSED).await?;

    let mut held = Vec::with_capacity(VALVE_LOAD);
    for _ in 0..VALVE_LOAD {
        held.push(TcpStream::connect(VALVE_EXPOSED).await?);
    }
    // Give the maintenance ticks time to try (and keep trying) for the tunnel
    // the client asked for: its own count allows a second, the server's valve
    // does not.
    settle(3.0).await;
    let pools = molehill_rathole::live_pools();
    let pool = pools
        .first()
        .unwrap_or_else(|| panic!("the valve scenario must have a live pool: {pools:?}"));
    assert_eq!(
        pool.count, 2,
        "the client's configured count is still what it wants: {pool:?}"
    );
    assert_eq!(
        pool.size, 1,
        "the server's valve must hold the pool at one tunnel: {pool:?}"
    );
    assert!(
        pool.size <= pool.count,
        "the valve may only ever leave the pool short of its count: {pool:?}"
    );
    assert_eq!(
        pool.grows, 0,
        "the refused establishment must not read as a growth: {pool:?}"
    );

    // The tunnel it holds still carries visitors: the valve is a refusal, not
    // a session teardown.
    let mut forwarded = 0;
    for _ in 0..20 {
        if wait_for_tcp(VALVE_EXPOSED).await.is_ok() {
            forwarded += 1;
        }
    }
    assert!(forwarded > 0, "the surviving tunnel must keep forwarding");
    drop(held);
    assert!(
        molehill_rathole::control_sessions_accepted() > 0,
        "the session must still be up after the refusal"
    );
    // The refusal's *report* — once per process, not once per retry — is
    // asserted in `a_refused_establishment_is_reported_once`, on a real binary:
    // "once per process" is only a well-defined claim when the process is the
    // scenario, and this process runs the whole file.

    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}

/// A pool the valve holds **below its count** must still never reach the
/// engine's stream cap (the regression the v0.10.0 sweep caught).
///
/// A stream past the engine's cap is refused on its own now, but the refusal
/// is a failed visitor and the vendored engine logs an unguarded `error!`
/// first — so the load that matters is the one the pool cannot relieve by
/// adding a tunnel. The server's valve (`max_tunnels_per_client = 1`) is
/// exactly that state: the client asked for two and holds one, and this
/// scenario puts more concurrent visitors on it than one tunnel may carry.
///
/// Before the ceiling existed this held 64 visitors and the server logged
/// `maximum number of streams reached`, killing the tunnel under them; the
/// assertion below is what fails if the ceiling is removed again.
#[tokio::test]
async fn a_valve_held_pool_still_never_reaches_the_stream_cap() -> Result<()> {
    init();
    spawn_tcp_backends();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/tunnel_valve.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server("tests/for_tcp/tunnel_valve.toml", server_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_tcp(VALVE_EXPOSED).await?;

    // Past the engine cap, with the pool pinned at one tunnel by the valve:
    // the only way to serve these is to leave the cap alone.
    let mut held = Vec::with_capacity(ENGINE_STREAM_CAP + 8);
    for _ in 0..(ENGINE_STREAM_CAP + 8) {
        held.push(TcpStream::connect(VALVE_EXPOSED).await?);
    }
    // The tunnel is still there: the visitors it turned away are the ones the
    // ceiling refused, not the ones a terminated tunnel dropped. A terminated
    // tunnel shows up as every visitor failing at once, so the round trips
    // below are the signal — some are answered, and the survivors of a
    // cap-hit would be none.
    for conn in &mut held {
        conn.write_all(PING.as_bytes()).await?;
    }
    // Three outcomes, and only the first is an answer: a shed visitor's socket
    // is closed by the server (`Ok(Err(_))` — the read *ends*, and its buffer
    // is still zero-initialized, which is why the read's own result has to be
    // checked rather than the timeout's: `timeout(..).is_ok()` is also true
    // for a read that failed, and a failed read of an untouched buffer would
    // otherwise read as garbage). A still-waiting visitor is `Elapsed`.
    let mut answered = 0;
    for conn in &mut held {
        let mut rd = [0u8; 4];
        // `Ok` here is anyhow's function (imported above), so the two layers
        // are spelled out: the timeout's, then the read's.
        match time::timeout(Duration::from_secs(2), conn.read_exact(&mut rd)).await {
            std::result::Result::Ok(std::result::Result::Ok(_)) => {
                assert_eq!(&rd, PING.as_bytes(), "a visitor was answered with garbage");
                answered += 1;
            }
            // Refused and shed: the server closed the socket, and the read
            // *ends* rather than failing the timeout. Its buffer was never
            // written, which is why the read's own result matters: checking
            // only the timeout's would read an untouched (zero) buffer as
            // an answer.
            std::result::Result::Ok(std::result::Result::Err(e)) => {
                eprintln!("valve: visitor connection ended: {e}");
            }
            // Refused, still inside its pairing budget.
            std::result::Result::Err(_) => {
                eprintln!("valve: visitor still unanswered after 2s");
            }
        }
    }
    assert!(
        answered > 0,
        "the tunnel must survive a burst the pool cannot add a tunnel for"
    );

    let pools = molehill_rathole::live_pools();
    let pool = pools
        .first()
        .unwrap_or_else(|| panic!("the valve scenario must have a live pool: {pools:?}"));
    assert_eq!(
        pool.count, 2,
        "the client's configured count is what the valve is refusing: {pool:?}"
    );
    assert_eq!(
        pool.size, 1,
        "the valve must still hold the pool at one tunnel"
    );
    assert!(
        pool.streams() <= ENGINE_STREAM_CAP,
        "the pool placed {} streams on {} tunnel(s): the engine's cap is {ENGINE_STREAM_CAP}",
        pool.streams(),
        pool.size
    );
    assert!(
        pool.tunnels
            .iter()
            .all(|(streams, _, _)| *streams <= POOL_STREAM_CEILING),
        "the placement ceiling must hold per tunnel: {pool:?}"
    );

    drop(held);
    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}

/// A service whose pool is saturated must still **recover**: once capacity
/// comes back, the next visitor is served.
///
/// This is the difference between "the burst was refused" (acceptable: a
/// visitor fails) and "the service stopped" (not acceptable: every later
/// visitor hangs). A shaped sweep measured the second one — after a 100 ms
/// path wedged the tunnels, the rest of the run showed `churn/s` collapsing
/// from 2398 to 11 and every interactive probe timing out, which is what a
/// service stuck behind one unanswerable visitor looks like (HANDOFF.md).
///
/// The scenario drives the pool to its ceiling with held visitors, drops them
/// to return the capacity, and then asks the only question that matters: does
/// a fresh visitor get served?
#[tokio::test]
async fn a_saturated_pool_still_serves_the_next_visitor() -> Result<()> {
    init();
    spawn_tcp_backends();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/tunnel_valve.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server("tests/for_tcp/tunnel_valve.toml", server_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_tcp(VALVE_EXPOSED).await?;

    // Saturate the pool: more concurrent visitors than its one tunnel may
    // carry, so the tail of the burst meets the placement ceiling (the valve
    // holds the pool at one tunnel, so it cannot add another).
    let mut held = Vec::with_capacity(ENGINE_STREAM_CAP + 8);
    for _ in 0..(ENGINE_STREAM_CAP + 8) {
        held.push(TcpStream::connect(VALVE_EXPOSED).await?);
    }
    for conn in &mut held {
        let _ = conn.write_all(PING.as_bytes()).await;
    }
    settle(2.0).await;

    // Return the capacity: every held visitor goes away, so the tunnel is
    // empty again.
    drop(held);
    settle(2.0).await;

    let pools = molehill_rathole::live_pools();
    if let Some(pool) = pools.first() {
        println!("after the burst: {pool:?}");
    }

    // The question: a visitor arriving now must be served, not left hanging
    // behind a request the pool already refused.
    let mut fresh = time::timeout(Duration::from_secs(10), TcpStream::connect(VALVE_EXPOSED))
        .await
        .map_err(|_| anyhow!("a fresh visitor must be accepted"))??;
    fresh.write_all(PING.as_bytes()).await?;
    let mut rd = [0u8; 4];
    time::timeout(Duration::from_secs(10), fresh.read_exact(&mut rd))
        .await
        .map_err(|_| anyhow!("a fresh visitor must be served after the pool emptied"))??;
    assert_eq!(
        &rd,
        PING.as_bytes(),
        "the fresh visitor was answered with garbage"
    );

    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}

/// Echo server that records the source address of every datagram it receives.
async fn sticky_echo_server(seen_srcs: Arc<Mutex<HashSet<SocketAddr>>>) -> Result<()> {
    let l = UdpSocket::bind(UDP_POOL_LOCAL).await?;
    let mut buf = [0u8; 2048];
    loop {
        let (n, from) = l.recv_from(&mut buf).await?;
        seen_srcs.lock().unwrap().insert(from);
        l.send_to(&buf[..n], from).await?;
    }
}

/// The S1 observation, on the **real binary**: with `MOLEHILL_POOL_STATS=1`
/// and `MOLEHILL_PLACEMENT_STATS=1`, a healthy run's output carries the pool
/// timeline and the aggregated placement line. Both are opt-in, so the same
/// run without them stays silent — which the second half of the test checks.
/// Gated on `native_target`: this is the one scenario here that runs the
/// compiled binary as a child process, and a child cannot be exec'd from
/// inside an emulated test binary (the release workflow's `cross` targets).
/// The other scenarios drive the pool in-process and run everywhere.
#[cfg(native_target)]
#[tokio::test]
async fn the_pool_and_placement_lines_are_opt_in() -> Result<()> {
    // The real binary forwards to the same in-process echo backend the other
    // scenarios use.
    spawn_tcp_backends();
    let (with_stats, pool_key) = run_binary_pair(true, 3, None).await?;
    for needle in [
        // The pool's timeline: its identity, size, count, per-tunnel
        // `streams/pending/pinned`, and the reason for each change.
        "pool-stats: tunnel pool timeline",
        // The pool's key: its owner, carrier and data endpoint.
        &pool_key,
        "tunnels=",
        // The per-placement aggregate, INFO (the default level prints it).
        "placement-stats: aggregate of the last interval",
    ] {
        assert!(
            with_stats.contains(needle),
            "the opt-in run must emit {needle:?}; client log:\n{with_stats}"
        );
    }
    // The run's pool is at its configured count from the first visitor on:
    // `count` is what the operator asked for, `size` what is up.
    assert!(
        with_stats.contains("size=3") && with_stats.contains("count=3"),
        "the line must carry the pool's size and its configured count:\n{with_stats}"
    );
    // Establishment is a lifecycle line, not an observation; a healthy pinned
    // run establishes and then leaves the pool alone.
    assert!(
        with_stats.contains("pool-stats: tunnel pool established"),
        "the pool's establishment must be logged:\n{with_stats}"
    );
    assert!(
        !with_stats.contains("pool-stats: tunnel pool repaired"),
        "a healthy pinned run has nothing to repair:\n{with_stats}"
    );

    let (without, _) = run_binary_pair(false, 3, None).await?;
    // The needles are the message text, not the switch names: the run's own
    // temporary directory is named after the switch. The two *observation*
    // lines — the per-second timeline and the per-second placement aggregate —
    // are opt-in. Establishment is not an observation but a lifecycle event
    // (one line per pool, whatever the switch), and it is now the only one a
    // healthy run emits: the growth line an elastic pool printed on its first
    // visitor is gone, because a pinned pool dials nothing until a tunnel dies.
    for needle in [
        "pool-stats: tunnel pool timeline",
        "placement-stats: aggregate of the last interval",
    ] {
        assert!(
            !without.contains(needle),
            "the instrumentation is opt-in, so {needle:?} must not appear without it:\n{without}"
        );
    }
    assert!(
        without.contains("pool-stats: tunnel pool established"),
        "a pool's establishment is a lifecycle line, switch or not:\n{without}"
    );
    assert!(
        !without.contains("pool-stats: tunnel pool repaired"),
        "a healthy pinned pool grows nothing; a repair line would mean a tunnel died:\n{without}"
    );
    Ok(())
}

/// The valve's refusal is a **reported** condition, and reported once per
/// process: the pool retries the establishment its count asks for on every
/// maintenance tick, and an operator who set the valve wants one line, not one
/// per retry.
///
/// Measured on the real binary, in a fresh process. The in-process scenarios in
/// this file share one process — and one `RepeatNotice` — so "once per process"
/// is only a well-defined claim when the process *is* the scenario.
#[cfg(native_target)]
#[tokio::test]
async fn a_refused_establishment_is_reported_once() -> Result<()> {
    // The real binary forwards to the same in-process echo backend the other
    // scenarios use.
    spawn_tcp_backends();
    // The client asks for two tunnels, the server's valve admits one.
    let (log, _) = run_binary_pair(true, 2, Some(1)).await?;
    assert_eq!(
        log.matches("pool-stats: growth refused").count(),
        1,
        "the refused establishment must be reported once, not once per retry:\n{log}"
    );
    assert!(
        log.contains("count=2") && log.contains("size=1"),
        "the pool's line must show the client's count against the server's valve:\n{log}"
    );
    assert!(
        !log.contains("pool-stats: tunnel pool repaired"),
        "a refused establishment is not a repair:\n{log}"
    );
    Ok(())
}

/// Stop a child: `SIGINT` (the signal the binary turns into a clean exit),
/// then `SIGKILL` if it is still alive. A leaked child would hold the
/// scenario's ports for the whole test session.
#[cfg(native_target)]
fn stop(child: &mut std::process::Child) {
    use std::process::Command;
    let _ = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status();
    for _ in 0..50 {
        if let std::io::Result::Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Run the compiled binary as a server+client pair over its own ports, force a
/// visitor through it, and return everything the client printed.
///
/// `tunnels` is the client's `[client.data.tcp].tunnels`; `valve` is the
/// server's `[server.data].max_tunnels_per_client` when the scenario wants the
/// valve to bite. The pairing is what lets one runner measure both the healthy
/// establishment and the refused one.
#[cfg(native_target)]
async fn run_binary_pair(
    stats: bool,
    tunnels: u16,
    valve: Option<u16>,
) -> Result<(String, String)> {
    use std::process::{Command, Stdio};

    let ([control_port, exposed_port], held) = free_ports::<2>();
    let control = format!("127.0.0.1:{control_port}");
    let exposed = format!("127.0.0.1:{exposed_port}");
    let pool_key = format!("session/tcp:{control}");
    let valve_block = valve.map_or_else(String::new, |n| {
        format!("\n[server.data]\nmax_tunnels_per_client = {n}\n")
    });

    let dir = std::env::temp_dir().join(format!(
        "molehill-pool-stats-{}-{stats}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let server_cfg = dir.join("server.toml");
    let client_cfg = dir.join("client.toml");
    std::fs::write(
        &server_cfg,
        format!(
            "[server]\n\
             default_token = \"pool_stats_token\"\n\
             allow_ports = [\"{exposed_port}\"]\n\
             \n\
             [server.control]\n\
             bind_addr = \"{control}\"\n\
             {valve_block}"
        ),
    )?;
    std::fs::write(
        &client_cfg,
        format!(
            "[client]\n\
             default_token = \"pool_stats_token\"\n\
             \n\
             [client.control]\n\
             default_remote_addr = \"{control}\"\n\
             \n\
             [client.transport]\n\
             type = \"plain\"\n\
             \n\
             [client.data]\n\
             shared_pool = true\n\
             \n\
             [client.data.tcp]\n\
             tunnels = {tunnels}\n\
             \n\
             [client.services.echo]\n\
             local_addr = \"{STATS_BACKEND}\"\n\
             remote_bind_addr = \"{exposed}\"\n"
        ),
    )?;

    // The reservation held the ports; the children need them now, so it is
    // released first. A run that loses the race fails the readiness probe
    // below, and the scenario simply asks for another pair.
    drop(held);
    let bin = env!("CARGO_BIN_EXE_molehill");
    let log = dir.join("client.log");
    let mut server = Command::new(bin)
        .arg("--server")
        .arg(&server_cfg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // The server's control listener first: the client is started second, so
    // the run has no "connection refused, retrying" in it.
    for _ in 0..300 {
        if TcpStream::connect(&control).await.is_ok() {
            break;
        }
        time::sleep(Duration::from_millis(50)).await;
    }
    let out = std::fs::File::create(&log)?;
    let err = out.try_clone()?;
    let mut command = Command::new(bin);
    command
        .arg("--client")
        .arg(&client_cfg)
        .env("RUST_LOG", "debug");
    if stats {
        command
            .env("MOLEHILL_POOL_STATS", "1")
            .env("MOLEHILL_PLACEMENT_STATS", "1");
    }
    let mut client = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()?;

    // Two visitors, then enough time for the 1 Hz reporters to emit.
    wait_for_tcp(&exposed).await?;
    let _ = probe_tcp(&exposed).await;
    time::sleep(Duration::from_millis(2500)).await;

    stop(&mut client);
    stop(&mut server);
    let log = std::fs::read_to_string(&log).unwrap_or_default();
    // Keep the scenario's files when the caller asks, so a failure can be
    // diagnosed from the raw log (the test removes the directory otherwise).
    if std::env::var_os("MOLEHILL_KEEP_POOL_LOGS").is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok((log, pool_key))
}

/// One unanswerable visitor must not park the accept loop behind its budget.
///
/// Pairing is per visitor (`MAX_CONCURRENT_VISITORS` in flight), so three
/// visitors the client cannot answer are shed *together*, when their own
/// budgets run out. The old accept loop paired one visitor at a time: the
/// first held it for its whole budget, the second was not even accepted until
/// the first was shed, so the k-th unanswerable visitor waited `k ×` the
/// budget — the service really was parked behind one visitor.
///
/// The assertion is the timing: every connection ends within one budget of the
/// first, and none of them waits for another visitor's budget to elapse.
#[tokio::test]
async fn one_unanswerable_visitor_does_not_park_the_service() -> Result<()> {
    init();
    spawn_tcp_backends();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/tunnel_valve.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server("tests/for_tcp/tunnel_valve.toml", server_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_tcp(VALVE_EXPOSED).await?;
    // Hold the pool at its placement ceiling, so the visitors below have
    // nothing to be paired with (the valve holds the pool at one tunnel, so
    // there is no second one to place them on).
    let mut held = Vec::with_capacity(56);
    for _ in 0..56 {
        held.push(TcpStream::connect(VALVE_EXPOSED).await?);
    }
    for conn in &mut held {
        conn.write_all(PING.as_bytes()).await?;
    }
    let mut answered_held = 0;
    for conn in &mut held {
        let mut rd = [0u8; 4];
        if matches!(
            time::timeout(Duration::from_secs(5), conn.read_exact(&mut rd)).await,
            std::result::Result::Ok(std::result::Result::Ok(_))
        ) {
            assert_eq!(&rd, PING.as_bytes(), "a visitor was answered with garbage");
            answered_held += 1;
        }
    }
    assert_eq!(
        answered_held, 56,
        "the ceiling visitors (TUNNEL_STREAM_CEILING) must all be served          before the pool is full"
    );

    // Three visitors the client cannot answer, each waiting for an echo that
    // will never come until its own budget expires and it is shed.
    let mut waiting = Vec::with_capacity(3);
    for _ in 0..3 {
        let mut conn = TcpStream::connect(VALVE_EXPOSED).await?;
        conn.write_all(PING.as_bytes()).await?;
        waiting.push(conn);
    }
    settle(1.0).await;

    // A shed visitor is a closed socket: the read *ends* rather than timing
    // out, which is the signal this records (an unanswered visitor inside its
    // budget still hangs instead).
    let t0 = Instant::now();
    let mut shed_after: Vec<f64> = Vec::with_capacity(3);
    for conn in &mut waiting {
        let mut rd = [0u8; 4];
        let ended = matches!(
            time::timeout(Duration::from_secs(40), conn.read_exact(&mut rd)).await,
            std::result::Result::Ok(std::result::Result::Err(_))
        );
        shed_after.push(t0.elapsed().as_secs_f64());
        assert!(
            ended,
            "an unanswerable visitor must be shed (closed), not left hanging"
        );
    }
    // The pairing budget is five attempts of five seconds. Three visitors shed
    // by the same rule land within a few seconds of each other; one visitor
    // parked behind another's budget lands 25 s apart.
    let budget = Duration::from_secs(25).as_secs_f64();
    for (i, at) in shed_after.iter().enumerate() {
        assert!(
            *at <= budget + 10.0,
            "visitor {i} waited {at:.1}s to be shed: the accept loop was parked \
             behind an earlier visitor's budget (the serial loop would shed \
             them {budget:.0}s apart)"
        );
    }

    drop(held);
    drop(waiting);
    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}
