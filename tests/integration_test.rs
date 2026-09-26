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
    net::{TcpListener, TcpStream, UdpSocket},
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

// Ports for the transparent-visibility regression
// (`dead_backend_fails_one_visitor_and_stays_registered`): one service whose
// backend is not running, and one healthy service on the same client.
const DEAD_BACKEND: &str = "127.0.0.1:8099";
const DEAD_EXPOSED: &str = "127.0.0.1:2350";
const DEAD_NEIGHBOUR_EXPOSED: &str = "127.0.0.1:2351";

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

/// Per-run overrides of `[client.data]` default fields, materialized into a
/// temp copy of a fixture.
#[cfg(feature = "multiplex")]
#[derive(Debug, Default)]
struct ClientOverrides {
    /// `"multiplex"` or `"direct"`.
    mode: Option<&'static str>,
    count: Option<usize>,
}

/// Placeholder so `test()` keeps its signature without the `multiplex`
/// feature (callers only ever pass `None` there).
#[cfg(not(feature = "multiplex"))]
#[derive(Debug)]
struct ClientOverrides;

/// Materialize a copy of `config_path` with the requested `[client.data]`
/// default fields applied.
///
/// Fixtures intentionally omit `[client.data]` so they follow the
/// compiled-in defaults (`mode = "multiplex"`, four tunnels, with the
/// `multiplex` feature). Explicit copies are what give the integration
/// matrix its non-multiplexed and multi-tunnel legs. The copy lives in the
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
    if overrides.mode.is_some() || overrides.count.is_some() {
        if !client.contains_key("data") {
            client.insert("data".to_owned(), toml::Value::Table(toml::map::Map::new()));
        }
        let data = client
            .get_mut("data")
            .and_then(toml::Value::as_table_mut)
            .ok_or_else(|| {
                anyhow::anyhow!("Test fixture {config_path} has a non-table [client.data]")
            })?;
        if let Some(mode) = overrides.mode {
            data.insert(
                "default_mode".to_owned(),
                toml::Value::String(mode.to_owned()),
            );
        }
        if let Some(count) = overrides.count {
            data.insert(
                "default_count".to_owned(),
                toml::Value::Integer(i64::try_from(count).unwrap()),
            );
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

/// Run one transport fixture through the full lifecycle with the default
/// data-plane mode, then (when the feature is compiled in) again in
/// `direct` mode. This is the `{transport} × {multiplex|direct}` matrix.
async fn test_transport(config_path: &'static str, t: Type) -> Result<()> {
    test(config_path, t, None).await?;

    #[cfg(feature = "multiplex")]
    test(
        config_path,
        t,
        Some(ClientOverrides {
            mode: Some("direct"),
            ..Default::default()
        }),
    )
    .await?;

    Ok(())
}

/// Arm 1 of the transport comparison: N parallel tunnels per control session
/// (streams round-robin across them), full lifecycle including client/server
/// restarts and concurrent load.
#[cfg(feature = "multiplex")]
#[tokio::test]
async fn multiplex_tunnel_pool() -> Result<()> {
    init();

    spawn_tcp_backends();

    test(
        "tests/for_tcp/tcp_transport.toml",
        Type::Tcp,
        Some(ClientOverrides {
            mode: Some("multiplex"),
            count: Some(3),
        }),
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

/// Per-service data-plane overrides: the fixture keeps
/// `mode = "multiplex"` (count 1) as the client-wide default while one
/// service forces `mode = "direct"` — both data paths must work side by
/// side on one client through the full lifecycle.
#[cfg(feature = "multiplex")]
#[tokio::test]
async fn per_service_data_modes() -> Result<()> {
    init();

    spawn_tcp_backends();

    test("tests/for_tcp/per_service_modes.toml", Type::Tcp, None).await?;

    Ok(())
}

/// Bytes pushed through the echo service in the striped-data-channel test:
/// far more than one stripe frame (32 KiB), so the group's chunking and
/// reassembly run for real, and a pattern that makes any reordering or
/// duplication visible byte-for-byte.
#[cfg(feature = "multiplex")]
const STRIPE_BULK_BYTES: usize = 8 * 1024 * 1024;

/// Push a deterministic pattern through the echo service and verify the
/// reply is the same bytes in the same order: the striped group's
/// reassembly contract, in both directions at once (the echo service
/// mirrors the stream).
#[cfg(feature = "multiplex")]
async fn bulk_echo_roundtrip(addr: &'static str, len: usize) -> Result<()> {
    let conn = TcpStream::connect(addr).await?;
    let (mut rd, mut wr) = conn.into_split();

    let mut expected = vec![0u8; len];
    for (i, b) in expected.iter_mut().enumerate() {
        *b = u8::try_from((i.wrapping_mul(0x9E37_79B1) >> 24) & 0xff).unwrap();
    }

    let write_pattern = expected.clone();
    let writer = tokio::spawn(async move {
        for chunk in write_pattern.chunks(64 * 1024) {
            wr.write_all(chunk).await?;
        }
        wr.flush().await?;
        Ok(())
    });

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
    assert_eq!(got, expected, "the striped path corrupted the byte stream");
    writer
        .await
        .context("bulk writer task failed")?
        .context("bulk writer returned an error")?;
    Ok(())
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
    bulk_echo_roundtrip(exposed_addrs(Type::Tcp).0, STRIPE_BULK_BYTES).await?;

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
/// (Noise-wrapped with the control transport's keys, 2 parallel sessions);
/// the control channel stays TCP+Noise. Full lifecycle.
#[cfg(all(feature = "multiplex", feature = "kcp"))]
#[tokio::test]
async fn kcp_tunnel() -> Result<()> {
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

    // The dead service is registered as soon as the server holds its exposed
    // port (a port this process can no longer bind). This is the registration
    // proof, and it creates no visitor traffic to explain away.
    wait_for_port_bound(DEAD_EXPOSED).await?;
    // The healthy neighbour doubles as the control: the client itself is fine.
    wait_for_echo(DEAD_NEIGHBOUR_EXPOSED, Type::Tcp).await?;

    // 1. One visitor, one failure — and it must not hang.
    visitor_gets_a_failed_request(DEAD_EXPOSED).await?;

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

/// Wait until `addr` is *held* by someone else, i.e. binding it fails.
///
/// The inverse of `wait_for_port_release`, and the way to observe a listener
/// without making a visitor: a bind that succeeds means nothing is listening
/// yet, and the listener is dropped immediately so the observation cannot
/// itself hold the port.
async fn wait_for_port_bound(addr: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        match TcpListener::bind(addr).await {
            std::result::Result::Err(_) => return Ok(()),
            std::result::Result::Ok(listener) => {
                drop(listener);
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!("nothing bound {addr} within 15 s");
                }
                time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
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
