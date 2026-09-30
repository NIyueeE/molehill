//! The Noise key material contract, driven through the **real binary**: what
//! `--genkey` prints and accepts, and what a PSK does to a real handshake.
//!
//! - `--genkey` defaults to X25519 and prints a keypair in the documented
//!   shape; the pair has to *work*, so the test feeds it back into a real
//!   server/client pair over the pattern `--genkey` itself builds
//!   (`Noise_KK_<curve>_ChaChaPoly_BLAKE2s`, see `docs/transport.md`).
//! - A curve the build cannot do must be **refused**, not silently downgraded
//!   to a 25519 pair: the assertion is on the size of what was printed, or on
//!   the absence of any key at all.
//! - A PSK that matches on both ends must carry traffic; one that does not (or
//!   one only one side has) must fail the handshake **and say so** in the
//!   process's own output. `docs/configuration.md` names "Noise handshake
//!   fails" as the user-visible symptom, and a silent hang would be a
//!   different — much worse — product.
//!
//! The handshake cases drive child processes rather than the in-process
//! helpers because the diagnosis *is* the assertion: the client retries a
//! failed handshake for ever, so its log is the only place the reason appears,
//! and a child's captured stderr is exactly what an operator would see.
#![cfg(all(feature = "noise", feature = "client", feature = "server"))]
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

/// The pattern `--genkey` builds its keypairs for: `KK`, the curve it names,
/// `ChaChaPoly`, `BLAKE2s`. Spelled here so a change to the binary's own
/// pattern has to be made twice.
const KK_25519: &str = "Noise_KK_25519_ChaChaPoly_BLAKE2s";
/// The PSK-carrying spelling of the same handshake (`docs/transport.md`).
const KK_PSK0_25519: &str = "Noise_KKpsk0_25519_ChaChaPoly_BLAKE2s";

/// Two X25519 keypairs, generated once with this binary's own `--genkey` and
/// pinned here as fixtures (exactly like `tests/for_tcp/noise_transport.toml`
/// does): a handshake test must not depend on the generator it ships beside.
/// security-scan:allow dummy key material for a test fixture, never a real key
const SERVER_PRIV: &str = "f2h02meRzyrwtFiIpG+S4uN4EUtxjUWHmC+PS1tE4Sk=";
const SERVER_PUB: &str = "MzrK8bCg9THzJTCRLkMsc0FiS1418u4IYo10cqKfEDA=";
const CLIENT_PRIV: &str = "UFtmCGmNPwxuTn1hL4EQGp5oVCxrsvB6x6Xsz/piiow=";
const CLIENT_PUB: &str = "Db/j5zBHbV8zmZ9lAllkYQuBtFh5ScR+vodz17QnihA=";

/// The shared PSK both ends agree on (32 zero bytes: a fixture, not a secret).
/// security-scan:allow dummy psk for a test fixture, never a real credential
const SHARED_PSK: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
/// A PSK the server does not have: the handshake must fail on it.
/// security-scan:allow dummy psk for a test fixture, never a real credential
const WRONG_PSK: &str = "DRE4m6IroPoUqifsfANWNI+aQyoDpOYWPzKibcp1Lgc=";

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

/// A scratch directory of its own per scenario, so the two binary-driven
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
/// bytes), which is what makes them usable as `local_private_key` /
/// `remote_public_key` in the first place.
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
///
/// The shipped build (snow with ring's resolver) has no X448, so the refusal
/// branch is the one that runs today; the accepting branch is written out too,
/// because "this build grew the curve" must stay a *correct* outcome rather
/// than a test failure, and it is the branch that says what 448-bit output
/// would have to look like.
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
            !out.stdout.is_empty() || !out.stderr.is_empty(),
            "a refused curve must produce a diagnostic, not silence"
        );
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

/// The generated pair is a *working* pair for the transport `--genkey` names:
/// two keys from the binary, a real `Noise_KK_25519_ChaChaPoly_BLAKE2s`
/// handshake between an in-process client and server, and a ping/pong through
/// it. A generator that printed a mismatched public key (or a key for another
/// curve) passes every structural assertion above and fails here.
#[tokio::test]
async fn a_generated_keypair_carries_traffic_over_the_pattern_genkey_names() -> Result<()> {
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
    let server_config = dir.join("server.toml");
    std::fs::write(
        &server_config,
        format!(
            "[server]\n\
             default_token = \"{TOKEN}\"\n\
             allow_ports = [\"{exposed}\"]\n\
             \n\
             [server.control]\n\
             bind_addr = \"127.0.0.1:{control}\"\n\
             \n\
             [server.transport.noise]\n\
             pattern = \"{KK_25519}\"\n\
             local_private_key = \"{server_priv}\"\n\
             remote_public_key = \"{client_pub}\"\n"
        ),
    )?;
    let client_config = dir.join("client.toml");
    std::fs::write(
        &client_config,
        format!(
            "[client]\n\
             default_token = \"{TOKEN}\"\n\
             \n\
             [client.control]\n\
             default_remote_addr = \"127.0.0.1:{control}\"\n\
             \n\
             [client.transport]\n\
             type = \"noise\"\n\
             \n\
             [client.transport.noise]\n\
             pattern = \"{KK_25519}\"\n\
             local_private_key = \"{client_priv}\"\n\
             remote_public_key = \"{server_pub}\"\n\
             \n\
             [client.services.pingpong]\n\
             local_addr = \"127.0.0.1:{backend}\"\n\
             remote_bind_addr = \"127.0.0.1:{exposed}\"\n"
        ),
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

/// Poll the endpoint until it answers, or fail after `budget`.
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

// --- PSK handshakes, through the real binary -------------------------------

/// One child of the compiled binary. Its whole output goes to a file, so a
/// refusal can be quoted from what the process actually said — which is the
/// assertion for the two failure cases.
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

    /// SIGINT first (the binary turns it into a clean exit), then SIGKILL if it
    /// is still alive: a leaked child would hold the scenario's ports.
    fn stop(&mut self) {
        // Already reaped — an explicit `stop()` followed by `drop` — so a
        // second `kill` would only print "No such process" over the output.
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

/// Wait until the child's output contains `needle`, quoting the tail of what
/// it did print when the budget runs out.
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

/// Write the server side: the `KKpsk0` pattern, the fixture keypair, and the
/// shared PSK when the case has one.
fn write_noise_server(
    dir: &Path,
    control: u16,
    exposed: u16,
    psk: Option<&str>,
) -> Result<PathBuf> {
    let psk_line = psk.map_or_else(String::new, |psk| {
        format!("psk = \"{psk}\"\npsk_location = 0\n")
    });
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
             [server.transport.noise]\n\
             pattern = \"{KK_PSK0_25519}\"\n\
             local_private_key = \"{SERVER_PRIV}\"\n\
             remote_public_key = \"{CLIENT_PUB}\"\n\
             {psk_line}"
        ),
    )?;
    Ok(path)
}

/// The same for the client: `name` distinguishes the three PSK variants, and
/// `psk = None` leaves the key out entirely (the "only one end has it" case).
fn write_noise_client(
    dir: &Path,
    name: &str,
    control: u16,
    exposed: u16,
    backend: u16,
    psk: Option<&str>,
) -> Result<PathBuf> {
    let psk_line = psk.map_or_else(String::new, |psk| {
        format!("psk = \"{psk}\"\npsk_location = 0\n")
    });
    let path = dir.join(format!("client_{name}.toml"));
    // The service block follows the PSK lines either way: `psk_line` ends with
    // a newline when present, and an empty string is nothing when absent.
    std::fs::write(
        &path,
        format!(
            "[client]\n\
             default_token = \"{TOKEN}\"\n\
             \n\
             [client.control]\n\
             default_remote_addr = \"127.0.0.1:{control}\"\n\
             \n\
             [client.transport]\n\
             type = \"noise\"\n\
             \n\
             [client.transport.noise]\n\
             pattern = \"{KK_PSK0_25519}\"\n\
             local_private_key = \"{CLIENT_PRIV}\"\n\
             remote_public_key = \"{SERVER_PUB}\"\n\
             {psk_line}\n\
             [client.services.pingpong]\n\
             local_addr = \"127.0.0.1:{backend}\"\n\
             remote_bind_addr = \"127.0.0.1:{exposed}\"\n"
        ),
    )?;
    Ok(path)
}

/// With the same PSK on both ends the handshake completes and traffic flows;
/// with a different one, or with the key present on only one end, the
/// connection fails **and both are reported by name** — a mismatch as a failed
/// Noise handshake, a missing key as `MissingPsk`.
///
/// The three cases share one server, exactly as an operator would meet them:
/// the same policy, three clients. Traffic is asserted only in the matching
/// case; in the other two the *absence* of a working path is the point, and
/// the process's own words are what makes it diagnosable.
#[tokio::test]
async fn a_matching_psk_carries_traffic_and_a_mismatch_says_why() -> Result<()> {
    init();

    let ([control, exposed, backend], held) = free_ports::<3>();
    tokio::spawn(async move {
        if let Err(e) = common::tcp::pingpong_server(format!("127.0.0.1:{backend}")).await {
            panic!("Failed to run the pingpong server for testing: {e:?}");
        }
    });

    let dir = scratch_dir("psk")?;
    let server_config = write_noise_server(&dir, control, exposed, Some(SHARED_PSK))?;
    let matching = write_noise_client(
        &dir,
        "matching",
        control,
        exposed,
        backend,
        Some(SHARED_PSK),
    )?;
    let mismatched = write_noise_client(
        &dir,
        "mismatched",
        control,
        exposed,
        backend,
        Some(WRONG_PSK),
    )?;
    let missing = write_noise_client(&dir, "missing", control, exposed, backend, None)?;
    drop(held);

    let exposed_addr = format!("127.0.0.1:{exposed}");
    let mut server = Child::spawn(&server_config, true)?;

    // The matching key: a real session, a real registration, real traffic.
    let mut client = Child::spawn(&matching, false)?;
    wait_for_pong(&exposed_addr, BUDGET).await?;
    client.stop();

    // A different key: the handshake must fail, the failure must be named, and
    // nothing may be forwarded.
    let mut client = Child::spawn(&mismatched, false)?;
    wait_for_log(&client, "noise handshake", BUDGET).await?;
    assert!(
        probe_pong(&exposed_addr).await.is_err(),
        "a client with the wrong psk must not be able to forward traffic"
    );
    client.stop();

    // No key at all on the client while the server requires one: refused for
    // the named reason, not by timing out.
    let mut client = Child::spawn(&missing, false)?;
    wait_for_log(&client, "MissingPsk", BUDGET).await?;
    assert!(
        probe_pong(&exposed_addr).await.is_err(),
        "a client without the psk must not be able to forward traffic"
    );
    client.stop();

    server.stop();
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
