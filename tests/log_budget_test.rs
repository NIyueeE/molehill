//! What an operator sees on a healthy run — measured, not asserted.
//!
//! The log level contract (docs/configuration.md, "Logging") says ERROR is
//! "a human must act", WARN is "something happened that the software handled,
//! and it is worth one line", INFO is "lifecycle", DEBUG is "per connection".
//! A contract nobody measures drifts: every individual `warn!` looks
//! reasonable at the moment it is written, and the file ends up with sixteen
//! "Failed to run the data channel: early eof" lines for a run in which
//! nothing went wrong.
//!
//! These tests therefore drive the **real binary** as a subprocess, with the
//! production formatter ([`molehill_rathole::logging::init`], not the test
//! subscriber the integration suite installs), and read what landed in the
//! log. Subprocesses also make the measurement honest in a second way: a
//! process boundary means one scenario's events cannot leak into another's
//! count.
//!
//! **Unix and native-target only.** The measurement ends with a *graceful*
//! shutdown, which is a signal (`SIGINT`, the one the binary turns into a clean
//! exit); Windows has no portable way to deliver `CTRL_C_EVENT` to a child, and
//! killing it instead would truncate the log mid-line and drop the teardown
//! path where most of the old noise lived. And it needs a target that can
//! execute what it just built: under `cross` a spawned child dies with `Exec
//! format error` (a v0.9.1 release build failed exactly there), so those
//! targets report `0 tests` — the build script emits `native_target` for a
//! target of the host's architecture.
#![cfg(all(unix, native_target))]

//! The budget:
//!
//! * **zero WARN and zero ERROR on a run where nothing failed** — the strict
//!   half, and the one that catches per-connection noise;
//! * **INFO below a documented ceiling** — lifecycle lines are legitimate, but
//!   a loop that logs one per iteration is not;
//! * **no message *shape* repeated more than [`MAX_SHAPE_REPEATS`] times** —
//!   the aggregation rule in measurable form: if something can happen a
//!   thousand times, it must not produce a thousand lines.
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

/// The real binary: the production formatter, the production default level.
const NEW_BIN: &str = env!("CARGO_BIN_EXE_molehill");

const TOKEN: &str = "log_budget_token";
const PING: &[u8] = b"log-budget-ping\n";
const STARTUP: Duration = Duration::from_secs(30);

/// Lifecycle ceiling for one service through its whole life: start, register,
/// listen (both ends), the visitor connections, shutdown. Measured at 11 for
/// the scenario below; the headroom is for a lifecycle line that legitimately
/// grows a step, not for per-connection logging.
const MAX_INFO_LINES: usize = 20;

/// One line may repeat this often. Above it, the event needs either a lower
/// level or aggregation — see the module docs.
const MAX_SHAPE_REPEATS: usize = 3;

/// Wait for a process to exit, returning `false` on timeout (the child is
/// killed by its `Drop` in that case).
fn wait_for_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// SIGINT, which is the signal the binary turns into a graceful shutdown
/// (`main` waits on ctrl-c). SIGKILL would skip the teardown path — and the
/// teardown path is where log noise hides.
fn interrupt(child: &Child) -> bool {
    Command::new("kill")
        .arg("-INT")
        .arg(child.id().to_string())
        .status()
        .is_ok_and(|s| s.success())
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

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

fn roundtrip(exposed: u16) -> std::io::Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], exposed));
    let mut conn = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    conn.set_read_timeout(Some(Duration::from_secs(2)))?;
    conn.set_write_timeout(Some(Duration::from_secs(2)))?;
    conn.write_all(PING)?;
    let mut buf = vec![0u8; PING.len()];
    conn.read_exact(&mut buf)?;
    assert_eq!(buf, PING, "the echo came back changed");
    Ok(())
}

/// A running server+client pair, with their logs on disk.
struct Scenario {
    dir: PathBuf,
    server: Child,
    client: Child,
    exposed: u16,
    visitors: usize,
}

impl Scenario {
    /// Start a server, wait for it, start a client, wait for forwarding.
    ///
    /// `client_extra` is appended to the `[client.services.echo]` table, which
    /// is how a test asks for a config that is not clean.
    fn start(label: &str, client_extra: &str) -> Scenario {
        let dir = std::env::temp_dir().join(format!(
            "molehill-log-budget-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let control = free_port();
        let exposed = free_port();
        let backend = free_port();
        spawn_echo_backend(backend);

        let server_cfg = dir.join("server.toml");
        let client_cfg = dir.join("client.toml");
        fs::write(
            &server_cfg,
            format!(
                "[server]\n\
                 default_token = \"{TOKEN}\"\n\
                 allow_ports = [\"{exposed}\"]\n\
                 \n\
                 [server.control]\n\
                 bind_addr = \"127.0.0.1:{control}\"\n"
            ),
        )
        .unwrap();
        fs::write(
            &client_cfg,
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
                 remote_bind_addr = \"127.0.0.1:{exposed}\"\n\
                 {client_extra}"
            ),
        )
        .unwrap();

        let mut scenario = Scenario {
            dir: dir.clone(),
            server: spawn(&dir, "server", "--server", &server_cfg),
            client: spawn(&dir, "client", "--client", &client_cfg),
            exposed,
            visitors: 0,
        };

        // The server's control listener first: the client is started second so
        // the run has no "connection refused, retrying" in it, and the budget
        // measures a healthy run rather than a race.
        wait_for_listener(control);
        scenario.wait_for_forwarding();
        scenario
    }

    fn wait_for_forwarding(&mut self) {
        let deadline = Instant::now() + STARTUP;
        let mut last = String::from("never attempted");
        while Instant::now() < deadline {
            assert!(
                self.server.try_wait().unwrap().is_none(),
                "the server exited early:\n{}",
                self.server_log()
            );
            assert!(
                self.client.try_wait().unwrap().is_none(),
                "the client exited early:\n{}",
                self.client_log()
            );
            match roundtrip(self.exposed) {
                Ok(()) => {
                    self.visitors += 1;
                    return;
                }
                Err(e) => last = e.to_string(),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "no forwarding within {STARTUP:?} (last: {last})\n--- server ---\n{}\n--- client ---\n{}",
            self.server_log(),
            self.client_log()
        );
    }

    fn server_log(&self) -> String {
        fs::read_to_string(self.dir.join("server.log")).unwrap_or_default()
    }

    fn client_log(&self) -> String {
        fs::read_to_string(self.dir.join("client.log")).unwrap_or_default()
    }

    /// Both ends shut down gracefully, and the test keeps the logs.
    fn stop(&mut self) -> (String, String) {
        assert!(interrupt(&self.client), "could not signal the client");
        assert!(interrupt(&self.server), "could not signal the server");
        assert!(
            wait_for_exit(&mut self.client, Duration::from_secs(10)),
            "the client did not exit on SIGINT:\n{}",
            self.client_log()
        );
        assert!(
            wait_for_exit(&mut self.server, Duration::from_secs(10)),
            "the server did not exit on SIGINT:\n{}",
            self.server_log()
        );
        let logs = (self.server_log(), self.client_log());
        let _ = fs::remove_dir_all(&self.dir);
        logs
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        let _ = self.client.kill();
        let _ = self.server.kill();
        let _ = self.client.wait();
        let _ = self.server.wait();
    }
}

fn spawn(dir: &Path, name: &str, mode: &str, config: &Path) -> Child {
    let out = fs::File::create(dir.join(format!("{name}.log"))).unwrap();
    let err = out.try_clone().unwrap();
    Command::new(NEW_BIN)
        .arg(mode)
        .arg(config)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .unwrap_or_else(|e| panic!("failed to start the {name}: {e}"))
}

fn wait_for_listener(port: u16) {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + STARTUP;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("nothing listened on {addr} within {STARTUP:?}");
}

/// The five-character level column, padded: ` INFO `, ` WARN `, `ERROR `.
fn count_level(log: &str, level: &str) -> usize {
    log.lines()
        .filter(|l| l.contains(&format!(" {level} ")))
        .count()
}

/// Lines whose level is not one of the three above (DEBUG/TRACE, which the
/// default level does not print).
fn malformed_level_lines(log: &str) -> Vec<String> {
    log.lines()
        .filter(|l| {
            !l.contains(" ERROR ")
                && !l.contains(" WARN ")
                && !l.contains(" INFO ")
                && !l.contains(" DEBUG ")
                && !l.contains(" TRACE ")
        })
        .map(str::to_string)
        .collect()
}

/// A message with its instance-specific parts removed: the timestamp, span
/// field values, addresses and numbers. Two lines with the same shape are the
/// same event happening twice.
fn shape(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_braces = 0usize;
    let mut digits = false;
    for c in line.chars() {
        match c {
            '{' => {
                in_braces += 1;
                continue;
            }
            '}' => {
                in_braces = in_braces.saturating_sub(1);
                continue;
            }
            _ if in_braces > 0 => continue,
            '0'..='9' => {
                if !digits {
                    out.push('N');
                    digits = true;
                }
                continue;
            }
            _ => digits = false,
        }
        out.push(c);
    }
    // The timestamp is the one variable-width prefix; drop it.
    out.split_once(' ')
        .map_or(out.clone(), |(_, rest)| rest.to_string())
}

fn repeated_shapes(log: &str) -> Vec<(usize, String)> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for line in log.lines() {
        let s = shape(line);
        match counts.iter_mut().find(|(k, _)| *k == s) {
            Some((_, n)) => *n += 1,
            None => counts.push((s, 1)),
        }
    }
    let mut over: Vec<(usize, String)> = counts
        .into_iter()
        .filter(|(_, n)| *n > MAX_SHAPE_REPEATS)
        .map(|(s, n)| (n, s))
        .collect();
    over.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
    over
}

/// The happy path: a service is registered, three visitors are forwarded, and
/// both ends shut down. Nothing failed, so nothing may be WARN or ERROR, INFO
/// stays under the ceiling, and no line repeats more than
/// [`MAX_SHAPE_REPEATS`] times.
#[test]
fn a_healthy_run_stays_within_the_log_budget() {
    let mut scenario = Scenario::start("healthy", "");
    for _ in 0..2 {
        scenario.wait_for_forwarding();
    }
    let visitors = scenario.visitors;
    let (server_log, client_log) = scenario.stop();

    let mut problems = Vec::new();
    for (who, log) in [("server", &server_log), ("client", &client_log)] {
        for level in ["WARN", "ERROR"] {
            let n = count_level(log, level);
            if n > 0 {
                let lines: Vec<&str> = log
                    .lines()
                    .filter(|l| l.contains(&format!(" {level} ")))
                    .collect();
                problems.push(format!("{who}: {n} {level} line(s):\n{}", lines.join("\n")));
            }
        }
        let info = count_level(log, "INFO");
        if info > MAX_INFO_LINES {
            problems.push(format!(
                "{who}: {info} INFO lines for one service (ceiling {MAX_INFO_LINES})"
            ));
        }
        for (n, s) in repeated_shapes(log) {
            problems.push(format!("{who}: one message shape {n} times: {s}"));
        }
        let malformed = malformed_level_lines(log);
        if !malformed.is_empty() {
            problems.push(format!("{who}: line without a level: {malformed:?}"));
        }
    }

    assert!(
        problems.is_empty(),
        "a healthy run ({visitors} visitors, both ends shut down cleanly) is not within budget:\n{}",
        problems.join("\n")
    );
    // The scenario is only meaningful if it forwarded something.
    assert!(visitors >= 1, "the scenario never forwarded a visitor");
}

/// The migration wart, measured the same way: a config that still carries the
/// removed `health_check` key starts, **warns** about it (rather than obeying
/// it silently or refusing to start), and that warning is the *only* one.
///
/// The count is deliberately not pinned: the warning is emitted per config
/// parse, and the config watcher parses the file once more when its initial
/// event arrives. Whether that lands inside this test's lifetime depends on the
/// platform's notify backend — macOS delivered it before shutdown and Linux did
/// not — so pinning "exactly one" made the test measure the watcher's timing.
/// What matters is platform-independent: it warns at all, it names the key,
/// everything it warns about is that key, and it is not yet an error.
#[test]
fn a_removed_key_warns_and_is_otherwise_quiet() {
    let mut scenario = Scenario::start(
        "removed-key",
        "health_check = { type = \"tcp\", interval = 10 }\n",
    );
    let (_, client_log) = scenario.stop();

    let warnings: Vec<&str> = client_log
        .lines()
        .filter(|l| l.contains(" WARN "))
        .collect();
    assert!(
        !warnings.is_empty(),
        "the removed key must warn — it is ignored, not silently obeyed:\n{client_log}"
    );
    for line in &warnings {
        assert!(
            line.contains("health_check"),
            "the removed key is the only thing this path may warn about, got: {line}"
        );
    }
    assert_eq!(
        count_level(&client_log, "ERROR"),
        0,
        "the removed key must not be an error yet:\n{client_log}"
    );
}
