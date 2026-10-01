//! A configuration that cannot serve is refused — out loud, and with a
//! non-zero exit.
//!
//! The log contract (docs/configuration.md, "Logging") makes `ERROR` "a human
//! must act". An instance that could not start is the clearest case there is:
//! nothing is listening, so the operator's next move is theirs alone, and the
//! message has to say what to move on. It did neither. `run` spawned the
//! instance and only ever looked at its `Result` when the *next* general
//! configuration change arrived, so every failure before that point — a control
//! port another process already holds, an address that cannot be bound, a key
//! that does not decode — left the process alive, silent and serving nothing.
//! Measured with the control port already bound: four `INFO` lines, "Running as
//! a server" among them, no `ERROR`, no exit.
//!
//! These tests drive the real binary as a subprocess, like the log-budget
//! suite, because the contract is about what a process does at its own
//! boundary: the exit status is half of it.
//!
//! **Unix and native-target only**, on the same grounds as the log-budget
//! suite: the child is spawned and signalled like a service, and under `cross`
//! a spawned child cannot execute what was just built, so those targets report
//! `0 tests`.
#![cfg(all(unix, native_target))]
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "an integration test unwraps and asserts on values it just produced"
)]

use std::{
    fs,
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// The real binary, with the production formatter and level.
const BIN: &str = env!("CARGO_BIN_EXE_molehill");

/// How long a process that cannot serve may take to say so and leave. Generous:
/// the assertion is "it exits at all", and a loaded CI machine is still an
/// order of magnitude below this.
const EXIT_DEADLINE: Duration = Duration::from_secs(30);

/// A directory of our own, so a failure leaves the config behind to be read.
fn workdir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("molehill-startup-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Wait for the child to leave on its own, then return `(exit code, output)`.
///
/// Killing it on the deadline is deliberate: a regression here is "the process
/// stays up", and a test that hangs instead of failing tells nobody why.
fn wait_for_exit(mut child: Child, what: &str) -> (Option<i32>, String) {
    let deadline = Instant::now() + EXIT_DEADLINE;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let out = child.wait_with_output().unwrap();
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            return (status.code(), text);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!(
                "{what} was still running after {EXIT_DEADLINE:?} with nothing serving; \
                 output was:\n{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The control port is held by someone else — the startup failure an operator
/// actually meets (a stale instance, another service on the same port).
///
/// The assertions are the contract: a non-zero exit, the cause, and the config
/// key that carries it (`server.control.bind_addr` is what the operator has to
/// change; the literal address is in the config they just wrote). A silent
/// stay-alive, or an exit with an empty log, both fail here — the first is the
/// defect this test was written for.
#[test]
fn a_server_whose_control_port_is_taken_exits_with_the_reason() {
    let holder = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = holder.local_addr().unwrap();

    let dir = workdir("control-taken");
    let cfg = dir.join("server.toml");
    fs::write(
        &cfg,
        format!(
            "[server]\n\
             default_token = \"startup_failure_token\"\n\
             \n\
             [server.control]\n\
             bind_addr = \"{addr}\"\n"
        ),
    )
    .unwrap();

    let child = Command::new(BIN)
        .arg(&cfg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to start molehill: {e}"));

    let (code, output) = wait_for_exit(child, "a server whose control port is taken");

    assert_ne!(
        code,
        Some(0),
        "a server that could not bind its control port must not exit successfully; \
         output was:\n{output}"
    );
    // The cause is the OS's own wording, and the C libraries do not agree on
    // it: glibc and macOS say "Address already in use", musl says "Address in
    // use" (measured on the release matrix's musl leg, which is the only place
    // these tests meet musl). Assert on what every spelling shares plus the
    // project's own prefix, so the test keeps meaning "the operator is told
    // why" without pinning one libc's phrasing.
    assert!(
        output.contains("Failed to listen at") && output.contains("in use"),
        "the failure must name the cause; output was:\n{output}"
    );
    assert!(
        output.contains("server.control.bind_addr"),
        "the failure must name the key the operator has to change; output was:\n{output}"
    );
}
