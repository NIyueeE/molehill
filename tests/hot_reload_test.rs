//! Hot reload of a running **client** (`hot-reload` feature): the config file
//! changes while the process runs, and the client picks the change up without
//! a restart.
//!
//! The three observable outcomes are the ones `docs/configuration.md`
//! promises, and each is asserted on the wire rather than in a log line:
//!
//! - an **added** service starts being served at its new public port;
//! - a **modified** service takes the new value — the scenario rewires the
//!   service to a second backend that answers differently, so "took the new
//!   value" is visible from the visitor's side;
//! - a **deleted** service stops being served and *releases its port* (the
//!   test binds that port again, the OS-level proof the listener is gone —
//!   the same check `finished_control_channel_releases_its_ports` uses).
//!
//! Throughout, the service that did not change keeps answering and the server
//! must not see a second control session: `control_sessions_accepted` is read
//! before the client starts and after the last change, so a "hot reload" that
//! silently restarted the instance — tearing the sessions down and rebuilding
//! them — fails even though every forwarding assertion would still pass.
//!
//! The config is written at run time on ports the OS picked: this scenario has
//! to *edit* its config file, and a stale child from an interrupted run must
//! not be able to collide with a fixed port. It is rewritten in place, which is
//! how an editor that saves over the file does it; the client's watcher
//! rescans on the modification event, so every wait below is a bounded poll
//! for the effect rather than a sleep.
#![cfg(all(feature = "client", feature = "server", feature = "hot-reload"))]
#![expect(
    clippy::unwrap_used,
    reason = "a test unwraps and asserts on values it just produced"
)]

mod common;

use anyhow::{Context, Result, bail};
use common::{run_molehill_client, run_molehill_server};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::broadcast,
    time,
};
use tracing_subscriber::EnvFilter;

/// What the two backends answer, so a probe can tell which one a service is
/// currently wired to. The reply does not depend on the request, which is the
/// point: a service's identity here is its backend, not its echo.
const ALPHA_REPLY: &[u8] = b"AAAA";
const BETA_REPLY: &[u8] = b"BBBB";

/// The request every probe sends; the backends ignore its content.
const PROBE: &[u8] = b"ping";

/// How long a hot reload may take before the test calls it a failure. The
/// watcher's rescan is event-driven and lands in milliseconds; the budget is
/// for a loaded machine, not for the mechanism.
const RELOAD_BUDGET: Duration = Duration::from_secs(15);
/// How long the server may take to drop a deregistered service's listener.
const RELEASE_BUDGET: Duration = Duration::from_secs(10);

fn init() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::from("info")),
        )
        .try_init();
}

/// Free localhost ports, asked of the OS and **held**: the listeners stay
/// alive until the caller drops them, so the config file can name ports
/// nothing else can take in between.
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

/// A backend that answers every request with one fixed reply, so a probe can
/// say *which* backend a service is wired to.
async fn fixed_reply_server(listener: TcpListener, reply: &'static [u8]) {
    loop {
        let Ok((mut conn, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            while let Ok(n) = conn.read(&mut buf).await {
                if n == 0 || conn.write_all(reply).await.is_err() {
                    break;
                }
            }
        });
    }
}

/// A fixed-reply backend on an OS-chosen port: the config names the port, and
/// the task keeps serving it for the test's lifetime.
async fn start_backend(reply: &'static [u8]) -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(fixed_reply_server(listener, reply));
    Ok(port)
}

/// One probe: connect, send [`PROBE`], read the reply.
async fn probe_once(addr: &str) -> Result<Vec<u8>> {
    let attempt = async {
        let mut conn = TcpStream::connect(addr).await?;
        conn.write_all(PROBE).await?;
        let mut rd = [0u8; 4];
        conn.read_exact(&mut rd).await?;
        Ok::<_, std::io::Error>(rd.to_vec())
    };
    time::timeout(Duration::from_millis(500), attempt)
        .await
        .context("probe timed out")?
        .context("probe failed")
}

/// Poll `addr` until it answers `expected`, failing with what it answered
/// instead — never a fixed sleep and a hope.
async fn wait_for_reply(addr: &str, expected: &[u8], budget: Duration) -> Result<()> {
    let deadline = Instant::now() + budget;
    let mut last;
    loop {
        match probe_once(addr).await {
            Ok(reply) if reply == expected => return Ok(()),
            Ok(reply) => last = String::from_utf8_lossy(&reply).into_owned(),
            Err(e) => last = e.to_string(),
        }
        if Instant::now() >= deadline {
            bail!(
                "{addr} did not answer {:?} within {budget:?}; last observation: {last:?}",
                String::from_utf8_lossy(expected)
            );
        }
        time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait until `addr` can be bound again: while the server still holds the
/// listener the bind is refused, so this is the port-release assertion.
async fn wait_for_port_release(addr: &str, budget: Duration) -> Result<()> {
    let deadline = Instant::now() + budget;
    loop {
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                drop(listener);
                return Ok(());
            }
            Err(e) => {
                if Instant::now() >= deadline {
                    bail!("{addr} was still bound {budget:?} after the service was deleted: {e}");
                }
                time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// The server side: one control listener and an `allow_ports` entry per
/// service the client will register. Two single-port entries, not one range:
/// the ports come from the OS in whatever order it picked, and a reversed
/// range is a configuration error.
fn write_server_config(dir: &Path, control: u16, allowed: [u16; 2]) -> Result<PathBuf> {
    let path = dir.join("server.toml");
    std::fs::write(
        &path,
        format!(
            "[server]\n\
             default_token = \"hot_reload_token\"\n\
             allow_ports = [\"{}\", \"{}\"]\n\
             \n\
             [server.control]\n\
             bind_addr = \"127.0.0.1:{control}\"\n",
            allowed[0], allowed[1]
        ),
    )?;
    Ok(path)
}

/// Rewrite the client config with exactly the given `(name, local, remote)`
/// services — the "the user saved the file" step of every reload below.
fn write_client_config(path: &Path, control: u16, services: &[(&str, u16, u16)]) -> Result<()> {
    let mut cfg = format!(
        "[client]\n\
         default_token = \"hot_reload_token\"\n\
         \n\
         [client.control]\n\
         default_remote_addr = \"127.0.0.1:{control}\"\n\
         \n\
         [client.transport]\n\
         type = \"plain\"\n"
    );
    for (name, local, remote) in services {
        write!(
            cfg,
            "\n[client.services.{name}]\n\
             local_addr = \"127.0.0.1:{local}\"\n\
             remote_bind_addr = \"127.0.0.1:{remote}\"\n"
        )?;
    }
    std::fs::write(path, cfg)?;
    Ok(())
}

/// The running pair plus everything the scenarios need to edit its config and
/// stop it again.
struct Running {
    dir: PathBuf,
    config: PathBuf,
    control: u16,
    alpha: u16,
    beta: u16,
    backend_a: u16,
    backend_b: u16,
    client_shutdown: broadcast::Sender<bool>,
    server_shutdown: broadcast::Sender<bool>,
    client: tokio::task::JoinHandle<()>,
    server: tokio::task::JoinHandle<()>,
}

impl Running {
    fn alpha_addr(&self) -> String {
        format!("127.0.0.1:{}", self.alpha)
    }

    fn beta_addr(&self) -> String {
        format!("127.0.0.1:{}", self.beta)
    }

    /// The client config with `alpha` and `beta`, each pointing at the given
    /// backend, written in place.
    fn write(&self, services: &[(&str, u16, u16)]) -> Result<()> {
        write_client_config(&self.config, self.control, services)
    }

    async fn stop(self) {
        let _ = self.server_shutdown.send(true);
        let _ = self.client_shutdown.send(true);
        let _ = tokio::join!(self.server, self.client);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Start a pair whose config file the caller can keep editing: the backends
/// and the two exposed ports come from the OS, and `alpha` is the only service
/// in the initial file.
async fn start_pair() -> Result<Running> {
    let ([control, alpha, beta], held) = free_ports::<3>();
    let backend_a = start_backend(ALPHA_REPLY).await?;
    let backend_b = start_backend(BETA_REPLY).await?;

    let dir = std::env::temp_dir().join(format!("molehill-hot-reload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let server_config = write_server_config(&dir, control, [alpha, beta])?;
    let config = dir.join("client.toml");
    write_client_config(&config, control, &[("alpha", backend_a, alpha)])?;

    // The server needs the control and public ports now; the client dials out
    // and binds nothing. Releasing the reservations is what lets the server
    // bind them, and a lost race fails the first readiness probe loudly.
    drop(held);

    let (client_shutdown, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown, server_shutdown_rx) = broadcast::channel(1);
    let client_path = config.to_string_lossy().into_owned();
    let client = tokio::spawn(async move {
        run_molehill_client(&client_path, client_shutdown_rx)
            .await
            .unwrap();
    });
    let server_path = server_config.to_string_lossy().into_owned();
    let server = tokio::spawn(async move {
        run_molehill_server(&server_path, server_shutdown_rx)
            .await
            .unwrap();
    });

    let running = Running {
        dir,
        config,
        control,
        alpha,
        beta,
        backend_a,
        backend_b,
        client_shutdown,
        server_shutdown,
        client,
        server,
    };
    wait_for_reply(&running.alpha_addr(), ALPHA_REPLY, RELOAD_BUDGET).await?;
    Ok(running)
}

/// Add, modify and delete services in a running client's config file: each
/// change must take effect on the wire, the untouched service must keep
/// working, and the whole sequence must stay on one control session.
#[tokio::test]
async fn a_running_client_applies_add_modify_and_delete() -> Result<()> {
    init();

    let sessions_before = molehill_rathole::control_sessions_accepted();
    let running = start_pair().await?;

    // Add: the file gains a second service, which must start being served
    // while the first one keeps answering.
    running.write(&[
        ("alpha", running.backend_a, running.alpha),
        ("beta", running.backend_b, running.beta),
    ])?;
    wait_for_reply(&running.beta_addr(), BETA_REPLY, RELOAD_BUDGET).await?;
    wait_for_reply(&running.alpha_addr(), ALPHA_REPLY, RELOAD_BUDGET).await?;

    // Modify: `alpha` is rewired to the other backend, on the same public port.
    // The changed answer is what proves the new value was applied rather than
    // the old service kept running.
    running.write(&[
        ("alpha", running.backend_b, running.alpha),
        ("beta", running.backend_b, running.beta),
    ])?;
    wait_for_reply(&running.alpha_addr(), BETA_REPLY, RELOAD_BUDGET).await?;
    wait_for_reply(&running.beta_addr(), BETA_REPLY, RELOAD_BUDGET).await?;

    // Delete: `beta` is gone from the file. Its port must be released — the
    // OS-level proof that the server dropped the listener — and `alpha`, which
    // did not change, must still be served.
    running.write(&[("alpha", running.backend_b, running.alpha)])?;
    wait_for_port_release(&running.beta_addr(), RELEASE_BUDGET).await?;
    wait_for_reply(&running.alpha_addr(), BETA_REPLY, RELOAD_BUDGET).await?;

    // One session for the whole sequence: the server saw the client connect
    // once. A client that restarted on any of these edits would have opened a
    // second one, and the forwarding assertions above could not tell.
    let sessions = molehill_rathole::control_sessions_accepted() - sessions_before;
    assert_eq!(
        sessions, 1,
        "the client's hot reloads must stay on one control session, not restart the instance"
    );

    running.stop().await;
    Ok(())
}
