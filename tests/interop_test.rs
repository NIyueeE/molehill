//! Interoperability matrix: this build against the binary of the previous
//! release.
//!
//! Every other test in this suite compiles *both* ends from today's tree, so
//! none of them can see a wire-format break: a change that makes this build
//! unable to talk to yesterday's peer passes all of them. This target closes
//! that hole — it runs a real server and a real client as subprocesses, one of
//! them the released asset (a binary built from today's tree cannot test
//! yesterday's protocol, which is the whole point).
//!
//! It is **skipped, loudly, unless `MOLEHILL_OLD_BIN` points at that binary**:
//! the fetch needs network access, so this is a local / pre-release step rather
//! than a CI gate. `just interop` does both halves:
//!
//! ```text
//! uv run benches/scripts/interop/fetch_old.py   # prints MOLEHILL_OLD_BIN=...
//! MOLEHILL_OLD_BIN=... cargo test --test interop_test -- --include-ignored
//! ```
//!
//! "Loudly" is why the cases below are `#[ignore]`d: libtest captures a passing
//! test's output, so a printed skip notice would be invisible in a plain
//! `cargo test` — but the summary line is not captured, and it now reads
//! `3 ignored`. Anyone reading a green run sees that the matrix did not run.
//!
//! Three cases, in the order a break would be caught:
//!
//! 1. an old server **refuses** the new client's dialect and says nothing else
//!    — and the refusal is local to that connection: the same old server
//!    process still forwards for a client of its own version, and the new
//!    client reports *why* instead of retrying forever;
//! 2. the new server **refuses** the old client's dialect (v0.10.0 dropped the
//!    v3 server path, so there is nothing on the other side to serve it) —
//!    and the refusal is local: the same server process still forwards for a
//!    client of its own version;
//! 3. an old server rejects an *unknown dialect* cleanly and keeps serving
//!    (it closes that one connection instead of hanging, crashing, or taking
//!    the listener down with it).
//!
//! Only case 2 is a forwarding case now: v0.10.0 changed the dialect the client
//! speaks *and* dropped the dialect the server used to serve, so a matrix that
//! claimed old-to-new forwarding in either direction would hide exactly the
//! break it exists to catch. Case 2 therefore proves the direction that
//! changed — the new server refuses the dialect it no longer serves, says so on
//! the connection it happens on, and keeps its listener — while cases 1 and 3
//! prove that the old server's own refusals are local. Both halves of the
//! break being announced is what makes the upgrade path visible: an operator
//! running mixed versions sees *why* the connection died rather than a retry
//! loop.
//!
//! The matrix's peer is the previous *release*, and that binary speaks the v3
//! dialect this server no longer serves, so there is no binary here that speaks
//! v4 minus the stripe-group request: what a v4 peer receives is pinned by
//! `tests/session_test.rs::a_striped_gather_names_its_group_on_every_request`
//! instead.
//!
//! The configs below deliberately use only keys that exist in both versions —
//! a key this cycle renames would make the old binary fail to start, and the
//! test would report a config error as an interop failure.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "an integration test unwraps and asserts on values it just produced"
)]

use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// The freshly built binary of this tree. Cargo builds it for this target and
/// hands over its path, so the test can never run a stale `target/release`
/// leftover.
const NEW_BIN: &str = env!("CARGO_BIN_EXE_molehill");

/// Shared by both ends; the config is generated per case.
const TOKEN: &str = "interop_token";

/// Generous: this covers process start, control-channel auth, registration and
/// the first tunnel. The passing runs take well under a second.
const STARTUP: Duration = Duration::from_secs(30);

/// Payload of the forwarding check — long enough that a truncated read is
/// visible, short enough to stay inside one frame.
const PING: &[u8] = b"interop-ping\n";

/// The plain transport selector (`0x00`); see `protocol::PLAIN_SELECTOR`.
const PLAIN_SELECTOR: u8 = 0x00;

/// Length of the encoded `Hello::ControlChannelHello`: one variant tag, one
/// `u8` protocol version, one 32-byte digest. The server reads exactly this
/// many bytes, so a short write would make it wait rather than reject.
const CONTROL_HELLO_LEN: usize = 1 + 1 + 32;

/// An old peer must reject a dialect it does not know. Nothing has a
/// `u8` version 99.
const UNKNOWN_VERSION: u8 = 99;

/// The dialect a 0.9.0 peer speaks and this build does not serve, written raw
/// because this build can no longer produce such a hello.
const PROTO_V3_BYTE: u8 = 3;

/// The old release binary, or `None` when the environment did not provide one.
fn old_bin() -> Option<PathBuf> {
    let raw = std::env::var("MOLEHILL_OLD_BIN").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    if path.is_file() {
        return Some(path);
    }
    eprintln!("MOLEHILL_OLD_BIN={} is not a file", path.display());
    None
}

/// The old peer, or a printed explanation of why nothing ran.
///
/// The `#[ignore]`s make a plain `cargo test` report `3 ignored`; this guard
/// covers the other door — `--include-ignored` without the variable — where a
/// silent `return` would look exactly like a pass.
fn skip(test: &str) -> Option<PathBuf> {
    if let Some(p) = old_bin() {
        Some(p)
    } else {
        eprintln!(
            "\n\
             ============================================================\n\
             SKIPPED: {test}\n\
             MOLEHILL_OLD_BIN is unset, so there is no old peer to test\n\
             against. Run the matrix with:\n\
             \n    just interop\n\n\
             (it fetches the previous release and sets the variable).\n\
             ============================================================\n"
        );
        None
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A TCP echo backend: the far end of the forwarded path, in-process.
fn spawn_echo_backend(port: u16) {
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                loop {
                    match conn.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if conn.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
}

fn server_config(control: u16, exposed: u16) -> String {
    format!(
        "[server]\n\
         default_token = \"{TOKEN}\"\n\
         allow_ports = [\"{exposed}\"]\n\
         \n\
         [server.control]\n\
         bind_addr = \"127.0.0.1:{control}\"\n"
    )
}

fn client_config(control: u16, exposed: u16, backend: u16) -> String {
    format!(
        "[client]\n\
         default_token = \"{TOKEN}\"\n\
         \n\
         [client.control]\n\
         default_remote_addr = \"127.0.0.1:{control}\"\n\
         \n\
         [client.transport]\n\
         type = \"plain\"\n\
         \n\
         [client.services.echo]\n\
         local_addr = \"127.0.0.1:{backend}\"\n\
         remote_bind_addr = \"127.0.0.1:{exposed}\"\n"
    )
}

/// One launched process, with its output on disk: a pipe nobody drains (or
/// drains only after a failure) can block the child once the buffer fills.
struct Proc {
    name: &'static str,
    child: Child,
    log: PathBuf,
}

impl Proc {
    fn spawn(name: &'static str, bin: &Path, mode: &str, config: &Path, log: PathBuf) -> Proc {
        let out = fs::File::create(&log).unwrap();
        let err = out.try_clone().unwrap();
        let child = Command::new(bin)
            .arg(mode)
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .unwrap_or_else(|e| panic!("failed to start {} ({}): {e}", bin.display(), name));
        Proc { name, child, log }
    }

    /// The tail of the child's output, for a failure message.
    fn tail(&self) -> String {
        let text = fs::read_to_string(&self.log).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(20);
        lines[start..].join("\n")
    }

    /// The whole log. A case that asserts on a line (rather than on a round
    /// trip) cannot use `tail`: the line it wants may be older than the last
    /// twenty by the time the wait ends.
    fn log_text(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Non-empty only when the process died on its own: a clean shutdown at the
    /// end of a case is not an error.
    fn died(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(format!(
                "{} exited early with {status}\n--- {name} log (tail) ---\n{tail}",
                self.name,
                name = self.name,
                tail = self.tail()
            )),
            _ => None,
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A running matrix case: the temp directory and the processes, all torn down
/// when the case ends.
struct Case {
    dir: PathBuf,
    procs: Vec<Proc>,
    exposed: u16,
}

impl Case {
    fn new(label: &str) -> Case {
        let dir =
            std::env::temp_dir().join(format!("molehill-interop-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Case {
            dir,
            procs: Vec::new(),
            exposed: 0,
        }
    }

    fn log_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.log"))
    }

    fn write(&self, name: &str, body: &str) -> PathBuf {
        let path = self.dir.join(name);
        fs::write(&path, body).unwrap();
        path
    }

    fn spawn(&mut self, name: &'static str, bin: &Path, mode: &str, config: &Path) {
        let log = self.log_path(name);
        self.procs.push(Proc::spawn(name, bin, mode, config, log));
    }

    /// Neither end may die while the case runs.
    fn assert_alive(&mut self) {
        let mut dead = None;
        for proc in &mut self.procs {
            if let Some(msg) = proc.died() {
                dead = Some(msg);
                break;
            }
        }
        if let Some(msg) = dead {
            self.fail(&msg);
        }
    }

    /// The temp directory is left behind on purpose: the logs inside it are the
    /// only evidence of *why* a case failed.
    fn fail(&self, msg: &str) -> ! {
        panic!(
            "{msg}\n(temp directory kept for inspection: {})",
            self.dir.display()
        );
    }

    fn cleanup(self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// One forwarding round trip through the exposed port.
fn roundtrip(exposed: u16) -> std::io::Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], exposed));
    let mut conn = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    conn.set_read_timeout(Some(Duration::from_secs(2)))?;
    conn.set_write_timeout(Some(Duration::from_secs(2)))?;
    conn.write_all(PING)?;
    let mut buf = vec![0u8; PING.len()];
    conn.read_exact(&mut buf)?;
    if buf != PING {
        return Err(std::io::Error::other(format!(
            "echo came back changed: {buf:?}"
        )));
    }
    Ok(())
}

/// Wait until a visitor's bytes reach the backend and come back. The exposed
/// port only exists after the registration landed, so a successful round trip
/// is the strongest possible "both ends are up and forwarding".
fn wait_for_forwarding(case: &mut Case) -> std::io::Result<()> {
    let deadline = Instant::now() + STARTUP;
    let mut last = String::from("never attempted");
    while Instant::now() < deadline {
        case.assert_alive();
        match roundtrip(case.exposed) {
            Ok(()) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(std::io::Error::other(format!(
        "no forwarding through 127.0.0.1:{} within {STARTUP:?} (last: {last})",
        case.exposed
    )))
}

/// Wait until the process at `case.procs[index]` has written `needle`.
///
/// A log line is the observable when the thing being asserted *is* a message:
/// case 1 exists because a client meeting a server that cannot serve its
/// dialect must say what happened ("this server is older than me") rather than
/// retry into the void, and the client's own output is the only place that
/// statement exists.
fn wait_for_log(case: &mut Case, index: usize, needle: &str) -> Result<(), String> {
    let deadline = Instant::now() + STARTUP;
    while Instant::now() < deadline {
        if case.procs[index].log_text().contains(needle) {
            return Ok(());
        }
        case.assert_alive();
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "no line containing {needle:?} within {STARTUP:?}\n--- {} log ---\n{}",
        case.procs[index].name,
        case.procs[index].log_text()
    ))
}

/// 1. An old server cannot serve the new client's dialect — and says so by
///    closing, while the new client reports the mismatch instead of retrying.
///
/// This case inverted when v0.10.0 moved the client to protocol v4: the old
/// server reads version 4, fails its version check and closes without a reply.
/// A silent retry loop against it would be the dangerous shape (the client
/// looks healthy while its services are invisible), so the assertion is on the
/// *message*: the client must name the version mismatch. The refusal also has
/// to be local: the same server process then serves a client of its own
/// version, which is the strongest statement that the dialect broke and not the
/// listener.
#[test]
#[ignore = "interop: needs MOLEHILL_OLD_BIN (run: just interop)"]
fn old_server_refuses_new_client_and_says_so() {
    let Some(old) = skip("old_server_refuses_new_client_and_says_so") else {
        return;
    };
    println!(
        "matrix: old server ({}) + new client ({NEW_BIN})",
        old.display()
    );
    let mut case = Case::new("old-server-new-client");
    let control = free_port();
    case.exposed = free_port();
    let backend = free_port();
    spawn_echo_backend(backend);

    let server_cfg = case.write("server.toml", &server_config(control, case.exposed));
    let client_cfg = case.write(
        "client.toml",
        &client_config(control, case.exposed, backend),
    );
    case.spawn("server", &old, "--server", &server_cfg);
    case.spawn("client", Path::new(NEW_BIN), "--client", &client_cfg);

    // The client is process 1. It must not need the exposed port to exist: the
    // registration never happens, so this waits on the message alone.
    if let Err(e) = wait_for_log(&mut case, 1, "protocol v4") {
        let msg = format!(
            "the new client never reported the protocol mismatch: {e}\n\
             --- server log (tail) ---\n{}",
            case.procs[0].tail()
        );
        case.fail(&msg);
    }
    case.assert_alive();

    // Local to that connection: the same old server still forwards for a client
    // of the version it knows. The old binary is both ends here, which is why
    // this half needs no new-client cooperation at all.
    case.spawn("old-client", &old, "--client", &client_cfg);
    if let Err(e) = wait_for_forwarding(&mut case) {
        let msg = format!(
            "one refused dialect took the old server's service down: {e}\n\
             --- server log (tail) ---\n{}\n--- old client log (tail) ---\n{}",
            case.procs[0].tail(),
            case.procs[2].tail()
        );
        case.fail(&msg);
    }
    case.cleanup();
}

/// 2. The new server refuses what yesterday's client says — v0.10.0 dropped
///    the v3 server path, so the refusal is the contract, and it has to be
///    local: the same process then forwards for a client of its own version.
///
/// This case inverted from "new server + old client forwards traffic": keeping
/// a v3 serving path alive only for an old peer is exactly the kind of
/// compatibility that has to be *announced* rather than assumed, so the case
/// now pins the refusal and the listener's survival. What is asserted is the
/// refusal's shape — no answer to the dialect it does not serve, and a client of
/// its own version still served by the same process — which is what a reader of
/// this matrix needs to know.
#[test]
#[ignore = "interop: needs MOLEHILL_OLD_BIN (run: just interop)"]
fn new_server_refuses_old_client_and_keeps_serving() {
    let Some(old) = skip("new_server_refuses_old_client_and_keeps_serving") else {
        return;
    };
    println!(
        "matrix: new server ({NEW_BIN}) + old client ({})",
        old.display()
    );
    let mut case = Case::new("new-server-old-client");
    let control = free_port();
    case.exposed = free_port();
    let backend = free_port();
    spawn_echo_backend(backend);

    let server_cfg = case.write("server.toml", &server_config(control, case.exposed));
    let client_cfg = case.write(
        "client.toml",
        &client_config(control, case.exposed, backend),
    );
    // The new server first.
    case.spawn("server", Path::new(NEW_BIN), "--server", &server_cfg);

    // Wait for the control listener before speaking to it, then drive it with
    // a well-formed v3 control hello — the dialect a 0.9.0 client speaks and
    // this server no longer serves.
    let control_addr = SocketAddr::from(([127, 0, 0, 1], control));
    let deadline = Instant::now() + STARTUP;
    let mut conn = loop {
        if let Ok(c) = TcpStream::connect_timeout(&control_addr, Duration::from_secs(2)) {
            break c;
        }
        case.assert_alive();
        if Instant::now() >= deadline {
            let msg = format!(
                "the new server never listened on {control_addr}\n--- server log (tail) ---\n{}",
                case.procs[0].tail()
            );
            case.fail(&msg);
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let mut hello = Vec::with_capacity(1 + CONTROL_HELLO_LEN);
    hello.push(PLAIN_SELECTOR);
    hello.push(0); // variant tag: ControlChannelHello
    hello.push(PROTO_V3_BYTE); // the dialect this server does not serve
    hello.extend([0x42u8; 32]); // the digest that dialect would authenticate
    assert_eq!(hello.len(), 1 + CONTROL_HELLO_LEN, "the hello layout moved");
    conn.write_all(&hello).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    // The refusal is a closed connection with no answer: the reader cannot
    // parse a hello whose version it does not serve, so nothing comes back.
    let mut buf = [0u8; 64];
    match conn.read(&mut buf) {
        Ok(0) => {}
        Ok(n) => {
            let msg = format!(
                "the new server answered a v3 hello with {n} bytes: {:?}",
                &buf[..n]
            );
            case.fail(&msg);
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
            ) => {}
        Err(e) => {
            let msg =
                format!("the new server left a v3 hello hanging ({e:?}) instead of refusing it");
            case.fail(&msg);
        }
    }

    // The refusal was local: a client of the server's own version forwards
    // through the same process.
    case.assert_alive();
    case.spawn("new-client", Path::new(NEW_BIN), "--client", &client_cfg);
    if let Err(e) = wait_for_forwarding(&mut case) {
        let msg = format!(
            "one refused dialect took the new server's service down: {e}\n\
             --- server log (tail) ---\n{}",
            case.procs[0].tail()
        );
        case.fail(&msg);
    }
    case.cleanup();
}

/// 3. An unknown dialect is refused on that connection alone.
///
/// A protocol break has two halves, and only one of them is about forwarding:
/// the peer that does not understand the new grammar must *say so* — close the
/// connection with an error — instead of hanging on a partial read, dying, or
/// dropping its listener. This case drives the old server with a well-formed
/// control hello carrying a version that did not exist when it was built, then
/// proves the same process still serves a valid client. When this cycle
/// introduces a new hello variant, its bytes are the more interesting input
/// here; the version field is the part that stays unknown to every older build.
#[test]
#[ignore = "interop: needs MOLEHILL_OLD_BIN (run: just interop)"]
fn old_server_rejects_unknown_dialect_and_survives() {
    let Some(old) = skip("old_server_rejects_unknown_dialect_and_survives") else {
        return;
    };
    let mut case = Case::new("old-server-unknown-dialect");
    let control = free_port();
    case.exposed = free_port();
    let backend = free_port();
    spawn_echo_backend(backend);

    let server_cfg = case.write("server.toml", &server_config(control, case.exposed));
    let client_cfg = case.write(
        "client.toml",
        &client_config(control, case.exposed, backend),
    );
    case.spawn("server", &old, "--server", &server_cfg);

    // Wait for the control listener before speaking to it.
    let control_addr = SocketAddr::from(([127, 0, 0, 1], control));
    let deadline = Instant::now() + STARTUP;
    let mut conn = loop {
        if let Ok(c) = TcpStream::connect_timeout(&control_addr, Duration::from_secs(2)) {
            break c;
        }
        case.assert_alive();
        if Instant::now() >= deadline {
            let msg = format!(
                "the old server never listened on {control_addr}\n--- server log (tail) ---\n{}",
                case.procs[0].tail()
            );
            case.fail(&msg);
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let mut hello = Vec::with_capacity(1 + CONTROL_HELLO_LEN);
    hello.push(PLAIN_SELECTOR);
    hello.push(0); // variant tag: ControlChannelHello
    hello.push(UNKNOWN_VERSION);
    hello.extend([0u8; 32]); // the digest the version would have authenticated
    assert_eq!(hello.len(), 1 + CONTROL_HELLO_LEN, "the hello layout moved");
    conn.write_all(&hello).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    // A refusal is a closed connection: EOF or a reset. A peer that waits for
    // more bytes, or answers with a command, has not refused anything.
    let mut buf = [0u8; 64];
    match conn.read(&mut buf) {
        Ok(0) => {}
        Ok(n) => {
            let msg = format!(
                "the old server answered an unknown dialect with {n} bytes: {:?}",
                &buf[..n]
            );
            case.fail(&msg);
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
            ) => {}
        Err(e) => {
            let msg = format!(
                "the old server left an unknown dialect hanging ({e:?}) instead of refusing it"
            );
            case.fail(&msg);
        }
    }

    // The refusal must be local to that connection: the same process still
    // has to serve a client that speaks the dialect it knows. That client is
    // the old binary itself — the new client speaks v4, which this server has
    // just been shown to refuse (case 1), so it cannot be the witness here.
    case.assert_alive();
    case.spawn("old-client", &old, "--client", &client_cfg);
    if let Err(e) = wait_for_forwarding(&mut case) {
        let msg = format!(
            "one unknown dialect took the old server's service down: {e}\n\
             --- server log (tail) ---\n{}",
            case.procs[0].tail()
        );
        case.fail(&msg);
    }
    case.cleanup();
}
