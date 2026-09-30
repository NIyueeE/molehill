//! Protocol v4 session tests: this file is the v4 **client half**, spoken by
//! hand over a raw `TcpStream` against the real in-process server.
//!
//! The crate's `protocol` module is private, so every frame below is mirrored
//! here — which is the point: this is a second, independent reading of the wire
//! contract, so a change to a variant index, a field order or a frame width has
//! to be made twice. It covers the control plane (session auth, `Ack::SessionOk`
//! with its declared cadence, N services on one connection, per-service
//! rejection, `Deregister` releasing a port) and one *direct* v4 data channel,
//! whose service binding is the 4-byte prologue after the hello.
//!
//! The multiplexed tunnel's per-stream prologue is covered by the
//! `tunnel_streams_route_by_their_prologue` unit test in `src/core/server.rs`:
//! the yamux engine is crate-private, so an integration test cannot open a
//! tunnel, and the real v4 client (its own commit) is what exercises that path
//! end to end.
//!
//! Run serially (`--test-threads=1`): every scenario binds fixed ports. The
//! existing suites own 2333-2351 (control + exposed) and 8080-8099 (backends);
//! this file uses 2360 (control, which is also the data-plane listener) and
//! 2361-2364 (exposed services), with backends on 8100-8101.
#![expect(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "a test unwraps, expects and asserts on values it just produced"
)]

mod common;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::time::{sleep, timeout};
use tracing_subscriber::EnvFilter;

use crate::common::{PING, PONG, run_molehill_server};

/// Control address of the fixture server. The default `[server.data]`
/// configuration puts the data plane on the same listener, so data channels
/// and tunnels dial this address too.
const CONTROL_ADDR: &str = "127.0.0.1:2360";
/// Echo backends the data channels are bridged to.
const BACKEND_A: &str = "127.0.0.1:8100";
const BACKEND_B: &str = "127.0.0.1:8101";
/// Exposed services, all inside the fixture's `allow_ports`; `EXPOSED_PROBE`
/// is only ever registered by the "the session is still alive" checks.
const EXPOSED_A: u16 = 2361;
const EXPOSED_B: u16 = 2362;
const EXPOSED_PROBE: u16 = 2363;
/// Deliberately *outside* the fixture's `allow_ports` whitelist.
const DISALLOWED_PORT: u16 = 2364;

/// The fixture the other scenarios start (`session_v4.toml`).
const SESSION_CONFIG: &str = "tests/for_tcp/session_v4.toml";

/// The striped v4 fixture (`session_v4_striped.toml`): the same server as
/// [`SESSION_CONFIG`] with `[server.data] stripe_count = 2`, on ports of its own
/// — 2364 and below belong to the scenarios above, 2365 onwards is free.
///
/// `[server.data]` is part of the configuration surface only when the
/// `multiplex` feature is compiled in (`ServerDataConfig` is feature-gated), so
/// this fixture — and the scenario that reads it — belongs to that feature.
#[cfg(feature = "multiplex")]
const STRIPED_CONFIG: &str = "tests/for_tcp/session_v4_striped.toml";
#[cfg(feature = "multiplex")]
const STRIPED_CONTROL: &str = "127.0.0.1:2365";
#[cfg(feature = "multiplex")]
const STRIPED_EXPOSED: u16 = 2366;
/// The service id the striped scenario registers.
#[cfg(feature = "multiplex")]
const STRIPE_SERVICE: u32 = 1;

const DEFAULT_TOKEN: &str = "session_test_default_token";
const WRONG_TOKEN: &str = "not_the_server_token";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Plain transport selector (the first byte of every connection).
const PLAIN_SELECTOR: u8 = 0x00;
/// `CURRENT_PROTO_VERSION` of the client half of this change; the v4 dialect.
const PROTO_V4: u8 = 4;

type Digest = [u8; 32];

// --- the wire contract, mirrored ------------------------------------------
//
// Variant order and field order are the contract: postcard encodes an enum
// variant as its index and a struct as its fields in declaration order.

#[expect(clippy::enum_variant_names, reason = "the wire's variant names")]
#[derive(Serialize, Deserialize, Debug)]
enum Hello {
    ControlChannelHello(u8, Digest),
    DataChannelHello(u8, Digest),
    DataChannelTunnelHello(u8, Digest),
}

/// `Auth` is a newtype, so it goes on the wire as the bare digest.
#[derive(Serialize)]
struct Auth(Digest);

#[derive(Serialize, Deserialize, Debug)]
enum Ack {
    Ok,
    AuthFailed,
    RegisterRejected(String),
    SessionOk { heartbeat_interval_secs: u64 },
}

#[derive(Serialize, Debug, Clone, Copy)]
enum ServiceType {
    Tcp,
    Udp,
}

#[derive(Serialize, Debug, Clone, Copy)]
enum Carrier {
    Tcp,
    Kcp,
}

#[derive(Serialize, Debug, Clone)]
struct ServiceRegistrationV4 {
    name: String,
    service_type: ServiceType,
    bind_addr: SocketAddr,
    carrier: Carrier,
    udp_buffer_size: u16,
}

#[derive(Serialize, Debug, Clone)]
struct SessionRegistration {
    service_id: [u8; 4],
    auth: Digest,
    reg: ServiceRegistrationV4,
}

#[derive(Serialize, Debug, Clone)]
enum SessionCmd {
    Register(SessionRegistration),
    Deregister([u8; 4]),
}

/// One control-channel command, as the server sends it.
#[derive(Debug, PartialEq, Eq)]
enum ControlCmd {
    CreateDataChannel,
    HeartBeat,
    CreateDataChannelFor(u32),
    ServiceDropped(u32),
    /// The group request: service id, group id, stripe index, stripe count.
    CreateDataChannelForStripe(u32, [u8; 4], u8, u8),
}

// --- fixtures --------------------------------------------------------------

fn init() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::from("info")),
        )
        .try_init();
}

fn spawn_backends() {
    tokio::spawn(async move {
        if let Err(e) = common::tcp::pingpong_server(BACKEND_A).await {
            panic!("Failed to run the backend at {BACKEND_A}: {e:?}");
        }
    });
    tokio::spawn(async move {
        if let Err(e) = common::tcp::pingpong_server(BACKEND_B).await {
            panic!("Failed to run the backend at {BACKEND_B}: {e:?}");
        }
    });
}

/// Start the fixture server and wait until its control listener accepts.
///
/// The wait dials once and drops the socket, which the server logs as one
/// failed hello read — cheaper than racing every scenario against a fixed
/// startup sleep.
async fn start_server() -> Result<broadcast::Sender<bool>> {
    start_server_at(SESSION_CONFIG, CONTROL_ADDR).await
}

/// The same for a fixture with its own control port: the striped v4 scenario
/// runs its own server so that `stripe_count` cannot change what every other
/// scenario in this file exercises.
async fn start_server_at(config: &str, control_addr: &str) -> Result<broadcast::Sender<bool>> {
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let config = config.to_owned();
    tokio::spawn(async move {
        if let Err(e) = run_molehill_server(&config, shutdown_rx).await {
            panic!("the session fixture server failed: {e:#}");
        }
    });

    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    while TcpStream::connect(control_addr).await.is_err() {
        if Instant::now() > deadline {
            bail!("the control listener at {control_addr} never came up");
        }
        sleep(Duration::from_millis(50)).await;
    }
    Ok(shutdown_tx)
}

fn key_for(token: &str, nonce: &Digest) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher.update(nonce);
    hasher.finalize().into()
}

async fn write_frame(conn: &mut TcpStream, payload: &[u8]) -> Result<()> {
    let len = u16::try_from(payload.len()).context("frame longer than the u16 prefix")?;
    conn.write_u16(len).await?;
    conn.write_all(payload).await?;
    conn.flush().await?;
    Ok(())
}

async fn read_frame(conn: &mut TcpStream) -> Result<Vec<u8>> {
    let len = conn.read_u16().await?;
    let mut buf = vec![0u8; usize::from(len)];
    conn.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Read one control-channel command (tag-dispatched, exactly as the client
/// does: the tag alone decides how many bytes follow).
async fn read_control_cmd(conn: &mut TcpStream) -> Result<ControlCmd> {
    let tag = conn.read_u8().await?;
    match tag {
        0 => Ok(ControlCmd::CreateDataChannel),
        1 => Ok(ControlCmd::HeartBeat),
        2 | 3 => {
            let mut raw = [0u8; 4];
            conn.read_exact(&mut raw).await?;
            let id = u32::from_be_bytes(raw);
            Ok(if tag == 2 {
                ControlCmd::CreateDataChannelFor(id)
            } else {
                ControlCmd::ServiceDropped(id)
            })
        }
        4 => {
            let mut raw = [0u8; 10];
            conn.read_exact(&mut raw).await?;
            Ok(ControlCmd::CreateDataChannelForStripe(
                u32::from_be_bytes(raw[0..4].try_into()?),
                raw[4..8].try_into()?,
                raw[8],
                raw[9],
            ))
        }
        other => bail!("unknown control channel command tag {other:#x}"),
    }
}

/// Open a connection and complete the v4 hello exchange up to (but not
/// including) the auth response; the nonce is the server's session identity.
async fn open_session() -> Result<(TcpStream, Digest)> {
    let mut conn = TcpStream::connect(CONTROL_ADDR).await?;
    conn.write_u8(PLAIN_SELECTOR).await?;
    let hello = Hello::ControlChannelHello(PROTO_V4, [0x42; 32]);
    conn.write_all(&postcard::to_stdvec(&hello)?).await?;
    conn.flush().await?;

    let mut buf = [0u8; 34];
    timeout(HANDSHAKE_TIMEOUT, conn.read_exact(&mut buf)).await??;
    let Hello::ControlChannelHello(version, nonce) =
        postcard::from_bytes::<Hello>(&buf).context("failed to parse the server hello")?
    else {
        bail!("the server answered a control hello with another hello variant");
    };
    assert_eq!(
        version, PROTO_V4,
        "the server must answer in the v4 dialect"
    );
    Ok((conn, nonce))
}

// --- the v4 client ---------------------------------------------------------

/// One v4 control session, as this test's client sees it.
struct Session {
    conn: TcpStream,
    nonce: Digest,
    /// The session credential (`digest(default_token ‖ nonce)`), which the
    /// server also accepts as each service's credential — it owns no
    /// per-service token table.
    key: Digest,
}

impl Session {
    /// Authenticate and expect `Ack::SessionOk`.
    async fn connect() -> Result<Session> {
        let (mut conn, nonce) = open_session().await?;
        let key = key_for(DEFAULT_TOKEN, &nonce);
        conn.write_all(&postcard::to_stdvec(&Auth(key))?).await?;
        conn.flush().await?;

        let ack: Ack = postcard::from_bytes(&read_frame(&mut conn).await?)?;
        let Ack::SessionOk {
            heartbeat_interval_secs,
        } = ack
        else {
            bail!("expected Ack::SessionOk, got {ack:?}");
        };
        // The fixture declares 5 s (`[server.control].heartbeat_interval`).
        assert_eq!(
            heartbeat_interval_secs, 5,
            "the session ack must declare the server's configured cadence"
        );
        Ok(Session { conn, nonce, key })
    }

    /// Register one TCP service on this session and return the server's
    /// verdict. `token` is the *service's* credential.
    async fn register(&mut self, id: u32, name: &str, port: u16, token: &str) -> Result<Ack> {
        let reg = ServiceRegistrationV4 {
            name: name.to_owned(),
            service_type: ServiceType::Tcp,
            bind_addr: format!("0.0.0.0:{port}").parse()?,
            carrier: Carrier::Tcp,
            udp_buffer_size: 2048,
        };
        let cmd = SessionCmd::Register(SessionRegistration {
            service_id: id.to_be_bytes(),
            // The server owns no per-service token table, so the endpoint's own
            // credential (the session key) is what a default-token service
            // proves.
            auth: if token == DEFAULT_TOKEN {
                self.key
            } else {
                key_for(token, &self.nonce)
            },
            reg,
        });
        write_frame(&mut self.conn, &postcard::to_stdvec(&cmd)?).await?;
        Ok(postcard::from_bytes(&read_frame(&mut self.conn).await?)?)
    }

    async fn deregister(&mut self, id: u32) -> Result<()> {
        let cmd = SessionCmd::Deregister(id.to_be_bytes());
        write_frame(&mut self.conn, &postcard::to_stdvec(&cmd)?).await
    }

    /// Open a *direct* v4 data channel for one service: the same 34-byte hello
    /// as v3, then the 4-byte service prologue.
    async fn open_data_channel(&self, id: u32) -> Result<TcpStream> {
        let mut conn = TcpStream::connect(CONTROL_ADDR).await?;
        conn.write_u8(PLAIN_SELECTOR).await?;
        let hello = Hello::DataChannelHello(PROTO_V4, self.nonce);
        conn.write_all(&postcard::to_stdvec(&hello)?).await?;
        conn.write_all(&id.to_be_bytes()).await?;
        conn.flush().await?;
        Ok(conn)
    }

    /// Wait for the server's request for one data channel of `id`; heartbeats
    /// are skipped.
    async fn wait_for_data_channel(&mut self, id: u32) -> Result<()> {
        loop {
            match timeout(PROBE_TIMEOUT, read_control_cmd(&mut self.conn)).await?? {
                ControlCmd::CreateDataChannelFor(got) if got == id => return Ok(()),
                // A session heartbeat is not this test's business; keep reading.
                ControlCmd::HeartBeat => {}
                other => bail!("expected a data-channel request for service {id}, got {other:?}"),
            }
        }
    }

    /// The session is still usable: a registration answered `Ack::Ok` proves
    /// the control connection survived whatever happened before.
    async fn assert_alive(&mut self, id: u32, port: u16) -> Result<()> {
        let ack = self
            .register(id, "still-alive", port, DEFAULT_TOKEN)
            .await?;
        assert!(
            matches!(ack, Ack::Ok),
            "the session should still register services, got {ack:?}"
        );
        Ok(())
    }
}

/// One visitor round trip: connect to the exposed port, send PING, expect the
/// backend's PONG.
async fn visitor_round_trip(exposed: u16) -> Result<()> {
    let mut visitor = TcpStream::connect(("127.0.0.1", exposed)).await?;
    visitor.write_all(PING.as_bytes()).await?;
    let mut reply = [0u8; 4];
    timeout(PROBE_TIMEOUT, visitor.read_exact(&mut reply)).await??;
    assert_eq!(&reply, PONG.as_bytes(), "the visitor was not forwarded");
    Ok(())
}

/// Bridge one established data channel to a backend until either side ends.
async fn bridge(mut channel: TcpStream, backend: &'static str) -> Result<()> {
    let mut backend_conn = TcpStream::connect(backend).await?;
    tokio::io::copy_bidirectional(&mut channel, &mut backend_conn).await?;
    Ok(())
}

/// Drive one full visitor round trip over a direct data channel of `id`:
/// visit the exposed port, answer the server's data-channel request, consume
/// the 1-byte `StartForwardTcp`, then bridge to the backend.
async fn direct_round_trip(session: &mut Session, id: u32, exposed: u16, backend: &'static str) {
    let visitor = tokio::spawn(visitor_round_trip(exposed));
    session
        .wait_for_data_channel(id)
        .await
        .expect("the server never asked for a data channel");
    let mut channel = session
        .open_data_channel(id)
        .await
        .expect("failed to open a data channel");
    let mut cmd = [0u8; 1];
    timeout(PROBE_TIMEOUT, channel.read_exact(&mut cmd))
        .await
        .expect("the server never sent StartForwardTcp")
        .expect("failed to read StartForwardTcp");
    assert_eq!(cmd, [0x00], "the first data command is StartForwardTcp");
    tokio::spawn(bridge(channel, backend));
    visitor
        .await
        .expect("the visitor task panicked")
        .expect("the visitor was not forwarded");
}

/// Poll until binding `port` succeeds (the previous owner released it), then
/// release it again.
///
/// The probe binds the **wildcard** address, because that is what the server
/// binds for a registered service: a `127.0.0.1` probe is not evidence of
/// occupancy on a BSD-derived host, where `SO_REUSEADDR` (which
/// `TcpListener::bind` sets) lets a specific-address bind coexist with a
/// wildcard one. macOS caught exactly that — `a registered service must hold
/// its port` passed on Linux and failed there.
async fn wait_for_free_port(port: u16) -> Result<()> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(l) => {
                drop(l);
                return Ok(());
            }
            Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(50)).await,
            Err(e) => bail!("port {port} was never released: {e}"),
        }
    }
}

/// The mirror is total over the contract, and the widths it fixes are pinned
/// here: every hello variant is 34 bytes (the server reads that before it knows
/// the version), every enum tag is one byte, and the failure ack the server
/// sends outside the framed path is exactly one byte.
#[test]
fn the_wire_mirror_is_total() {
    let tag = [0u8; 32];
    for hello in [
        Hello::ControlChannelHello(PROTO_V4, tag),
        Hello::DataChannelHello(PROTO_V4, tag),
        Hello::DataChannelTunnelHello(PROTO_V4, tag),
    ] {
        assert_eq!(postcard::to_stdvec(&hello).unwrap().len(), 34);
    }
    for carrier in [Carrier::Tcp, Carrier::Kcp] {
        assert_eq!(postcard::to_stdvec(&carrier).unwrap().len(), 1);
    }
    for service_type in [ServiceType::Tcp, ServiceType::Udp] {
        assert_eq!(postcard::to_stdvec(&service_type).unwrap().len(), 1);
    }
    assert_eq!(postcard::to_stdvec(&Ack::AuthFailed).unwrap().len(), 1);
    assert_eq!(postcard::to_stdvec(&Ack::Ok).unwrap().len(), 1);
}

// --- scenarios -------------------------------------------------------------

/// One session, two services, one control connection: both exposed ports
/// forward traffic, and each gets its own data channel.
#[tokio::test]
async fn one_session_registers_two_services_and_both_forward() -> Result<()> {
    init();
    spawn_backends();
    let _server = start_server().await?;

    let mut session = Session::connect().await?;
    assert!(matches!(
        session
            .register(1, "svc-a", EXPOSED_A, DEFAULT_TOKEN)
            .await?,
        Ack::Ok
    ));
    assert!(matches!(
        session
            .register(2, "svc-b", EXPOSED_B, DEFAULT_TOKEN)
            .await?,
        Ack::Ok
    ));

    direct_round_trip(&mut session, 1, EXPOSED_A, BACKEND_A).await;
    direct_round_trip(&mut session, 2, EXPOSED_B, BACKEND_B).await;

    // Both services are still registered on the one session afterwards.
    session.assert_alive(3, EXPOSED_PROBE).await?;
    Ok(())
}

/// A service outside `allow_ports` is rejected on its own: the rejection is a
/// per-service ack, and the session keeps serving the valid one.
#[tokio::test]
async fn a_rejected_port_does_not_kill_the_session() -> Result<()> {
    init();
    spawn_backends();
    let _server = start_server().await?;

    let mut session = Session::connect().await?;
    match session
        .register(1, "outside-the-whitelist", DISALLOWED_PORT, DEFAULT_TOKEN)
        .await?
    {
        Ack::RegisterRejected(reason) => {
            assert!(
                reason.contains("allow_ports"),
                "the reason should name the policy, got {reason:?}"
            );
        }
        other => panic!("expected Ack::RegisterRejected, got {other:?}"),
    }

    // The session is alive and the valid service works.
    assert!(matches!(
        session
            .register(2, "svc-a", EXPOSED_A, DEFAULT_TOKEN)
            .await?,
        Ack::Ok
    ));
    direct_round_trip(&mut session, 2, EXPOSED_A, BACKEND_A).await;
    Ok(())
}

/// The wrong *session* token is refused with `Ack::AuthFailed` (the bare
/// fixed-width ack) and the connection is closed.
#[tokio::test]
async fn a_wrong_session_token_is_refused_and_the_connection_closes() -> Result<()> {
    init();
    let _server = start_server().await?;

    let (mut conn, nonce) = open_session().await?;
    let wrong = key_for(WRONG_TOKEN, &nonce);
    conn.write_all(&postcard::to_stdvec(&Auth(wrong))?).await?;
    conn.flush().await?;

    let mut ack = [0u8; 1];
    timeout(HANDSHAKE_TIMEOUT, conn.read_exact(&mut ack)).await??;
    assert_eq!(ack[0], 1, "Ack::AuthFailed is variant index 1");

    // A failed session auth ends the connection: no session, no more bytes.
    let mut after = [0u8; 1];
    let n = timeout(HANDSHAKE_TIMEOUT, conn.read(&mut after)).await??;
    assert_eq!(n, 0, "the server must close after a failed session auth");
    Ok(())
}

/// The wrong *service* token rejects that service only; its sibling keeps
/// working on the same session.
#[tokio::test]
async fn a_wrong_service_token_rejects_only_that_service() -> Result<()> {
    init();
    spawn_backends();
    let _server = start_server().await?;

    let mut session = Session::connect().await?;
    match session.register(1, "svc-a", EXPOSED_A, WRONG_TOKEN).await? {
        Ack::RegisterRejected(reason) => {
            assert!(
                reason.contains("token"),
                "the reason should name the credential, got {reason:?}"
            );
        }
        other => panic!("expected Ack::RegisterRejected, got {other:?}"),
    }
    // The rejected service was never bound.
    assert!(
        TcpListener::bind(("0.0.0.0", EXPOSED_A)).await.is_ok(),
        "a rejected service must not hold its port"
    );

    assert!(matches!(
        session
            .register(2, "svc-b", EXPOSED_B, DEFAULT_TOKEN)
            .await?,
        Ack::Ok
    ));
    direct_round_trip(&mut session, 2, EXPOSED_B, BACKEND_B).await;
    Ok(())
}

/// `Deregister` drops that service's listener and releases its port, while the
/// session stays up.
#[tokio::test]
async fn deregister_releases_the_port() -> Result<()> {
    init();
    let _server = start_server().await?;

    let mut session = Session::connect().await?;
    assert!(matches!(
        session
            .register(1, "svc-a", EXPOSED_A, DEFAULT_TOKEN)
            .await?,
        Ack::Ok
    ));
    assert!(
        TcpListener::bind(("0.0.0.0", EXPOSED_A)).await.is_err(),
        "a registered service must hold its port"
    );

    session.deregister(1).await?;
    wait_for_free_port(EXPOSED_A).await?;

    // The session itself never noticed.
    session.assert_alive(2, EXPOSED_B).await?;
    Ok(())
}

/// A striped visitor's channels are asked for **by group**: a hand-written peer
/// sees one `CreateDataChannelForStripe` per stripe, all naming the same group,
/// with the stripe's own index and the group's count.
///
/// The client is covered by `integration_test.rs::striped_data_channels`; this
/// is the request vocabulary on its own, against a real server, so a change to
/// what the server asks for cannot hide behind the client that reads it. The
/// channels are never opened: this file's peer is a wire mirror, not a data
/// plane.
///
/// `multiplex`-only, like the fixture it starts: a build without that feature
/// has no `[server.data]` to put a stripe count in.
#[cfg(feature = "multiplex")]
#[tokio::test]
async fn a_striped_gather_names_its_group_on_every_request() -> Result<()> {
    init();
    let _server = start_server_at(STRIPED_CONFIG, STRIPED_CONTROL).await?;

    // The hello exchange, by hand, on the striped fixture's port.
    let mut conn = TcpStream::connect(STRIPED_CONTROL).await?;
    conn.write_u8(PLAIN_SELECTOR).await?;
    let hello = Hello::ControlChannelHello(PROTO_V4, [0x42; 32]);
    conn.write_all(&postcard::to_stdvec(&hello)?).await?;
    conn.flush().await?;
    let mut buf = [0u8; 34];
    timeout(HANDSHAKE_TIMEOUT, conn.read_exact(&mut buf)).await??;
    let Hello::ControlChannelHello(version, nonce) =
        postcard::from_bytes::<Hello>(&buf).context("failed to parse the server hello")?
    else {
        bail!("the server answered a control hello with another hello variant");
    };
    assert_eq!(version, PROTO_V4, "the server speaks protocol v4");

    // Authenticate, then register one service — the same frames `Session`
    // writes, and the same session key for a default-token service.
    let key = key_for(DEFAULT_TOKEN, &nonce);
    conn.write_all(&postcard::to_stdvec(&Auth(key))?).await?;
    conn.flush().await?;
    let ack: Ack = postcard::from_bytes(&read_frame(&mut conn).await?)?;
    assert!(matches!(ack, Ack::SessionOk { .. }), "got {ack:?}");
    let cmd = SessionCmd::Register(SessionRegistration {
        service_id: STRIPE_SERVICE.to_be_bytes(),
        auth: key,
        reg: ServiceRegistrationV4 {
            name: "striped".to_owned(),
            service_type: ServiceType::Tcp,
            bind_addr: format!("0.0.0.0:{STRIPED_EXPOSED}").parse()?,
            carrier: Carrier::Tcp,
            udp_buffer_size: 2048,
        },
    });
    write_frame(&mut conn, &postcard::to_stdvec(&cmd)?).await?;
    let ack: Ack = postcard::from_bytes(&read_frame(&mut conn).await?)?;
    assert!(matches!(ack, Ack::Ok), "registration refused: {ack:?}");

    // One visitor makes the server gather the fixture's 2-stripe group, and the
    // gather asks for every stripe before its first wait.
    let visitor = TcpStream::connect(("127.0.0.1", STRIPED_EXPOSED)).await?;
    let mut requests = Vec::new();
    while requests.len() < 2 {
        match timeout(PROBE_TIMEOUT, read_control_cmd(&mut conn)).await?? {
            ControlCmd::CreateDataChannelForStripe(id, group, index, count) => {
                requests.push((id, group, index, count));
            }
            // The session's declared cadence is not this test's business.
            ControlCmd::HeartBeat => {}
            other => bail!("a stripe was asked for with {other:?}, which names no group"),
        }
    }
    let (id0, group0, index0, count0) = requests[0];
    let (id1, group1, index1, count1) = requests[1];
    assert_eq!(
        (id0, id1),
        (STRIPE_SERVICE, STRIPE_SERVICE),
        "each request names the service the group serves"
    );
    assert_eq!(group0, group1, "one gather is one group");
    assert_eq!(
        (index0, index1),
        (0, 1),
        "the stripes are asked for in index order"
    );
    assert_eq!((count0, count1), (2, 2), "the count is the fixture's own");
    drop(visitor);
    Ok(())
}

/// A direct data channel naming a service that is not registered is dropped,
/// and the session (with its registered service) stays usable.
#[tokio::test]
async fn a_data_channel_for_an_unknown_service_is_dropped() -> Result<()> {
    init();
    spawn_backends();
    let _server = start_server().await?;

    let mut session = Session::connect().await?;
    assert!(matches!(
        session
            .register(1, "svc-a", EXPOSED_A, DEFAULT_TOKEN)
            .await?,
        Ack::Ok
    ));

    // Service 9 was never registered: the server reads the prologue, finds no
    // such service and drops the connection without an answer, and without
    // touching the session or service 1.
    let mut orphan = session.open_data_channel(9).await?;
    let mut buf = [0u8; 1];
    match timeout(Duration::from_millis(500), orphan.read(&mut buf)).await {
        Ok(Ok(0)) | Err(_) => {}
        Ok(Ok(n)) => panic!("the orphan channel got {n} bytes; it must be dropped"),
        Ok(Err(e)) => bail!("reading the orphan channel failed: {e}"),
    }

    direct_round_trip(&mut session, 1, EXPOSED_A, BACKEND_A).await;
    Ok(())
}
