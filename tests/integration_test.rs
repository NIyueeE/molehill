//! End-to-end integration tests: real server/client pairs over every
//! transport, plus the UDP session-affinity regression scenario.
//!
//! Run serially (`--test-threads=1`): the scenarios bind fixed ports.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "integration tests unwrap and assert on values they just produced"
)]

use anyhow::{Context, Ok, Result};
use common::{PING, PONG, run_molehill_client};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::broadcast,
    time,
};
use tracing::{debug, info, instrument};
use tracing_subscriber::EnvFilter;

use crate::common::run_molehill_server;

use std::path::PathBuf;

#[cfg(feature = "multiplex")]
use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

mod common;

const ECHO_SERVER_ADDR: &str = "127.0.0.1:8080";
const PINGPONG_SERVER_ADDR: &str = "127.0.0.1:8081";
const HITTER_NUM: usize = 4;

// Ports for the UDP session-affinity regression test (`udp_session_affinity`).
const AFFINITY_LOCAL_SERVICE: &str = "127.0.0.1:8082";
const AFFINITY_EXPOSED_ADDR: &str = "127.0.0.1:2340";
const AFFINITY_PACKETS: usize = 64;

// Ports for the two UDP-knob scenarios (`tests/for_udp/knob_effects.toml`):
// the fixture owns control 2352 and the exposed 2353/2354, with its backends
// on 8102/8103. The block 2352-2359 is free in the suite (2333-2351 belong to
// the transport fixtures, 2360-2366 to the session tests).
const UDP_KNOBS_CONFIG: &str = "tests/for_udp/knob_effects.toml";
const KNOB_SMALL_LOCAL: &str = "127.0.0.1:8102";
const KNOB_IDLE_LOCAL: &str = "127.0.0.1:8103";
const KNOB_SMALL_EXPOSED: &str = "127.0.0.1:2353";
const KNOB_IDLE_EXPOSED: &str = "127.0.0.1:2354";
/// The fixture's `udp_buffer_size`; the assertions below are written in terms
/// of it so a changed fixture fails loudly instead of drifting.
const KNOB_BUFFER_SIZE: usize = 1024;
/// The fixture's `udp_idle_timeout`, and the silence the test waits out before
/// expecting the mapping (and its local socket) to be gone.
const KNOB_IDLE_TIMEOUT_SECS: u64 = 2;
const KNOB_IDLE_SILENCE: f64 = 3.5;
/// Datagrams sent inside one idle window before the silence starts.
const KNOB_IDLE_BURST: usize = 4;

// Ports for the transparent-visibility regression
// (`dead_backend_fails_one_visitor_and_stays_registered`): one service whose
// backend is not running, and one healthy service on the same client.
const DEAD_BACKEND: &str = "127.0.0.1:8099";
const DEAD_EXPOSED: &str = "127.0.0.1:2350";
const DEAD_NEIGHBOUR_EXPOSED: &str = "127.0.0.1:2351";

// Ports for the session-scoped scenarios (`one_control_session_*`,
// `a_rejected_service_*`, `a_foreign_service_token_*`,
// `a_timeout_below_the_heartbeat_floor_*`). Each fixture owns a control port
// and two exposed ones: the fixtures above own 2333-2351, `session_test` owns
// 2360-2364, and these are the free block between them.
const SESSION_REJECT_OK: &str = "127.0.0.1:2373";
const SESSION_REJECT_BAD: &str = "127.0.0.1:2374";
const SESSION_TOKEN_OK: &str = "127.0.0.1:2376";
const SESSION_TOKEN_BAD: &str = "127.0.0.1:2377";
const SESSION_HEARTBEAT_ADDR: &str = "127.0.0.1:2378";
const SESSION_HEARTBEAT_EXPOSED: &str = "127.0.0.1:2379";

#[cfg(feature = "multiplex")]
static MUX_CONFIG_SEQ: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug)]
enum Type {
    Tcp,
    Udp,
}

// The tcp and udp tests run in parallel and must not share exposed ports
fn exposed_addrs(t: Type) -> (&'static str, &'static str) {
    match t {
        Type::Tcp => ("127.0.0.1:2334", "127.0.0.1:2335"),
        Type::Udp => ("127.0.0.1:2336", "127.0.0.1:2337"),
    }
}

fn init() {
    let level = "info";
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::from(level)),
        )
        .try_init();
}

/// Spawn the TCP echo + pingpong backend servers every TCP scenario needs;
/// the tasks run until the test process exits.
fn spawn_tcp_backends() {
    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(ECHO_SERVER_ADDR).await {
            panic!("Failed to run the echo server for testing: {e:?}");
        }
    });
    tokio::spawn(async move {
        if let Err(e) = common::tcp::pingpong_server(PINGPONG_SERVER_ADDR).await {
            panic!("Failed to run the pingpong server for testing: {e:?}");
        }
    });
}

/// Spawn the UDP echo backend server the UDP scenarios need.
fn spawn_udp_backends() {
    tokio::spawn(async move {
        if let Err(e) = common::udp::echo_server(ECHO_SERVER_ADDR).await {
            panic!("Failed to run the echo server for testing: {e:?}");
        }
    });
}

/// One round-trip with a 500 ms cap: the readiness probe the test
/// lifecycle waits on (registration + first data channel) instead of
/// guessing at startup timing with fixed sleeps. A half-up service (port
/// bound, channels not yet up) fails the probe and is polled again. The
/// payload is "ping": the echo service returns it verbatim, the pingpong
/// service answers "pong" — either reply proves the path works.
async fn probe_echo(addr: &'static str, t: Type) -> Result<()> {
    let attempt = async {
        let reply = match t {
            Type::Tcp => {
                let mut conn = TcpStream::connect(addr).await?;
                conn.write_all(PING.as_bytes()).await?;
                let mut rd = [0u8; 4];
                conn.read_exact(&mut rd).await?;
                rd
            }
            Type::Udp => {
                let conn = UdpSocket::bind("127.0.0.1:0").await?;
                conn.connect(addr).await?;
                conn.send(PING.as_bytes()).await?;
                let mut rd = [0u8; 4];
                conn.recv(&mut rd).await?;
                rd
            }
        };
        if reply != *PING.as_bytes() && reply != *PONG.as_bytes() {
            anyhow::bail!("unexpected reply in readiness probe");
        }
        Ok(())
    };
    time::timeout(Duration::from_millis(500), attempt).await??;
    Ok(())
}

/// One round-trip on an existing UDP socket: the readiness probe for
/// scenarios that must keep a single source port (session affinity) — a
/// dedicated probe socket would add a second source port and trip the very
/// invariant the test asserts.
async fn probe_udp_socket(conn: &UdpSocket) -> Result<()> {
    let attempt = async {
        conn.send(PING.as_bytes()).await?;
        let mut rd = [0u8; 4];
        conn.recv(&mut rd).await?;
        if rd != *PING.as_bytes() && rd != *PONG.as_bytes() {
            anyhow::bail!("unexpected reply in readiness probe");
        }
        Ok(())
    };
    time::timeout(Duration::from_millis(500), attempt).await??;
    Ok(())
}

/// Poll `probe_udp_socket` until the service answers or the deadline passes.
async fn wait_for_udp_socket(conn: &UdpSocket) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if probe_udp_socket(conn).await.is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("the UDP service did not become ready within 15 s");
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll `probe_echo` until the exposed service answers or the deadline
/// passes (the molehill pair failed to come up).
async fn wait_for_echo(addr: &'static str, t: Type) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if probe_echo(addr, t).await.is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("the exposed service at {addr} did not become ready within 15 s");
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

/// Pace the retry-path scenarios (e.g. the client reconnecting while the
/// server is still down). Correctness never depends on these sleeps: the
/// readiness polls above carry it.
async fn settle(secs: f64) {
    time::sleep(Duration::from_secs_f64(secs)).await;
}

/// Per-run overrides of `[client.data]` fields, materialized into a temp copy
/// of a fixture.
#[cfg(feature = "multiplex")]
#[derive(Debug, Default)]
struct ClientOverrides {
    /// `[client.data.tcp].tunnels`: the pinned pool's count — the tunnels it
    /// establishes at service start and keeps, not a cap it grows to.
    tunnels: Option<u16>,
}

/// Placeholder so `test()` keeps its signature without the `multiplex`
/// feature (callers only ever pass `None` there).
#[cfg(not(feature = "multiplex"))]
#[derive(Debug)]
struct ClientOverrides;

/// Materialize a copy of `config_path` with the requested `[client.data]`
/// fields applied.
///
/// Fixtures intentionally omit `[client.data]` so they follow the compiled-in
/// defaults (`tunnels = 4`, with the `multiplex` feature). Explicit copies are
/// what give the integration matrix its wider-count legs. The copy lives in the
/// system temp dir and is removed after the scenario.
#[cfg(feature = "multiplex")]
fn write_client_variant(config_path: &str, overrides: &ClientOverrides) -> Result<PathBuf> {
    let source = Path::new(config_path);
    let contents = fs::read_to_string(source)?;
    let mut doc: toml::Value = toml::from_str(&contents)?;
    let client = doc
        .get_mut("client")
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| anyhow::anyhow!("Test fixture {config_path} has no [client] table"))?;
    if overrides.tunnels.is_some() {
        if !client.contains_key("data") {
            client.insert("data".to_owned(), toml::Value::Table(toml::map::Map::new()));
        }
        let data = client
            .get_mut("data")
            .and_then(toml::Value::as_table_mut)
            .ok_or_else(|| {
                anyhow::anyhow!("Test fixture {config_path} has a non-table [client.data]")
            })?;
        if let Some(count) = overrides.tunnels {
            let tcp = data
                .entry("tcp".to_owned())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
                .as_table_mut()
                .ok_or_else(|| {
                    anyhow::anyhow!("Test fixture {config_path} has a non-table [client.data.tcp]")
                })?;
            tcp.insert("tunnels".to_owned(), toml::Value::Integer(i64::from(count)));
        }
    }

    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("integration");
    let variant = std::env::temp_dir().join(format!(
        "molehill_it_{stem}_{}_{}.toml",
        std::process::id(),
        MUX_CONFIG_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&variant, toml::to_string(&doc)?)?;
    Ok(variant)
}

/// Run one transport fixture through the full lifecycle. A forwarding service
/// always multiplexes now, so a transport fixture's own carrier stack is the
/// only axis left to vary.
async fn test_transport(config_path: &'static str, t: Type) -> Result<()> {
    test(config_path, t, None).await?;

    Ok(())
}

/// Arm 1 of the transport comparison: a multiplexed pool pinned at 3 tunnels
/// per control session (a stream takes the least-loaded one), full lifecycle
/// including client/server restarts and concurrent load. The three tunnels are
/// established when the service starts and kept for its lifetime.
#[cfg(feature = "multiplex")]
#[tokio::test]
async fn multiplex_tunnel_pool() -> Result<()> {
    init();

    spawn_tcp_backends();

    test(
        "tests/for_tcp/tcp_transport.toml",
        Type::Tcp,
        Some(ClientOverrides { tunnels: Some(3) }),
    )
    .await?;

    Ok(())
}

/// Noise session resume: the client caches the server's ticket on the
/// first full handshake and the next control connection (after the
/// client restarts) resumes the session instead of repeating the key
/// exchanges (transport selector 0x02). The service must behave
/// identically across the restart — same replies, same payloads.
#[cfg(feature = "noise")]
#[tokio::test]
async fn noise_session_resume() -> Result<()> {
    init();

    spawn_tcp_backends();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/noise_resume.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server("tests/for_tcp/noise_resume.toml", server_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_echo(exposed_addrs(Type::Tcp).0, Type::Tcp).await?;
    echo_hitter(exposed_addrs(Type::Tcp).0, Type::Tcp)
        .await
        .unwrap();

    // Restart the client: the control channel reconnects and the resume
    // path engages (the server's ticket from the first connection is
    // cached client-side).
    info!("restart the client onto the resumed session");
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(client);
    let client_shutdown_rx = client_shutdown_tx.subscribe();
    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/noise_resume.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    wait_for_echo(exposed_addrs(Type::Tcp).0, Type::Tcp).await?;
    echo_hitter(exposed_addrs(Type::Tcp).0, Type::Tcp)
        .await
        .unwrap();

    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(server, client);
    Ok(())
}

/// Per-service data-plane overrides: the fixture keeps a pinned pool as the
/// client-wide shape and gives one service its own carrier.
#[cfg(feature = "multiplex")]
#[tokio::test]
async fn per_service_data_modes() -> Result<()> {
    init();

    spawn_tcp_backends();

    test("tests/for_tcp/per_service_carriers.toml", Type::Tcp, None).await?;

    Ok(())
}

/// Bytes pushed through the echo service in the striped-data-channel test:
/// far more than one stripe frame (32 KiB), so the group's chunking and
/// reassembly run for real, and a pattern that makes any reordering or
/// duplication visible byte-for-byte.
#[cfg(feature = "multiplex")]
const STRIPE_BULK_BYTES: usize = 8 * 1024 * 1024;

/// The deterministic payload the striped bulk assertions write: a pattern
/// rather than a constant, so a reordered or duplicated chunk cannot pass for
/// a match.
#[cfg(feature = "multiplex")]
fn bulk_pattern(len: usize) -> Vec<u8> {
    let mut pattern = vec![0u8; len];
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = u8::try_from((i.wrapping_mul(0x9E37_79B1) >> 24) & 0xff).unwrap();
    }
    pattern
}

/// Start writing `pattern` on an owned write half without waiting for it: the
/// striped path's own backpressure is what leaves a large write in flight,
/// which is the state the placement observation below needs.
#[cfg(feature = "multiplex")]
fn spawn_bulk_writer<W: tokio::io::AsyncWrite + Unpin + Send + 'static>(
    mut wr: W,
    pattern: Vec<u8>,
) -> tokio::task::JoinHandle<std::io::Result<()>> {
    tokio::spawn(async move {
        for chunk in pattern.chunks(64 * 1024) {
            wr.write_all(chunk).await?;
        }
        wr.flush().await?;
        // `Ok` is anyhow's in this file; name the standard one explicitly.
        std::result::Result::Ok(())
    })
}

/// Read exactly `pattern.len()` bytes back and verify they are `pattern`: the
/// striped group's reassembly contract (in sequence, no byte lost, none
/// duplicated), in both directions at once because the echo service mirrors
/// the stream.
#[cfg(feature = "multiplex")]
async fn read_bulk_echo<R: tokio::io::AsyncRead + Unpin>(rd: &mut R, pattern: &[u8]) -> Result<()> {
    let len = pattern.len();
    let mut got = vec![0u8; len];
    let mut read = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while read < len {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "striped bulk echo timed out at {read}/{len} bytes"
        );
        let n = time::timeout(Duration::from_secs(30), rd.read(&mut got[read..]))
            .await
            .context("striped bulk echo read timed out")??;
        anyhow::ensure!(n > 0, "echo service closed after {read}/{len} bytes");
        read += n;
    }
    assert_eq!(got, pattern, "the striped path corrupted the byte stream");
    Ok(())
}

/// Push a deterministic pattern through the echo service and verify the
/// reply is the same bytes in the same order.
#[cfg(feature = "multiplex")]
async fn bulk_echo_roundtrip(addr: &'static str, len: usize) -> Result<()> {
    let conn = TcpStream::connect(addr).await?;
    let (mut rd, wr) = conn.into_split();
    let pattern = bulk_pattern(len);
    let writer = spawn_bulk_writer(wr, pattern.clone());
    read_bulk_echo(&mut rd, &pattern).await?;
    writer
        .await
        .context("bulk writer task failed")?
        .context("bulk writer returned an error")?;
    Ok(())
}

/// One live pool as the placement poll reads it: its size, and the
/// `(streams, pending, pinned)` of each of its tunnels.
#[cfg(feature = "multiplex")]
type PoolShape = (usize, Vec<(usize, usize, usize)>);

/// Wait until one live pool carries a stream on **every** one of its four
/// tunnels — the per-instant half of D24, which the pool-size assertion after
/// a transfer cannot see: four streams on three tunnels (with the fourth
/// empty) is the same pool size as the spread.
///
/// The four tunnels are already there: a fixture with no `[client.data.tcp]`
/// block gets the default count (`DEFAULT_MUX_TUNNELS`, 4), established at
/// service start. What is waited for is therefore the *group's* placement —
/// each of its four channels landing on a distinct tunnel — not the pool
/// growing to receive it.
///
/// Bounded by a deadline rather than a fixed sleep, so the assertion is on a
/// state that is polled for, not on a guess about how long a placement takes;
/// the observed `(size, per-tunnel streams)` vectors travel in the failure
/// message, because "the spread never happened" and "the pool never came up"
/// are different defects.
#[cfg(feature = "multiplex")]
async fn wait_for_a_stream_on_every_stripe_tunnel(deadline: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        let observed: Vec<PoolShape> = molehill_rathole::live_pools()
            .iter()
            .map(|pool| (pool.size, pool.tunnels.clone()))
            .collect();
        let spread = observed.iter().any(|(size, tunnels)| {
            *size == 4 && tunnels.len() == 4 && tunnels.iter().all(|(streams, _, _)| *streams >= 1)
        });
        if spread {
            return Ok(());
        }
        anyhow::ensure!(
            start.elapsed() < deadline,
            "no pool carried a stream on each of its four tunnels within {deadline:?}: {observed:?}"
        );
        time::sleep(Duration::from_millis(5)).await;
    }
}

/// Data-channel striping: `[server.data] stripe_count = 4` spreads every
/// visitor connection over 4 parallel data channels (a stripe group, see
/// `src/stripe.rs`). A multi-megabyte transfer must come back
/// byte-identical and in order through the group, back-to-back visitors
/// must each get their own group, and the unstriped request path must keep
/// working beside it.
#[cfg(feature = "multiplex")]
#[tokio::test]
async fn striped_data_channels() -> Result<()> {
    init();

    spawn_tcp_backends();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let client = tokio::spawn(async move {
        run_molehill_client(
            "tests/for_tcp/striped_data_channels.toml",
            client_shutdown_rx,
        )
        .await
        .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server(
            "tests/for_tcp/striped_data_channels.toml",
            server_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_echo(exposed_addrs(Type::Tcp).0, Type::Tcp).await?;

    info!("bulk round trip through a striped visitor connection");
    // One visitor, held open across the placement observation: a group's four
    // channels are gathered when the connection is accepted and live exactly
    // as long as it does. The write is started first and its echo read only
    // after the assertion, so the transfer is in flight *by construction*
    // rather than by timing luck — and the connection is still open when the
    // pool is read, which is what makes this an instant during the transfer
    // and not the warm pool an earlier one left behind.
    let visitor = TcpStream::connect(exposed_addrs(Type::Tcp).0).await?;
    let (mut visitor_rd, visitor_wr) = visitor.into_split();
    let pattern = bulk_pattern(STRIPE_BULK_BYTES);
    let writer = spawn_bulk_writer(visitor_wr, pattern.clone());

    info!("watching the four stripe channels' placement while the transfer runs");
    wait_for_a_stream_on_every_stripe_tunnel(Duration::from_secs(15)).await?;

    read_bulk_echo(&mut visitor_rd, &pattern).await?;
    writer
        .await
        .context("bulk writer task failed")?
        .context("bulk writer returned an error")?;
    // The visitor's end: the group's four channels are released with it.
    drop(visitor_rd);

    // The structural half of the claim (D24), on the real client and a real
    // pinned pool: the group's four channels are spread over the four tunnels
    // the pool established at service start. Before the group was named on the
    // wire the pool stayed at *one* (the elastic rule's business was demand,
    // and the group's four concurrent streams sat below its per-tunnel growth
    // threshold — 7 on the shipped cap), so the assertion below is what catches
    // channels silently sharing a tunnel.
    //
    // The size is the configured count, not a number the group produced: the
    // pool was already at four before the first visitor, and it stays there.
    let sizes: Vec<usize> = molehill_rathole::live_pools()
        .iter()
        .map(|pool| pool.size)
        .collect();
    assert!(
        sizes.iter().all(|size| *size == 4),
        "the stripe group's client must keep its four pinned tunnels: {sizes:?}"
    );

    info!("a second visitor gets its own stripe group");
    bulk_echo_roundtrip(exposed_addrs(Type::Tcp).0, STRIPE_BULK_BYTES).await?;

    info!("small interactive request after the bulk transfers");
    echo_hitter(exposed_addrs(Type::Tcp).0, Type::Tcp)
        .await
        .unwrap();

    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(server, client);

    Ok(())
}

/// Per-service servers: one client registers its services on two different
/// molehill servers — the echo service follows the client-wide control
/// endpoint (server A), the pingpong service overrides it with its own
/// `remote_addr` (server B). Both data planes follow their service's own
/// server, and both must survive a client restart.
#[tokio::test]
async fn services_on_different_servers() -> Result<()> {
    if cfg!(not(all(feature = "client", feature = "server"))) {
        return Ok(());
    }
    init();

    spawn_tcp_backends();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_a_shutdown_tx, server_a_shutdown_rx) = broadcast::channel(1);
    let (server_b_shutdown_tx, server_b_shutdown_rx) = broadcast::channel(1);

    // Client first (it retries until the servers come up).
    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/multi_server_a.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    let server_a = tokio::spawn(async move {
        run_molehill_server("tests/for_tcp/multi_server_a.toml", server_a_shutdown_rx)
            .await
            .unwrap();
    });
    let server_b = tokio::spawn(async move {
        run_molehill_server("tests/for_tcp/multi_server_b.toml", server_b_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_echo(exposed_addrs(Type::Tcp).0, Type::Tcp).await?;
    wait_for_echo(exposed_addrs(Type::Tcp).1, Type::Tcp).await?;

    info!("echo on server A");
    echo_hitter(exposed_addrs(Type::Tcp).0, Type::Tcp)
        .await
        .unwrap();
    info!("pingpong on server B");
    pingpong_hitter(exposed_addrs(Type::Tcp).1, Type::Tcp)
        .await
        .unwrap();

    // Client restart: both services must re-register on their own servers.
    info!("shutdown the client");
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(client);
    info!("restart the client");
    let client_shutdown_rx = client_shutdown_tx.subscribe();
    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/multi_server_a.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await; // Wait for the client to start

    info!("echo on server A after restart");
    echo_hitter(exposed_addrs(Type::Tcp).0, Type::Tcp)
        .await
        .unwrap();
    info!("pingpong on server B after restart");
    pingpong_hitter(exposed_addrs(Type::Tcp).1, Type::Tcp)
        .await
        .unwrap();

    // Shutdown
    info!("shutdown the servers and the client");
    server_a_shutdown_tx.send(true)?;
    server_b_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(server_a, server_b, client);

    Ok(())
}

/// Data plane decoupled from the control plane: the client dials
/// `[client.data].default_addr` and the server binds `[server.data].bind_addr`, so
/// tunnel connections never touch the control listener.
#[cfg(feature = "multiplex")]
#[tokio::test]
async fn separate_data_plane() -> Result<()> {
    init();

    spawn_tcp_backends();

    test("tests/for_tcp/data_plane_separate.toml", Type::Tcp, None).await?;

    Ok(())
}

/// Per-service transport: one client with a plain client-wide transport
/// runs a plain service AND a Noise service (per-service `enable` + own
/// keys) against one keyed server — the multi-server encryption scenario.
/// Both services must work side by side and survive a client restart.
#[tokio::test]
async fn per_service_transport() -> Result<()> {
    if cfg!(not(all(feature = "client", feature = "server"))) {
        return Ok(());
    }
    init();

    spawn_tcp_backends();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let client = tokio::spawn(async move {
        run_molehill_client(
            "tests/for_tcp/per_service_transport.toml",
            client_shutdown_rx,
        )
        .await
        .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server(
            "tests/for_tcp/per_service_transport.toml",
            server_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_echo(exposed_addrs(Type::Tcp).0, Type::Tcp).await?;
    wait_for_echo(exposed_addrs(Type::Tcp).1, Type::Tcp).await?;

    // Plain service and Noise service through the SAME client.
    info!("echo via plain service");
    echo_hitter(exposed_addrs(Type::Tcp).0, Type::Tcp)
        .await
        .unwrap();
    info!("pingpong via noise-enabled service");
    pingpong_hitter(exposed_addrs(Type::Tcp).1, Type::Tcp)
        .await
        .unwrap();

    // Client restart: both services re-register with their own transports.
    info!("shutdown the client");
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(client);
    info!("restart the client");
    let client_shutdown_rx = client_shutdown_tx.subscribe();
    let client = tokio::spawn(async move {
        run_molehill_client(
            "tests/for_tcp/per_service_transport.toml",
            client_shutdown_rx,
        )
        .await
        .unwrap();
    });
    settle(1.0).await;

    info!("echo via plain service after restart");
    echo_hitter(exposed_addrs(Type::Tcp).0, Type::Tcp)
        .await
        .unwrap();
    info!("pingpong via noise-enabled service after restart");
    pingpong_hitter(exposed_addrs(Type::Tcp).1, Type::Tcp)
        .await
        .unwrap();

    // Shutdown
    info!("shutdown everything");
    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(server, client);

    Ok(())
}

/// v3 mixed transports: one server that only places its Noise keys (no
/// transport `type` — the client decides plain vs Noise via the selector
/// byte) serves a plain client AND a Noise client at the same time. Both
/// services must work side by side and survive a plain-client restart.
#[tokio::test]
async fn mixed_transports() -> Result<()> {
    if cfg!(not(all(feature = "client", feature = "server"))) {
        return Ok(());
    }
    init();

    spawn_tcp_backends();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (client_noise_shutdown_tx, client_noise_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/mixed_server.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server("tests/for_tcp/mixed_server.toml", server_shutdown_rx)
            .await
            .unwrap();
    });
    let client_noise = tokio::spawn(async move {
        run_molehill_client(
            "tests/for_tcp/mixed_client_noise.toml",
            client_noise_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_echo(exposed_addrs(Type::Tcp).0, Type::Tcp).await?;
    wait_for_echo(exposed_addrs(Type::Tcp).1, Type::Tcp).await?;

    // Plain client -> echo service; Noise client -> pingpong service.
    info!("echo via plain client");
    echo_hitter(exposed_addrs(Type::Tcp).0, Type::Tcp)
        .await
        .unwrap();
    info!("pingpong via noise client");
    pingpong_hitter(exposed_addrs(Type::Tcp).1, Type::Tcp)
        .await
        .unwrap();

    // Plain-client restart: the server keeps serving both transports.
    info!("shutdown the plain client");
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(client);
    info!("restart the plain client");
    let client_shutdown_rx = client_shutdown_tx.subscribe();
    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/mixed_server.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;

    info!("echo via plain client after restart");
    echo_hitter(exposed_addrs(Type::Tcp).0, Type::Tcp)
        .await
        .unwrap();
    info!("pingpong via noise client after restart");
    pingpong_hitter(exposed_addrs(Type::Tcp).1, Type::Tcp)
        .await
        .unwrap();

    // Shutdown
    info!("shutdown everything");
    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;
    client_noise_shutdown_tx.send(true)?;
    let _ = tokio::join!(server, client, client_noise);

    Ok(())
}

/// Arm 2 of the transport comparison: data tunnels are KCP-over-UDP sessions
/// (Noise-wrapped with the control transport's keys, `[client.data.kcp]`
/// `tunnels` established at service start — the default 4 when unwritten);
/// the control channel stays TCP+Noise. Full lifecycle.
#[cfg(all(feature = "multiplex", feature = "kcp"))]
#[tokio::test]
async fn kcp_tunnel() -> Result<()> {
    init();

    spawn_tcp_backends();

    test("tests/for_tcp/kcp_tunnel.toml", Type::Tcp, None).await?;

    Ok(())
}

/// The KCP carrier is orthogonal to a service's *shape*: this fixture pins a
/// KCP pool, and the KCP listener must accept the tunnel hello and carry the
/// service's data channels as streams of those sessions. Full lifecycle, over
/// Noise.
///
/// The *direct*-channel-over-KCP leg this used to cover is gone with the `mode`
/// key: after it, a direct data channel is only ever a transparent claim's lane,
/// and a claim's lanes need a TUN device on both ends — the root-only acceptance
/// script (`just l3-accept`), not this suite.
#[cfg(all(feature = "multiplex", feature = "kcp"))]
#[tokio::test]
async fn kcp_tunnel_carries_streams() -> Result<()> {
    init();

    spawn_tcp_backends();

    test("tests/for_tcp/kcp_tunnel.toml", Type::Tcp, None).await?;

    Ok(())
}

/// KCP tunnels on the default data-plane endpoint: with neither
/// `[client.data].default_addr` nor `[server.data].bind_addr` set, the KCP sessions
/// dial the control address over UDP — TCP control and UDP KCP data share
/// one port number (different protocols). Two sessions share the single UDP
/// listener, demuxed by peer address + conversation id.
#[cfg(all(feature = "multiplex", feature = "kcp"))]
#[tokio::test]
async fn kcp_same_port() -> Result<()> {
    init();

    spawn_tcp_backends();

    test("tests/for_tcp/kcp_same_port.toml", Type::Tcp, None).await?;

    Ok(())
}

#[tokio::test]
async fn tcp() -> Result<()> {
    init();

    spawn_tcp_backends();

    test_transport("tests/for_tcp/tcp_transport.toml", Type::Tcp).await?;

    #[cfg(feature = "noise")]
    test_transport("tests/for_tcp/noise_transport.toml", Type::Tcp).await?;

    Ok(())
}

#[tokio::test]
async fn udp() -> Result<()> {
    init();

    spawn_udp_backends();
    tokio::spawn(async move {
        if let Err(e) = common::udp::pingpong_server(PINGPONG_SERVER_ADDR).await {
            panic!("Failed to run the pingpong server for testing: {e:?}");
        }
    });

    test_transport("tests/for_udp/tcp_transport.toml", Type::Udp).await?;

    #[cfg(feature = "noise")]
    test_transport("tests/for_udp/noise_transport.toml", Type::Udp).await?;

    Ok(())
}

#[tokio::test]
async fn udp_session_affinity() -> Result<()> {
    init();

    // A stateful-UDP "game server": it records the source address of every
    // datagram (a stateful protocol pins the session to `(ip, port)`) and
    // echoes payloads back to it. One peer must arrive from exactly one
    // source port, or its session is torn in half.
    let seen_srcs = Arc::new(Mutex::new(HashSet::new()));
    let server_seen = seen_srcs.clone();
    tokio::spawn(async move {
        if let Err(e) = sticky_echo_server(server_seen).await {
            panic!("Failed to run the sticky echo server for testing: {e:?}");
        }
    });

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);
    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_udp/affinity_transport.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    // Sleep for 1 second. Expect the client keep retrying to reach the server
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server("tests/for_udp/affinity_transport.toml", server_shutdown_rx)
            .await
            .unwrap();
    });
    let conn = UdpSocket::bind("127.0.0.1:0").await?;
    conn.connect(AFFINITY_EXPOSED_ADDR).await?;
    // Readiness on the very socket that runs the session: a separate probe
    // socket would add a second source port and trip the single-source-port
    // invariant this test asserts.
    wait_for_udp_socket(&conn).await?;

    // Fire a burst without waiting for replies: exactly the pattern that
    // used to race one peer's packets onto different data channels (and out
    // of the client through different local sockets).
    let mut sent = HashSet::new();
    for i in 0..AFFINITY_PACKETS {
        let payload = format!("session-affinity-{i}");
        conn.send(payload.as_bytes()).await?;
        sent.insert(payload);
    }

    // Every datagram must come back.
    let mut received = HashSet::new();
    let mut buf = [0u8; 2048];
    time::timeout(Duration::from_secs(10), async {
        while received.len() < AFFINITY_PACKETS {
            let n = conn.recv(&mut buf).await?;
            received.insert(String::from_utf8_lossy(&buf[..n]).into_owned());
        }
        Ok(())
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for the echo replies"))?;

    assert_eq!(received, sent, "datagrams were lost or corrupted");

    // And all of them must have arrived at the local service from ONE source
    // address: the peer's session stayed on a single outbound socket.
    // Snapshot before asserting — the panic message must not re-lock a Mutex
    // whose guard is still alive in this very expression.
    let (src_count, srcs) = {
        let seen = seen_srcs.lock().unwrap();
        (seen.len(), seen.clone())
    };
    assert_eq!(
        src_count, 1,
        "stateful UDP session was split across multiple source ports: {srcs:?}"
    );

    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(server, client);

    Ok(())
}

/// Echo server that asserts session affinity: records the source address of
/// every datagram it receives.
async fn sticky_echo_server(seen_srcs: Arc<Mutex<HashSet<SocketAddr>>>) -> Result<()> {
    let l = UdpSocket::bind(AFFINITY_LOCAL_SERVICE).await?;
    let mut buf = [0u8; 2048];
    loop {
        let (n, from) = l.recv_from(&mut buf).await?;
        seen_srcs.lock().unwrap().insert(from);
        l.send_to(&buf[..n], from).await?;
    }
}

/// The running `knob_effects` pair, with the handles a scenario needs to stop
/// it. A struct rather than a tuple so the two senders cannot be swapped at a
/// call site.
struct UdpKnobPair {
    client_shutdown: broadcast::Sender<bool>,
    server_shutdown: broadcast::Sender<bool>,
    client: tokio::task::JoinHandle<()>,
    server: tokio::task::JoinHandle<()>,
}

impl UdpKnobPair {
    /// Stop both sides and wait for their instances to end, so a leaked
    /// listener cannot hold this fixture's ports for the next scenario.
    async fn stop(self) {
        let _ = self.server_shutdown.send(true);
        let _ = self.client_shutdown.send(true);
        let _ = tokio::join!(self.server, self.client);
    }
}

/// Start the `knob_effects` fixture's client and server, in that order (the
/// client retries until the server is up), and wait until `wait_for` answers.
/// Only the service under test is probed: the fixture defines both UDP
/// services, but each scenario starts one backend, and probing the other
/// would wait for a service whose local address nothing serves.
async fn start_udp_knob_pair(wait_for: &'static str) -> Result<UdpKnobPair> {
    let (client_shutdown, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown, server_shutdown_rx) = broadcast::channel(1);
    let client = tokio::spawn(async move {
        run_molehill_client(UDP_KNOBS_CONFIG, client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server(UDP_KNOBS_CONFIG, server_shutdown_rx)
            .await
            .unwrap();
    });
    let probe = UdpSocket::bind("127.0.0.1:0").await?;
    probe.connect(wait_for).await?;
    wait_for_udp_socket(&probe).await?;
    Ok(UdpKnobPair {
        client_shutdown,
        server_shutdown,
        client,
        server,
    })
}

/// A UDP backend that records the length of every datagram it receives and
/// answers: `b"reply-big"` gets a `KNOB_BUFFER_SIZE * 2`-byte reply, anything
/// else is echoed. Both halves exist because `udp_buffer_size` is enforced on
/// two different legs — the visitor's datagram is read into the server's
/// buffer, and the local service's reply is read into the client's — and each
/// has to be observable on its own.
async fn length_recording_server(addr: &'static str, lens: Arc<Mutex<Vec<usize>>>) -> Result<()> {
    let l = UdpSocket::bind(addr).await?;
    // Larger than either buffer under test: the truncation this fixture is
    // about must happen in molehill, not in the test's own recv. On the heap,
    // so the server task's future stays small.
    let mut buf = vec![0u8; 65535];
    loop {
        let (n, from) = l.recv_from(&mut buf).await?;
        lens.lock().unwrap().push(n);
        if &buf[..n] == b"reply-big" {
            let big = vec![0x5au8; KNOB_BUFFER_SIZE * 2];
            l.send_to(&big, from).await?;
        } else {
            l.send_to(&buf[..n], from).await?;
        }
    }
}

/// `udp_buffer_size`: a datagram larger than the configured buffer does not
/// arrive intact, and the data channel survives it.
///
/// The shipped code **truncates** such a datagram to `udp_buffer_size` on the
/// reading leg (`recv_from` into a `buffer_size` buffer) rather than dropping
/// it — `docs/configuration.md` says "dropped", and the in-stream drop in
/// `UdpTraffic::read` only fires for a payload longer than the *receiver's*
/// buffer, which a consistently configured pair never produces because the
/// sender already truncated. This pins the behaviour that exists: a visitor's
/// 2 KiB datagram reaches the backend as 1 KiB, and a backend's 2 KiB reply
/// reaches the visitor as 1 KiB; a small datagram still round-trips, which is
/// the "the channel stays usable" half. If the drop ever becomes deliberate,
/// the two `== KNOB_BUFFER_SIZE` assertions below are the ones to change.
#[tokio::test]
async fn udp_buffer_size_bounds_a_datagram_without_breaking_the_channel() -> Result<()> {
    init();

    let lens = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&lens);
    tokio::spawn(async move {
        if let Err(e) = length_recording_server(KNOB_SMALL_LOCAL, recorded).await {
            panic!("Failed to run the length-recording UDP server for testing: {e:?}");
        }
    });

    let pair = start_udp_knob_pair(KNOB_SMALL_EXPOSED).await?;

    // A visitor socket of its own: the readiness probe above is a different
    // peer, and this one is the peer the assertions are about.
    let conn = UdpSocket::bind("127.0.0.1:0").await?;
    conn.connect(KNOB_SMALL_EXPOSED).await?;
    wait_for_udp_socket(&conn).await?;
    // Everything the readiness probe sent is behind this index; the datagrams
    // the assertions below name are the ones this test sends itself.
    let settled = lens.lock().unwrap().len();

    // A visitor datagram over the limit: read into the *server's* buffer (the
    // client's registered `udp_buffer_size`), so the backend must see exactly
    // the limit and the visitor must get that shorter echo back.
    let oversized = vec![0xa5u8; KNOB_BUFFER_SIZE * 2];
    conn.send(&oversized).await?;
    let mut buf = vec![0u8; KNOB_BUFFER_SIZE * 4];
    let n = time::timeout(Duration::from_secs(10), conn.recv(&mut buf))
        .await
        .context("no echo of the oversized datagram")??;
    assert_eq!(
        n, KNOB_BUFFER_SIZE,
        "a datagram over udp_buffer_size must not arrive intact"
    );
    assert_eq!(
        &buf[..n],
        &oversized[..KNOB_BUFFER_SIZE],
        "the delivered bytes must be the datagram's prefix"
    );

    // A backend reply over the limit: read into the *client's* forwarder
    // buffer on the way back, so it reaches the visitor shortened too.
    conn.send(b"reply-big").await?;
    let n = time::timeout(Duration::from_secs(10), conn.recv(&mut buf))
        .await
        .context("no reply to the big-reply request")??;
    assert_eq!(
        n, KNOB_BUFFER_SIZE,
        "the local service's oversized reply must be bounded by udp_buffer_size too"
    );

    // And the channel is still usable after both: the costs of an oversized
    // datagram are its own bytes, not the connection.
    conn.send(b"small-after-oversized").await?;
    let n = time::timeout(Duration::from_secs(10), conn.recv(&mut buf))
        .await
        .context("the channel stopped carrying datagrams after an oversized one")??;
    assert_eq!(
        &buf[..n],
        b"small-after-oversized",
        "the datagram after the oversized ones must arrive intact"
    );

    let seen = lens.lock().unwrap().clone();
    assert!(
        seen.len() > settled + 1,
        "the backend must have received the test's datagrams: {seen:?}"
    );
    assert_eq!(
        seen[settled], KNOB_BUFFER_SIZE,
        "the backend sees the visitor's oversized datagram truncated to udp_buffer_size: {seen:?}"
    );
    assert_eq!(
        seen[settled + 1],
        b"reply-big".len(),
        "the big-reply request itself is small; only the reply is oversized: {seen:?}"
    );

    pair.stop().await;
    Ok(())
}

/// `udp_idle_timeout`: a peer mapping (and the local socket it owns) is
/// recycled after the configured silence, and the *next* datagram from the
/// same peer reaches the backend from a different source port — exactly the
/// consequence `docs/configuration.md` documents, and the reason a stateful
/// UDP session has to keep talking or raise the timeout.
///
/// Within the window the port must be stable instead: the first half is the
/// existing affinity guarantee, observed here because the second half is
/// meaningless without it.
///
/// The one assumption in the second half is that the kernel does not hand the
/// very same ephemeral port back to the fresh socket; with the whole range in
/// play that is a draw of one in tens of thousands, and it is the same
/// assumption the affinity test above rests on in the other direction.
#[tokio::test]
async fn udp_idle_timeout_rebinds_the_peer_to_a_fresh_local_socket() -> Result<()> {
    init();

    // Every datagram's source address, in arrival order: the ports the backend
    // observes are the only place the client's per-peer socket is visible.
    let seen_srcs: Arc<Mutex<Vec<SocketAddr>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen_srcs);
    tokio::spawn(async move {
        if let Err(e) = port_recording_echo_server(KNOB_IDLE_LOCAL, recorded).await {
            panic!("Failed to run the port-recording UDP server for testing: {e:?}");
        }
    });

    let pair = start_udp_knob_pair(KNOB_IDLE_EXPOSED).await?;

    // One peer for the whole scenario: the mapping under test is this
    // socket's, and re-binding the visitor would create a second peer whose
    // ports say nothing about the first one.
    let conn = UdpSocket::bind("127.0.0.1:0").await?;
    conn.connect(KNOB_IDLE_EXPOSED).await?;
    wait_for_udp_socket(&conn).await?;
    // The readiness probes (the fixture helper's and this socket's) are behind
    // this index, so the window below is exactly this test's own datagrams.
    let settled = seen_srcs.lock().unwrap().len();

    // A burst well inside the timeout: one peer, one socket, one port.
    for i in 0..KNOB_IDLE_BURST {
        conn.send(format!("inside-{i}").as_bytes()).await?;
    }
    let mut buf = [0u8; 64];
    for _ in 0..KNOB_IDLE_BURST {
        time::timeout(Duration::from_secs(5), conn.recv(&mut buf))
            .await
            .context("a datagram of the within-timeout burst went missing")??;
    }
    let within = {
        let seen = seen_srcs.lock().unwrap();
        seen[settled..].to_vec()
    };
    assert_eq!(
        within.len(),
        KNOB_IDLE_BURST,
        "every datagram of the burst must have arrived: {within:?}"
    );
    let first_port = within[0].port();
    assert!(
        within.iter().all(|src| src.port() == first_port),
        "every datagram inside udp_idle_timeout must leave one local socket: {within:?}"
    );

    // Now go quiet for longer than the timeout. Nothing in either direction
    // resets the client's idle timer, so the forwarder (and its socket) is
    // gone by the time the next datagram arrives; that datagram re-binds, and
    // the backend sees the new bind.
    settle(KNOB_IDLE_SILENCE).await;
    conn.send(b"after-idle").await?;
    time::timeout(Duration::from_secs(10), conn.recv(&mut buf))
        .await
        .context("the datagram after the idle timeout was never echoed")??;

    let after = {
        let seen = seen_srcs.lock().unwrap();
        seen[settled..].to_vec()
    };
    assert_eq!(
        after.len(),
        KNOB_IDLE_BURST + 1,
        "the post-timeout datagram must have arrived: {after:?}"
    );
    let rebound_port = after.last().unwrap().port();
    assert_ne!(
        rebound_port, first_port,
        "a peer idle for {KNOB_IDLE_TIMEOUT_SECS} s must be re-bound to a fresh local socket, \
         so the backend sees a new source port; the observed addresses were {after:?}"
    );

    pair.stop().await;
    Ok(())
}

/// Echo server for the idle-timeout scenario: records the source address of
/// every datagram, in order (the affinity test's set is unordered and cannot
/// show *which* datagram came from where).
async fn port_recording_echo_server(
    addr: &'static str,
    seen: Arc<Mutex<Vec<SocketAddr>>>,
) -> Result<()> {
    let l = UdpSocket::bind(addr).await?;
    let mut buf = [0u8; 2048];
    loop {
        let (n, from) = l.recv_from(&mut buf).await?;
        seen.lock().unwrap().push(from);
        l.send_to(&buf[..n], from).await?;
    }
}

#[instrument]
async fn test(
    config_path: &'static str,
    t: Type,
    #[cfg_attr(
        not(feature = "multiplex"),
        allow(unused_variables, reason = "only read by the multiplex arm")
    )]
    overrides: Option<ClientOverrides>,
) -> Result<()> {
    if cfg!(not(all(feature = "client", feature = "server"))) {
        // Skip the test if the client or the server is not enabled
        return Ok(());
    }

    // `None` uses the fixture as-is (compiled-in mux defaults). When the
    // multiplex feature is present, overrides materialize an explicit copy
    // so alternate data paths (no-mux, multi-tunnel) are exercised.
    #[cfg(feature = "multiplex")]
    let (run_config, variant) = match overrides {
        Some(overrides) => {
            let path = write_client_variant(config_path, &overrides)?;
            (path.to_string_lossy().into_owned(), Some(path))
        }
        None => (config_path.to_owned(), None),
    };
    #[cfg(not(feature = "multiplex"))]
    let (run_config, _variant) = (config_path.to_owned(), None::<PathBuf>);

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    // Start the client
    info!("start the client");
    let client_config = run_config.clone();
    let client = tokio::spawn(async move {
        run_molehill_client(&client_config, client_shutdown_rx)
            .await
            .unwrap();
    });

    // Sleep for 1 second. Expect the client keep retrying to reach the server
    settle(1.0).await;

    // Start the server
    info!("start the server");
    let server_config = run_config.clone();
    let server = tokio::spawn(async move {
        run_molehill_server(&server_config, server_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_echo(exposed_addrs(t).0, t).await?;
    wait_for_echo(exposed_addrs(t).1, t).await?;

    info!("echo");
    echo_hitter(exposed_addrs(t).0, t).await.unwrap();
    info!("pingpong");
    pingpong_hitter(exposed_addrs(t).1, t).await.unwrap();

    // Simulate the client crash and restart
    info!("shutdown the client");
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(client);

    info!("restart the client");
    let client_shutdown_rx = client_shutdown_tx.subscribe();
    let client_config = run_config.clone();
    let client = tokio::spawn(async move {
        run_molehill_client(&client_config, client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await; // Wait for the client to start

    info!("echo");
    echo_hitter(exposed_addrs(t).0, t).await.unwrap();
    info!("pingpong");
    pingpong_hitter(exposed_addrs(t).1, t).await.unwrap();

    // Simulate the server crash and restart
    info!("shutdown the server");
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(server);

    info!("restart the server");
    let server_shutdown_rx = server_shutdown_tx.subscribe();
    let server_config = run_config.clone();
    let server = tokio::spawn(async move {
        run_molehill_server(&server_config, server_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_echo(exposed_addrs(t).0, t).await?;
    wait_for_echo(exposed_addrs(t).1, t).await?;

    // Simulate heavy load
    info!("lots of echo and pingpong");

    let mut v = Vec::new();

    for _ in 0..HITTER_NUM / 2 {
        v.push(tokio::spawn(async move {
            echo_hitter(exposed_addrs(t).0, t).await.unwrap();
        }));

        v.push(tokio::spawn(async move {
            pingpong_hitter(exposed_addrs(t).1, t).await.unwrap();
        }));
    }

    for h in v {
        assert!(tokio::join!(h).0.is_ok());
    }

    // Shutdown
    info!("shutdown the server and the client");
    server_shutdown_tx.send(true)?;
    client_shutdown_tx.send(true)?;

    let _ = tokio::join!(server, client);

    #[cfg(feature = "multiplex")]
    if let Some(path) = variant {
        let _ = fs::remove_file(path);
    }

    Ok(())
}

async fn echo_hitter(addr: &'static str, t: Type) -> Result<()> {
    match t {
        Type::Tcp => tcp_echo_hitter(addr).await,
        Type::Udp => udp_echo_hitter(addr).await,
    }
}

async fn pingpong_hitter(addr: &'static str, t: Type) -> Result<()> {
    match t {
        Type::Tcp => tcp_pingpong_hitter(addr).await,
        Type::Udp => udp_pingpong_hitter(addr).await,
    }
}

async fn tcp_echo_hitter(addr: &'static str) -> Result<()> {
    let mut conn = TcpStream::connect(addr).await?;

    let mut wr = [0u8; 1024];
    let mut rd = [0u8; 1024];
    for _ in 0..100 {
        rand::fill(&mut wr);
        conn.write_all(&wr).await?;
        conn.read_exact(&mut rd).await?;
        assert_eq!(wr, rd);
    }

    Ok(())
}

async fn udp_echo_hitter(addr: &'static str) -> Result<()> {
    let conn = UdpSocket::bind("127.0.0.1:0").await?;
    conn.connect(addr).await?;

    let mut wr = [0u8; 128];
    let mut rd = [0u8; 128];
    for _ in 0..3 {
        rand::fill(&mut wr);

        conn.send(&wr).await?;
        debug!("send");

        // Bound: a dropped datagram must fail this test, not hang the
        // serial suite forever.
        time::timeout(Duration::from_secs(2), conn.recv(&mut rd))
            .await
            .context("udp echo reply timed out")??;
        debug!("recv");

        assert_eq!(wr, rd);
    }
    Ok(())
}

async fn tcp_pingpong_hitter(addr: &'static str) -> Result<()> {
    let mut conn = TcpStream::connect(addr).await?;

    let wr = PING.as_bytes();
    let mut rd = [0u8; PONG.len()];

    for _ in 0..100 {
        conn.write_all(wr).await?;
        conn.read_exact(&mut rd).await?;
        assert_eq!(rd, PONG.as_bytes());
    }

    Ok(())
}

async fn udp_pingpong_hitter(addr: &'static str) -> Result<()> {
    let conn = UdpSocket::bind("127.0.0.1:0").await?;
    conn.connect(&addr).await?;

    let wr = PING.as_bytes();
    let mut rd = [0u8; PONG.len()];

    for _ in 0..3 {
        conn.send(wr).await?;
        debug!("ping");

        // Bound: a dropped datagram must fail this test, not hang the
        // serial suite forever.
        time::timeout(Duration::from_secs(2), conn.recv(&mut rd))
            .await
            .context("udp pong reply timed out")??;
        debug!("pong");

        assert_eq!(rd, PONG.as_bytes());
    }

    Ok(())
}

/// A control channel that ends on its own must release the service's public
/// ports.
///
/// The server's service entry owns the connection pool, and the pool owns the
/// bound listener. When that entry outlived its control channel, the ports
/// stayed bound after the client was gone, and the next registration of the
/// same service was rejected with "Port N is already in use" while nothing
/// was serving it any more.
/// A dead local service must fail one visitor — not withdraw the service.
///
/// v0.9.1 removed the health check: visibility follows registration alone, so
/// the client never probes `local_addr` and never deregisters a service whose
/// backend is down. A visitor gets what a reverse proxy without a health check
/// gives: the request fails for that connection (nginx-502 semantics), and the
/// cause goes to the log instead of into a state machine.
///
/// Three observable consequences, in order: a visitor to the dead service
/// fails instead of hanging; the service is *still registered*, so it forwards
/// the moment the backend appears, with no client restart and no
/// re-registration; and a healthy service on the same client never noticed.
#[tokio::test]
async fn dead_backend_fails_one_visitor_and_stays_registered() -> Result<()> {
    if cfg!(not(all(feature = "client", feature = "server"))) {
        return Ok(());
    }
    init();
    spawn_tcp_backends();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/dead_backend.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server("tests/for_tcp/dead_backend.toml", server_shutdown_rx)
            .await
            .unwrap();
    });

    // The healthy neighbour doubles as the control: the client itself is fine,
    // and its exposed port appears only once its registration landed.
    wait_for_echo(DEAD_NEIGHBOUR_EXPOSED, Type::Tcp).await?;

    // 1. One visitor, one failure — and it must not hang. The registration is
    //    what makes the connection possible at all, so this is also the proof
    //    that the dead service is still registered (see
    //    `wait_for_failed_request`).
    wait_for_failed_request(DEAD_EXPOSED).await?;

    // 2. The backend appears. Nothing is restarted: the registration was never
    //    withdrawn, so the same exposed port starts forwarding.
    let backend = tokio::spawn(async move {
        let _ = common::tcp::echo_server(DEAD_BACKEND).await;
    });
    wait_for_echo(DEAD_EXPOSED, Type::Tcp).await?;

    // 3. The failure was local to one service; the neighbour never noticed.
    wait_for_echo(DEAD_NEIGHBOUR_EXPOSED, Type::Tcp).await?;

    backend.abort();
    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);

    Ok(())
}

/// Wait until a visitor to `addr` gets a *failure* rather than a hang.
///
/// Two "not yet" states are indistinguishable from the intended one on the
/// first attempt: the exposed port refuses connections until the client's
/// registration lands, and the first accepted visitor may arrive before the
/// server's pool is ready. So the check is retried, and what it waits for is
/// the assertion itself: a connection that the server *accepted* (nothing else
/// can produce a post-connect outcome) and that then ends instead of waiting
/// for a backend nobody is listening on.
///
/// Deliberately not a bind probe: asking the OS "is this port held?" reads the
/// platform's `SO_REUSEADDR` semantics, not the tool's behaviour — a wildcard
/// listener refuses a specific-address bind on Linux and accepts it on macOS,
/// which is exactly how this test failed its first CI run.
async fn wait_for_failed_request(addr: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        match visitor_gets_a_failed_request(addr).await {
            std::result::Result::Ok(()) => return Ok(()),
            std::result::Result::Err(e) => {
                if std::time::Instant::now() > deadline {
                    anyhow::bail!("a visitor to {addr} never failed within 15 s (last: {e})");
                }
            }
        }
        time::sleep(Duration::from_millis(200)).await;
    }
}

/// Connect to `addr` and require the connection to *end*: EOF or a reset.
///
/// A forwarded request to a backend nobody listens on has nothing to send
/// back, so those are the only acceptable outcomes. Silence for ten seconds is
/// the bug being guarded against — a visitor waiting on a request that will
/// never be answered.
async fn visitor_gets_a_failed_request(addr: &str) -> Result<()> {
    let mut conn = TcpStream::connect(addr).await?;
    conn.write_all(PING.as_bytes()).await?;
    let mut buf = [0u8; 64];
    let read = time::timeout(Duration::from_secs(10), conn.read(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("the visitor to {addr} hung for 10 s instead of failing"))?;
    match read {
        // Closed or reset: both are "the request failed", and neither is a
        // hang.
        std::result::Result::Ok(0) | std::result::Result::Err(_) => Ok(()),
        std::result::Result::Ok(n) => anyhow::bail!("a dead backend answered with {n} bytes"),
    }
}

#[tokio::test]
async fn finished_control_channel_releases_its_ports() -> Result<()> {
    if cfg!(not(all(feature = "client", feature = "server"))) {
        return Ok(());
    }
    init();

    spawn_tcp_backends();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/teardown_release.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server("tests/for_tcp/teardown_release.toml", server_shutdown_rx)
            .await
            .unwrap();
    });
    wait_for_echo(exposed_addrs(Type::Tcp).0, Type::Tcp).await?;
    wait_for_echo(exposed_addrs(Type::Tcp).1, Type::Tcp).await?;

    info!("shutdown the client, keep the server running");
    client_shutdown_tx.send(true)?;
    let _ = tokio::join!(client);

    // Binding the exposed ports here is the OS-level proof that the server
    // dropped its listeners: while it still holds them the bind is refused
    // with "address in use", and there is no client left to re-register them.
    let (echo_addr, pingpong_addr) = exposed_addrs(Type::Tcp);
    for addr in [echo_addr, pingpong_addr] {
        wait_for_port_release(addr).await?;
    }

    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(server);

    Ok(())
}

/// Wait until `addr` can be bound again, failing the test after a generous
/// timeout — the teardown is asynchronous by design, so a short grace period
/// is expected rather than a bug.
async fn wait_for_port_release(addr: &str) -> Result<()> {
    let deadline = time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            std::result::Result::Ok(listener) => {
                drop(listener);
                return Ok(());
            }
            std::result::Result::Err(e) => {
                if time::Instant::now() >= deadline {
                    anyhow::bail!("{addr} was still bound 10s after the client left: {e}");
                }
                settle(0.05).await;
            }
        }
    }
}

// --- protocol v4 sessions -------------------------------------------------
//
// The four scenarios below pin what one control *session* per endpoint bought:
// one connection for every service that dials it, a service-level failure that
// stays a service-level failure, and a heartbeat contract the client cannot
// silently misconfigure.

/// Wait until something accepts on `addr`, then drop the probe.
///
/// The control listener is the only thing up before a client registers
/// anything, so this is what an "assert the client connects exactly once"
/// scenario has to wait on.
async fn wait_for_listener(addr: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("nothing listened on {addr} within 15 s");
        }
        time::sleep(Duration::from_millis(50)).await;
    }
}

/// Assert that nothing is exposed at `addr` — the server never bound it.
async fn assert_not_exposed(addr: &str) -> Result<()> {
    match TcpStream::connect(addr).await {
        std::result::Result::Ok(_) => {
            anyhow::bail!("{addr} accepted a connection, but no service should be exposed")
        }
        std::result::Result::Err(_) => Ok(()),
    }
}

/// D1: two services on one endpoint share **one** control connection, and both
/// forward through it.
///
/// The count is the server's own (`control_sessions_accepted`), not a log line:
/// a client that opened a connection per service would look identical in every
/// forwarding assertion, and only the session count can tell the difference.
#[cfg(all(feature = "client", feature = "server"))]
#[tokio::test]
async fn one_control_session_carries_every_service() -> Result<()> {
    init();
    spawn_tcp_backends();

    let before = molehill_rathole::control_sessions_accepted();
    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/tcp_transport.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server("tests/for_tcp/tcp_transport.toml", server_shutdown_rx)
            .await
            .unwrap();
    });

    // Both services are registered and forwarding — over one connection, or
    // the delta below would be 2.
    wait_for_echo(exposed_addrs(Type::Tcp).0, Type::Tcp).await?;
    wait_for_echo(exposed_addrs(Type::Tcp).1, Type::Tcp).await?;
    assert_eq!(
        molehill_rathole::control_sessions_accepted() - before,
        1,
        "two services that dial one endpoint must share one control session"
    );

    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}

/// D2: a service the server refuses (here: a port outside `allow_ports`) is
/// rejected on its own — its sibling keeps forwarding and the session stays up.
#[cfg(all(feature = "client", feature = "server"))]
#[tokio::test]
async fn a_rejected_service_leaves_its_session_and_siblings_running() -> Result<()> {
    init();
    spawn_tcp_backends();

    let before = molehill_rathole::control_sessions_accepted();
    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let client = tokio::spawn(async move {
        run_molehill_client(
            "tests/for_tcp/session_partial_reject.toml",
            client_shutdown_rx,
        )
        .await
        .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server(
            "tests/for_tcp/session_partial_reject.toml",
            server_shutdown_rx,
        )
        .await
        .unwrap();
    });

    // The service inside `allow_ports` forwards…
    wait_for_echo(SESSION_REJECT_OK, Type::Tcp).await?;
    // …and the refusal cost exactly nothing else: one session, still one.
    assert_eq!(
        molehill_rathole::control_sessions_accepted() - before,
        1,
        "a refused service must not take the session (or its sibling) down"
    );
    // The refused service was never exposed at all. The verdicts of the two
    // registrations can arrive in either order (the client walks a HashMap),
    // so let the second one land before looking for a listener that must not
    // exist.
    settle(0.5).await;
    assert_not_exposed(SESSION_REJECT_BAD).await?;
    // The sibling is still forwarding after both verdicts.
    wait_for_echo(SESSION_REJECT_OK, Type::Tcp).await?;

    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}

/// D2, the credential half: a service whose own `token` is the server's is
/// accepted, and one whose token differs is refused **alone**.
///
/// The server owns no per-service token table, so the accepted service names
/// the same value `[server].default_token` holds; the foreign one proves a
/// different digest and is answered with its own `RegisterRejected`.
#[cfg(all(feature = "client", feature = "server"))]
#[tokio::test]
async fn a_foreign_service_token_rejects_only_that_service() -> Result<()> {
    init();
    spawn_tcp_backends();

    let before = molehill_rathole::control_sessions_accepted();
    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let client = tokio::spawn(async move {
        run_molehill_client(
            "tests/for_tcp/session_service_token.toml",
            client_shutdown_rx,
        )
        .await
        .unwrap();
    });
    settle(1.0).await;
    let server = tokio::spawn(async move {
        run_molehill_server(
            "tests/for_tcp/session_service_token.toml",
            server_shutdown_rx,
        )
        .await
        .unwrap();
    });

    wait_for_echo(SESSION_TOKEN_OK, Type::Tcp).await?;
    assert_eq!(
        molehill_rathole::control_sessions_accepted() - before,
        1,
        "a refused service credential must not cost the session"
    );
    settle(0.5).await;
    assert_not_exposed(SESSION_TOKEN_BAD).await?;
    wait_for_echo(SESSION_TOKEN_OK, Type::Tcp).await?;

    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}

/// Protocol v3 is not served: a **v3** control hello is refused on its own
/// connection, without an answer, and the listener keeps working.
///
/// v0.10.0 changed the dialect the client speaks, so a server of this release
/// has one dialect to serve. The interop matrix's new-server/old-client case
/// is the witness that needs the previous release's binary; this one needs
/// nothing but the server, and pins the two halves that matter on this tree —
/// the refused connection gets no reply (the reader cannot parse a hello whose
/// version it does not serve), and the same process then serves a v4 client
/// through the same listener.
#[cfg(all(feature = "client", feature = "server"))]
#[tokio::test]
async fn a_v3_hello_is_refused_on_its_own_connection() -> Result<()> {
    init();

    spawn_tcp_backends();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    let client = tokio::spawn(async move {
        run_molehill_client("tests/for_tcp/tcp_transport.toml", client_shutdown_rx)
            .await
            .unwrap();
    });
    let server = tokio::spawn(async move {
        run_molehill_server("tests/for_tcp/tcp_transport.toml", server_shutdown_rx)
            .await
            .unwrap();
    });
    // `[server.control].bind_addr` of `tcp_transport.toml`.
    wait_for_listener("127.0.0.1:2333").await?;

    // The v4 half first: the refused connection below must not be confused
    // with a listener that never came up.
    wait_for_echo("127.0.0.1:2334", Type::Tcp).await?;

    let mut conn = TcpStream::connect("127.0.0.1:2333").await?;
    // The plain selector, then a v3 control hello: variant tag 0, version 3,
    // and the 32 bytes a v3 client derives from its service name.
    let mut hello = vec![0x00u8, 0x00, 3u8];
    hello.extend([0x42u8; 32]);
    conn.write_all(&hello).await?;
    conn.flush().await?;

    // No answer at all: the version this server does not serve is refused by
    // `read_hello`, which ends that connection.
    let mut reply = [0u8; 1];
    let read = time::timeout(Duration::from_secs(10), conn.read(&mut reply)).await??;
    assert_eq!(read, 0, "a v3 hello must be answered with nothing");

    // The refusal was local: that same v4 session is still forwarding.
    wait_for_echo("127.0.0.1:2334", Type::Tcp).await?;

    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(server, client);
    Ok(())
}

/// D11: the client derives its heartbeat timeout from the cadence the server
/// declares, and a configured value *below* that floor ends the session once
/// instead of reconnecting forever.
///
/// The message itself — both numbers in one typed, terminal error — is pinned
/// by the `resolve_heartbeat_timeout` unit tests in `src/core/client.rs`; what
/// this scenario adds is the observable half: one authenticated connection and
/// no service exposed.
#[cfg(all(feature = "client", feature = "server"))]
#[tokio::test]
async fn a_timeout_below_the_heartbeat_floor_is_refused_once() -> Result<()> {
    init();

    let (client_shutdown_tx, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown_tx, server_shutdown_rx) = broadcast::channel(1);

    // The server first, its control listener confirmed before the client
    // starts: the count below measures *authenticated* sessions, and a client
    // that had to retry into a server that was not up yet would blur "refused
    // once" into "connected once".
    let server = tokio::spawn(async move {
        run_molehill_server(
            "tests/for_tcp/session_heartbeat_floor.toml",
            server_shutdown_rx,
        )
        .await
        .unwrap();
    });
    wait_for_listener(SESSION_HEARTBEAT_ADDR).await?;
    let before = molehill_rathole::control_sessions_accepted();

    let client = tokio::spawn(async move {
        run_molehill_client(
            "tests/for_tcp/session_heartbeat_floor.toml",
            client_shutdown_rx,
        )
        .await
        .unwrap();
    });
    settle(3.0).await;

    // The server declares 30 s, so the floor is 65 s and the configured 20 s
    // cannot survive it. The session authenticated once and stopped: a retry
    // loop would have opened a second authenticated connection by now.
    assert_eq!(
        molehill_rathole::control_sessions_accepted() - before,
        1,
        "a timeout below the derived floor must end the session after one try"
    );
    // The session ended before any registration: nothing is exposed.
    assert_not_exposed(SESSION_HEARTBEAT_EXPOSED).await?;

    client_shutdown_tx.send(true)?;
    server_shutdown_tx.send(true)?;
    let _ = tokio::join!(client, server);
    Ok(())
}
