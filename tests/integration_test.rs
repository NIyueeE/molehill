//! End-to-end integration tests: real server/client pairs over every
//! transport, plus the UDP session-affinity regression scenario.
//!
//! Run serially (`--test-threads=1`): the scenarios bind fixed ports.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use anyhow::{Ok, Result};
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

/// Materialize a copy of `config_path` with `[client].mux` pinned to `mux`.
///
/// Fixtures intentionally omit `mux` so they follow the compiled-in default
/// (`true` with the `multiplex` feature). The explicit `mux = false` copy is
/// what gives the integration matrix its non-multiplexed leg. The copy lives
/// in the system temp dir and is removed after the scenario.
#[cfg(feature = "multiplex")]
fn write_mux_variant(config_path: &str, mux: bool) -> Result<PathBuf> {
    let source = Path::new(config_path);
    let contents = fs::read_to_string(source)?;
    let mut doc: toml::Value = toml::from_str(&contents)?;
    let client = doc
        .get_mut("client")
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| anyhow::anyhow!("Test fixture {config_path} has no [client] table"))?;
    client.insert("mux".to_owned(), toml::Value::Boolean(mux));

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
/// mux setting, then (when the feature is compiled in) again with mux
/// explicitly disabled. This is the `{transport} × {mux±}` matrix.
async fn test_transport(config_path: &'static str, t: Type) -> Result<()> {
    test(config_path, t, None).await?;

    #[cfg(feature = "multiplex")]
    test(config_path, t, Some(false)).await?;

    Ok(())
}

#[tokio::test]
async fn tcp() -> Result<()> {
    init();

    // Spawn a echo server
    tokio::spawn(async move {
        if let Err(e) = common::tcp::echo_server(ECHO_SERVER_ADDR).await {
            panic!("Failed to run the echo server for testing: {e:?}");
        }
    });

    // Spawn a pingpong server
    tokio::spawn(async move {
        if let Err(e) = common::tcp::pingpong_server(PINGPONG_SERVER_ADDR).await {
            panic!("Failed to run the pingpong server for testing: {e:?}");
        }
    });

    test_transport("tests/for_tcp/tcp_transport.toml", Type::Tcp).await?;

    #[cfg(any(
         // macOS native-tls (Security Framework) pops up a GUI dialog for
         // self-signed certificates, so only rustls works there without
         // manual intervention.
         all(target_os = "macos", feature = "rustls"),
         // On other OS accept run with either
         all(not(target_os = "macos"), any(feature = "native-tls", feature = "rustls")),
     ))]
    test_transport("tests/for_tcp/tls_transport.toml", Type::Tcp).await?;

    #[cfg(feature = "noise")]
    test_transport("tests/for_tcp/noise_transport.toml", Type::Tcp).await?;

    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test_transport("tests/for_tcp/websocket_transport.toml", Type::Tcp).await?;

    #[cfg(not(target_os = "macos"))]
    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test_transport("tests/for_tcp/websocket_tls_transport.toml", Type::Tcp).await?;

    Ok(())
}

#[tokio::test]
async fn udp() -> Result<()> {
    init();

    // Spawn a echo server
    tokio::spawn(async move {
        if let Err(e) = common::udp::echo_server(ECHO_SERVER_ADDR).await {
            panic!("Failed to run the echo server for testing: {e:?}");
        }
    });

    // Spawn a pingpong server
    tokio::spawn(async move {
        if let Err(e) = common::udp::pingpong_server(PINGPONG_SERVER_ADDR).await {
            panic!("Failed to run the pingpong server for testing: {e:?}");
        }
    });

    test_transport("tests/for_udp/tcp_transport.toml", Type::Udp).await?;

    #[cfg(any(
         // macOS native-tls (Security Framework) pops up a GUI dialog for
         // self-signed certificates, so only rustls works there without
         // manual intervention.
         all(target_os = "macos", feature = "rustls"),
         // On other OS accept run with either
         all(not(target_os = "macos"), any(feature = "native-tls", feature = "rustls")),
     ))]
    test_transport("tests/for_udp/tls_transport.toml", Type::Udp).await?;

    #[cfg(feature = "noise")]
    test_transport("tests/for_udp/noise_transport.toml", Type::Udp).await?;

    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test_transport("tests/for_udp/websocket_transport.toml", Type::Udp).await?;

    #[cfg(not(target_os = "macos"))]
    #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
    test_transport("tests/for_udp/websocket_tls_transport.toml", Type::Udp).await?;

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
    time::sleep(Duration::from_secs(1)).await;
    let server = tokio::spawn(async move {
        run_molehill_server("tests/for_udp/affinity_transport.toml", server_shutdown_rx)
            .await
            .unwrap();
    });
    time::sleep(Duration::from_millis(2500)).await;

    let conn = UdpSocket::bind("127.0.0.1:0").await?;
    conn.connect(AFFINITY_EXPOSED_ADDR).await?;

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
async fn test(config_path: &'static str, t: Type, mux: Option<bool>) -> Result<()> {
    if cfg!(not(all(feature = "client", feature = "server"))) {
        // Skip the test if the client or the server is not enabled
        return Ok(());
    }

    // `None` uses the fixture as-is (compiled-in mux default). When the
    // multiplex feature is present, `Some(false)` materializes an explicit
    // no-mux copy so both data paths are exercised.
    #[cfg(feature = "multiplex")]
    let (run_config, variant) = match mux {
        Some(mux) => {
            let path = write_mux_variant(config_path, mux)?;
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
    time::sleep(Duration::from_secs(1)).await;

    // Start the server
    info!("start the server");
    let server_config = run_config.clone();
    let server = tokio::spawn(async move {
        run_molehill_server(&server_config, server_shutdown_rx)
            .await
            .unwrap();
    });
    time::sleep(Duration::from_millis(2500)).await; // Wait for the client to retry

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
    time::sleep(Duration::from_secs(1)).await; // Wait for the client to start

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
    time::sleep(Duration::from_millis(2500)).await; // Wait for the client to retry

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

        conn.recv(&mut rd).await?;
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

        conn.recv(&mut rd).await?;
        debug!("pong");

        assert_eq!(rd, PONG.as_bytes());
    }

    Ok(())
}
