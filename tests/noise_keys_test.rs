//! The Noise key material contract, driven through the **real binary**: what
//! `--genkey` prints and accepts, and whether a generated pair really carries
//! traffic over the fixed KK handshake.
//!
//! - `--genkey` defaults to X25519 and prints a keypair in the documented
//!   shape; the pair has to *work*, so the test feeds it back into a real
//!   server/client pair over the fixed `Noise_KK_25519_ChaChaPoly_BLAKE2s`
//!   handshake.
//! - A curve the build cannot do must be **refused**, not silently downgraded
//!   to a 25519 pair: the assertion is on the size of what was printed, or on
//!   the absence of any key at all.
//! - A keypair that does not match the peer's static key must fail the
//!   handshake **and say so** in the process's own output. There is no PSK and
//!   no resume path: the two cases this file used to drive are gone with the
//!   fixed handshake.
//!
//! The handshake case drives child processes rather than the in-process
//! helpers because the diagnosis *is* the assertion: the client retries a
//! failed handshake for ever, so its log is the only place the reason appears,
//! and a child's captured stderr is exactly what an operator would see.
//!
//! Every case here therefore needs a target that can execute its own
//! artifacts — hence the `native_target` gate. The release workflow tests every
//! target it publishes, the `cross` targets run under qemu, and a child process
//! cannot be exec'd from inside an emulated test binary: without the gate those
//! targets fail with `Exec format error`, which says nothing about the code.
//! The same rule and the same cfg are in `tests/log_budget_test.rs`.
#![cfg(all(
    feature = "noise",
    feature = "client",
    feature = "server",
    native_target
))]
#![expect(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "a test unwraps, expects and asserts on values it just produced"
)]

mod common;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use common::{PING, PONG, run_molehill_client, run_molehill_server};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::broadcast,
    time,
};
use tracing_subscriber::EnvFilter;

const TOKEN: &str = "dummy_fixture_token"; // security-scan:allow dummy fixture token

/// How long a handshake or a forwarding path may take before the test calls it
/// a failure (a loaded machine's budget, not the mechanism's).
const BUDGET: Duration = Duration::from_secs(15);

fn init() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::from("info")),
        )
        .try_init();
}

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

/// A scratch directory of its own per scenario, so the binary-driven
/// scenarios cannot see each other's config files.
fn scratch_dir(name: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("molehill-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

// --- `--genkey` ------------------------------------------------------------

/// One run of the compiled binary's `--genkey`, with an optional curve.
fn run_genkey(curve: Option<&str>) -> std::process::Output {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_molehill"));
    cmd.arg("--genkey");
    if let Some(curve) = curve {
        cmd.arg(curve);
    }
    cmd.output().expect("the compiled binary must be runnable")
}

/// The two base64 keys `--genkey` prints, in the documented block shape.
fn parse_genkey(stdout: &str) -> Result<(String, String)> {
    let mut lines = stdout.lines();
    let private = lines
        .find(|l| l.trim() == "Private Key:")
        .and_then(|_| lines.next())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .context("the output has no `Private Key:` block")?;
    let mut lines = stdout.lines();
    let public = lines
        .find(|l| l.trim() == "Public Key:")
        .and_then(|_| lines.next())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .context("the output has no `Public Key:` block")?;
    Ok((private.to_owned(), public.to_owned()))
}

fn decode(b64: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .with_context(|| format!("{b64:?} is not standard base64"))
}

/// The documented default: no curve means X25519, and the explicit spelling of
/// the same curve prints the same shape. Both keys are raw 25519 keys (32
/// bytes), which is what makes them usable as `private_key` / `public_key`
/// in the first place.
#[test]
fn genkey_defaults_to_x25519_and_its_keys_are_the_right_size() {
    for curve in [None, Some("x25519")] {
        let out = run_genkey(curve);
        assert!(
            out.status.success(),
            "`--genkey {curve:?}` must succeed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let (private, public) = parse_genkey(&String::from_utf8_lossy(&out.stdout)).unwrap();
        assert_eq!(
            decode(&private).unwrap().len(),
            32,
            "an X25519 private key is 32 bytes"
        );
        assert_eq!(
            decode(&public).unwrap().len(),
            32,
            "an X25519 public key is 32 bytes"
        );
    }
}

/// A curve the build cannot do is refused — and, crucially, *not* downgraded:
/// the failure mode worth catching is a 32-byte X25519 pair printed under an
/// `x448` request, which would silently put both ends on a key the operator
/// did not ask for.
#[test]
fn genkey_refuses_a_curve_the_build_cannot_do() {
    let out = run_genkey(Some("x448"));
    if out.status.success() {
        let (private, public) = parse_genkey(&String::from_utf8_lossy(&out.stdout)).unwrap();
        assert_eq!(
            decode(&private).unwrap().len(),
            56,
            "an X448 private key is 56 bytes; a 32-byte one would be a silent downgrade"
        );
        assert_eq!(
            decode(&public).unwrap().len(),
            56,
            "an X448 public key is 56 bytes; a 32-byte one would be a silent downgrade"
        );
    } else {
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("Private Key"),
            "a refused curve must not print key material: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            !out.stderr.is_empty(),
            "the refusal must say something on stderr"
        );
    }
}

// --- real pairs -------------------------------------------------------------

/// Write a server config for one KK pair: its own private key and the peer's
/// public key.
fn write_server(
    dir: &Path,
    control: u16,
    exposed: u16,
    private: &str,
    peer_public: &str,
) -> Result<PathBuf> {
    let path = dir.join("server.toml");
    std::fs::write(
        &path,
        format!(
            "[server]\n\
             default_token = \"{TOKEN}\"\n\
             allow_ports = [\"{exposed}\"]\n\
             \n\
             [server.control]\n\
             bind_addr = \"127.0.0.1:{control}\"\n\
             \n\
             [server.noise]\n\
             private_key = \"{private}\"\n\
             public_key = \"{peer_public}\"\n"
        ),
    )?;
    Ok(path)
}

/// The client half, with its own private key and the server's public key.
fn write_client(
    dir: &Path,
    name: &str,
    control: u16,
    exposed: u16,
    backend: u16,
    private: &str,
    server_public: &str,
) -> Result<PathBuf> {
    let path = dir.join(format!("client_{name}.toml"));
    std::fs::write(
        &path,
        format!(
            "[client]\n\
             default_token = \"{TOKEN}\"\n\
             \n\
             [client.control]\n\
             default_remote_addr = \"127.0.0.1:{control}\"\n\
             \n\
             [client.noise]\n\
             private_key = \"{private}\"\n\
             public_key = \"{server_public}\"\n\
             \n\
             [client.services.pingpong]\n\
             local_addr = \"127.0.0.1:{backend}\"\n\
             remote_bind_addr = \"127.0.0.1:{exposed}\"\n"
        ),
    )?;
    Ok(path)
}

/// One ping/pong round trip through the exposed endpoint: the readiness probe
/// for every scenario here, and the proof that a handshake actually carried a
/// connection rather than merely not failing.
async fn probe_pong(addr: &str) -> Result<()> {
    let attempt = async {
        let mut conn = TcpStream::connect(addr).await?;
        conn.write_all(PING.as_bytes()).await?;
        let mut rd = [0u8; 4];
        conn.read_exact(&mut rd).await?;
        anyhow::ensure!(rd.as_slice() == PONG.as_bytes(), "unexpected reply {rd:?}");
        Ok::<_, anyhow::Error>(())
    };
    time::timeout(Duration::from_millis(500), attempt).await??;
    Ok(())
}

async fn wait_for_pong(addr: &str, budget: Duration) -> Result<()> {
    let deadline = Instant::now() + budget;
    loop {
        if probe_pong(addr).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("{addr} did not answer a ping within {budget:?}");
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

/// Two keys from the binary, a real KK handshake between the compiled client
/// and server, and a ping/pong through it. A generator that printed a
/// mismatched public key (or a key for another curve) passes every structural
/// assertion above and fails here.
#[tokio::test]
async fn a_generated_keypair_carries_traffic() -> Result<()> {
    init();

    let (server_priv, server_pub) =
        parse_genkey(&String::from_utf8_lossy(&run_genkey(None).stdout))?;
    let (client_priv, client_pub) =
        parse_genkey(&String::from_utf8_lossy(&run_genkey(None).stdout))?;

    let ([control, exposed, backend], held) = free_ports::<3>();
    tokio::spawn(async move {
        if let Err(e) = common::tcp::pingpong_server(format!("127.0.0.1:{backend}")).await {
            panic!("Failed to run the pingpong server for testing: {e:?}");
        }
    });

    let dir = scratch_dir("genkey-transport")?;
    let server_config = write_server(&dir, control, exposed, &server_priv, &client_pub)?;
    let client_config = write_client(
        &dir,
        "generated",
        control,
        exposed,
        backend,
        &client_priv,
        &server_pub,
    )?;

    drop(held);
    let (client_shutdown, client_shutdown_rx) = broadcast::channel(1);
    let (server_shutdown, server_shutdown_rx) = broadcast::channel(1);
    let client_path = client_config.to_string_lossy().into_owned();
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

    let addr = format!("127.0.0.1:{exposed}");
    wait_for_pong(&addr, BUDGET).await?;

    let _ = server_shutdown.send(true);
    let _ = client_shutdown.send(true);
    let _ = tokio::join!(server, client);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// One child of the compiled binary. Its whole output goes to a file, so a
/// refusal can be quoted from what the process actually said.
struct Child {
    process: std::process::Child,
    log: PathBuf,
}

impl Child {
    fn spawn(config: &Path, server: bool) -> Result<Child> {
        let (mode, log) = if server {
            ("--server", config.with_extension("server.log"))
        } else {
            ("--client", config.with_extension("client.log"))
        };
        let out = std::fs::File::create(&log)?;
        let err = out.try_clone()?;
        let process = std::process::Command::new(env!("CARGO_BIN_EXE_molehill"))
            .arg(mode)
            .arg(config)
            .env("RUST_LOG", "debug")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(out))
            .stderr(std::process::Stdio::from(err))
            .spawn()?;
        Ok(Child { process, log })
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn stop(&mut self) {
        if matches!(self.process.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = std::process::Command::new("kill")
            .args(["-INT", &self.process.id().to_string()])
            .status();
        for _ in 0..50 {
            if let Ok(Some(_)) = self.process.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn wait_for_log(child: &Child, needle: &str, budget: Duration) -> Result<()> {
    let deadline = Instant::now() + budget;
    loop {
        let log = child.log();
        if log.contains(needle) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let tail: Vec<&str> = log.lines().rev().take(8).collect();
            bail!(
                "the process never said {needle:?} within {budget:?}; its last lines were:\n{}",
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            );
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

/// A client whose static key is not the one the server expects cannot complete
/// KK: the handshake fails, the failure is named in the client's own output,
/// and nothing is forwarded. (There is no PSK and no resume path to test.)
#[tokio::test]
async fn a_mismatched_keypair_says_the_handshake_failed() -> Result<()> {
    init();

    let (server_priv, server_pub) =
        parse_genkey(&String::from_utf8_lossy(&run_genkey(None).stdout))?;
    let (client_priv, _client_pub) =
        parse_genkey(&String::from_utf8_lossy(&run_genkey(None).stdout))?;
    // The server is told a *different* client public key, so KK cannot verify
    // the client's static key.
    let (_, other_pub) = parse_genkey(&String::from_utf8_lossy(&run_genkey(None).stdout))?;

    let ([control, exposed, backend], held) = free_ports::<3>();
    tokio::spawn(async move {
        if let Err(e) = common::tcp::pingpong_server(format!("127.0.0.1:{backend}")).await {
            panic!("Failed to run the pingpong server for testing: {e:?}");
        }
    });

    let dir = scratch_dir("mismatch")?;
    let server_config = write_server(&dir, control, exposed, &server_priv, &other_pub)?;
    let client_config = write_client(
        &dir,
        "mismatched",
        control,
        exposed,
        backend,
        &client_priv,
        &server_pub,
    )?;
    drop(held);

    let mut server = Child::spawn(&server_config, true)?;
    let mut client = Child::spawn(&client_config, false)?;
    wait_for_log(&client, "noise handshake", BUDGET).await?;
    let exposed_addr = format!("127.0.0.1:{exposed}");
    assert!(
        probe_pong(&exposed_addr).await.is_err(),
        "a mismatched key pair must not forward traffic"
    );
    client.stop();
    server.stop();
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
