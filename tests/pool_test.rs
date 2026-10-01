//! The shared elastic pool (M2a): one pool per `(session, carrier)` when
//! `[client.data].shared_pool` is on, per service when it is off, and the S1
//! observation that makes the policy falsifiable.
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
//! reads what the operator would see.
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

/// Enough load to make the valve scenario's pool want a second tunnel while
/// leaving the one it holds room to keep working. Holding the full
/// `GROW_STREAMS` there would also put the single tunnel at its placement
/// ceiling, and the visitors it then carries would be refused for capacity
/// rather than forwarded — a different scenario, covered by
/// `a_refused_growth_still_never_reaches_the_stream_cap`.
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
    assert!(
        pool.size <= pool.max_tunnels,
        "a shared pool must stay within its cap: {pool:?}"
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

/// The UDP invariant the elastic pool has to hold (D30): the pool grows for
/// the service's channels and shrinks again once a tunnel is idle, while the
/// visitor's peer keeps the exact source address the local service sees. A
/// shrink that removed the tunnel carrying the peer would drop its local
/// socket and change the source port.
#[tokio::test]
async fn udp_source_port_survives_a_grow_and_shrink_cycle() -> Result<()> {
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

    // The pool warms to its UDP-derived floor (the service's two workers ask
    // for two tunnels, and `max_tunnels = 3` leaves room above it), and the
    // visitor's peer is pinned to the tunnel its channel lives on.
    let mut grew = false;
    let mut pinned_seen = false;
    for _ in 0..60 {
        probe_udp(&conn).await?;
        settle(0.4).await;
        let pools = molehill_rathole::live_pools();
        let Some(pool) = pools.first() else {
            panic!("the UDP service's shared pool must exist: {pools:?}");
        };
        if pool.size >= 2 {
            grew = true;
        }
        if pool.pinned() > 0 {
            pinned_seen = true;
        }
        if grew && pinned_seen {
            break;
        }
    }
    // Keep the peer talking across the window in which the pool would shrink
    // if it did not see the pin: the pinned tunnel must stay.
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
        assert!(
            pool.size >= 2,
            "the pool must not shrink below the floor its workers need: {pool:?}"
        );
    }
    let pools = molehill_rathole::live_pools();
    assert!(
        grew,
        "the pool must grow to the service's UDP-derived floor: {pools:?}"
    );
    assert!(
        pinned_seen,
        "the visitor's peer must be pinned to a tunnel: {pools:?}"
    );

    let (src_count, srcs) = {
        let seen = seen_srcs.lock().unwrap();
        (seen.len(), seen.clone())
    };
    assert_eq!(
        src_count, 1,
        "the peer's session was split across source ports across a grow/shrink cycle: {srcs:?}"
    );

    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(server, client);
    Ok(())
}

/// Concurrent visitors one load test holds at once: past the per-tunnel
/// growth threshold (7 on a 64-stream cap) and at the placement ceiling
/// (56), which is the shape the growth rule exists for.
const GROW_STREAMS: usize = 56;

/// The engine's per-tunnel stream cap (`DEFAULT_MUX_MAX_STREAMS`), repeated
/// here because the assertions below are about crossing it.
const ENGINE_STREAM_CAP: usize = 64;

/// The pool's own per-tunnel placement ceiling
/// (`transport::pool::TUNNEL_STREAM_CEILING`), repeated for the same reason:
/// the burst scenarios drive the pool up to it, and the two are deliberately
/// different numbers.
const POOL_STREAM_CEILING: usize = 56;

/// A burst past the engine's stream cap must never cost a tunnel (D14's
/// hard half).
///
/// A 65th inbound stream is answered with a reset of that one stream — the
/// tunnel survives, where a session-terminating goaway used to take the
/// whole connection and every visitor on it down — and the vendored engine
/// logs an unguarded `error!` first. The pool's job is to keep even that
/// unreachable — growth keeps every tunnel strictly below the cap — so this
/// scenario holds more concurrent visitors than one tunnel may carry and
/// asserts that the engine's cap is never reached, that no visitor is
/// dropped, and that the pool grew to take the load.
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

    // More concurrent visitors than one tunnel may carry, but no more than the
    // pool's own cap can hold (two tunnels): the scenario is "the pool serves
    // the burst by spreading it", not "the pool refuses most of it". Every one
    // of these must therefore be forwarded.
    let burst = POOL_STREAM_CEILING * 2;
    let mut held = Vec::with_capacity(burst);
    for _ in 0..burst {
        held.push(TcpStream::connect(IDLE_POOL_EXPOSED).await?);
    }
    // Every visitor still answers: a terminated tunnel would have dropped
    // them all at once, and the survivors would be the ones opened after it.
    for conn in &mut held {
        conn.write_all(PING.as_bytes()).await?;
    }
    for conn in &mut held {
        let mut rd = [0u8; 4];
        conn.read_exact(&mut rd).await?;
        assert_eq!(&rd, PING.as_bytes(), "a visitor lost its tunnel to the cap");
    }

    let pools = molehill_rathole::live_pools();
    let pool = pools
        .first()
        .unwrap_or_else(|| panic!("the pool exists once its service is up"));
    assert!(
        pool.size >= 2,
        "{} concurrent visitors must have grown the pool past one tunnel: {pool:?}",
        held.len()
    );

    drop(held);
    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}

/// The cold start and the shrink rule, measured, in one scenario.
///
/// There is no initial pool size any more (`default_count` / `count` are
/// gone), so the sequence a TCP service actually lives through is:
///
/// 1. the pool exists but is **cold** — size 0, nothing to carry;
/// 2. its first visitor grows it synchronously (the setup cost the M2a
///    measurement records, printed here);
/// 3. load above the growth threshold (12 % of a tunnel's stream capacity)
///    grows it again, up to `max_tunnels`;
/// 4. once nothing is carried for `idle_timeout`, the shrink rule removes one
///    tunnel — down to the floor of one, never to zero while the service is
///    registered.
#[tokio::test]
async fn a_cold_pool_grows_under_load_and_shrinks_when_idle() -> Result<()> {
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

    // The pool starts cold and stays cold: no service has opened a channel,
    // so there is no demand for a tunnel.
    let mut saw_empty = false;
    for _ in 0..10 {
        settle(0.2).await;
        let pools = molehill_rathole::live_pools();
        let Some(pool) = pools.first() else { continue };
        assert_eq!(
            pool.size, 0,
            "a cold pool must not hold a tunnel before anything asks: {pool:?}"
        );
        saw_empty = true;
    }
    assert!(saw_empty, "the pool must exist once its service is active");

    // The first visitor grows it synchronously: this is what a reader pays
    // for a cold pool (M2a: 2.0-3.2 ms on loopback for the tunnel setup).
    let visit = Instant::now();
    wait_for_tcp(IDLE_POOL_EXPOSED).await?;
    println!("first visitor on the cold pool: {:?}", visit.elapsed());
    let mut grew_for_cold_start = false;
    for _ in 0..50 {
        settle(0.2).await;
        if let Some(pool) = molehill_rathole::live_pools().first()
            && pool.size >= 1
        {
            grew_for_cold_start = true;
            break;
        }
    }
    let pools = molehill_rathole::live_pools();
    assert!(
        grew_for_cold_start,
        "the first open must grow the cold pool: {pools:?}"
    );

    // The load rule: hold enough concurrent visitors that the one tunnel is
    // above the growth threshold (12 % of its stream capacity), and the tick
    // adds the second (its cap).
    let startup = Instant::now();
    let mut held = Vec::with_capacity(GROW_STREAMS);
    for _ in 0..GROW_STREAMS {
        held.push(TcpStream::connect(IDLE_POOL_EXPOSED).await?);
    }
    let mut reached_two = None;
    for _ in 0..100 {
        settle(0.2).await;
        if let Some(pool) = molehill_rathole::live_pools().first()
            && pool.size == 2
        {
            reached_two = Some(startup.elapsed());
            break;
        }
    }
    let pools = molehill_rathole::live_pools();
    let grew_at = reached_two.unwrap_or_else(|| {
        panic!("{GROW_STREAMS} concurrent streams must grow the pool to its cap: {pools:?}")
    });
    assert!(
        grew_at >= Duration::from_millis(50),
        "growth is the maintenance tick's business, not the open's: {grew_at:?}"
    );

    // Drop the visitors: the pool is idle again, and after `idle_timeout`
    // the shrink rule removes one tunnel.
    drop(held);
    let mut shrunk_at = None;
    for _ in 0..120 {
        settle(0.2).await;
        if let Some(pool) = molehill_rathole::live_pools().first()
            && pool.shrinks >= 1
        {
            shrunk_at = Some(startup.elapsed());
            break;
        }
    }
    let pools = molehill_rathole::live_pools();
    let shrunk_at = shrunk_at
        .unwrap_or_else(|| panic!("the idle pool never shrank; idle_timeout was 2 s: {pools:?}"));
    let after = &pools[0];
    assert_eq!(
        after.size, 1,
        "an idle pool shrinks to its floor (one tunnel), never to zero: {after:?}"
    );
    assert!(
        shrunk_at >= Duration::from_secs(2),
        "the shrink must wait out `idle_timeout` (2 s), it happened after {shrunk_at:?}"
    );

    // And the pool that shrank still forwards: the first visitor after the
    // shrink is served by the surviving tunnel.
    let visit = Instant::now();
    wait_for_tcp(IDLE_POOL_EXPOSED).await?;
    println!("first visitor after the pool shrank: {:?}", visit.elapsed());

    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}

/// The operator's valve (D14), end to end: the server's
/// `[server.data].max_tunnels_per_client = 1` refuses the second tunnel the
/// client's own cap (`max_tunnels = 2`) would allow, and the refusal is
/// non-fatal — the tunnel the client already holds keeps carrying its
/// visitors, and the session is never closed.
#[tokio::test]
async fn the_server_tunnel_valve_refuses_growth_without_killing_the_session() -> Result<()> {
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
    // Give the maintenance ticks time to try (and keep trying) to grow: the
    // client's cap would allow a second tunnel, the server's does not.
    settle(3.0).await;
    let pools = molehill_rathole::live_pools();
    let pool = pools
        .first()
        .unwrap_or_else(|| panic!("the valve scenario must have a live pool: {pools:?}"));
    assert_eq!(
        pool.size, 1,
        "the server's valve must hold the pool at one tunnel: {pool:?}"
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

    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}

/// A pool that **cannot grow** must still never reach the engine's stream cap
/// (the regression the v0.10.0 sweep caught).
///
/// A stream past the engine's cap is refused on its own now, but the refusal
/// is a failed visitor and the vendored engine logs an unguarded `error!`
/// first — so the load that matters is the one where growth is *refused* and
/// cannot relieve the pressure. The server's valve
/// (`max_tunnels_per_client = 1`) is exactly that state, and this scenario
/// holds more concurrent visitors than one tunnel may carry.
///
/// Before the ceiling existed this held 64 visitors and the server logged
/// `maximum number of streams reached`, killing the tunnel under them; the
/// assertion below is what fails if the ceiling is removed again.
#[tokio::test]
async fn a_refused_growth_still_never_reaches_the_stream_cap() -> Result<()> {
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
        "the tunnel must survive a burst its pool cannot grow for"
    );

    let pools = molehill_rathole::live_pools();
    let pool = pools
        .first()
        .unwrap_or_else(|| panic!("the valve scenario must have a live pool: {pools:?}"));
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
    // carry, so the tail of the burst meets the placement ceiling.
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
    let (with_stats, pool_key) = run_binary_pair(true).await?;
    for needle in [
        // The pool's timeline: its identity, size, per-tunnel
        // `streams/pending/pinned`, and the reason for each size change.
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
    // The run's own pool starts cold and grows to one tunnel for its first
    // visitor, so its line is the steady-state timeline by then; the growth
    // entry itself is asserted on directly by the tests above.
    assert!(
        with_stats.contains("size=1") && with_stats.contains("max_tunnels=3"),
        "the line must carry the pool's size and its cap:\n{with_stats}"
    );

    let (without, _) = run_binary_pair(false).await?;
    // The needles are the message text, not the switch names: the run's own
    // temporary directory is named after the switch. The two *observation*
    // lines — the per-second timeline and the per-second placement aggregate —
    // are opt-in. A size change is not an observation but a lifecycle event
    // (one line per change, whatever the switch), and with the pool starting
    // cold the run below grows it on its first visitor, so that line is
    // expected here — pinned, so a silent pool would be noticed.
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
        without.contains("pool-stats: tunnel pool grew"),
        "a cold pool logs its first growth as a lifecycle line, switch or not:\n{without}"
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
#[cfg(native_target)]
async fn run_binary_pair(stats: bool) -> Result<(String, String)> {
    use std::process::{Command, Stdio};

    let ([control_port, exposed_port], held) = free_ports::<2>();
    let control = format!("127.0.0.1:{control_port}");
    let exposed = format!("127.0.0.1:{exposed_port}");
    let pool_key = format!("session/tcp:{control}");

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
             bind_addr = \"{control}\"\n"
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
             default_mode = \"multiplex\"\n\
             shared_pool = true\n\
             idle_timeout = 2\n\
             \n\
             [client.data.tcp]\n\
             max_tunnels = 3\n\
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
    // nothing to be paired with and the valve keeps the pool from growing.
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
