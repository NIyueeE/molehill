//! The bench's protocol-level carrier harness.
//!
//! Two carriers, one workload, nothing above either of them: a request/response
//! echo over a raw `TCP` socket (L1) and over the `mhd` carrier's own stream
//! (L2). No yamux, no tunnel pool, no tunnel hello, no control session, no
//! backend — so a difference between the two numbers is the carriers', and the
//! product path is measured separately, by the model
//! (`MHD-FINALIZATION-PLAN.md`, phase 0).
//!
//! # What it emits
//!
//! One JSON object per line on stdout, and nothing else: human-readable lines go
//! to stderr so a runner can pipe stdout straight into an analyzer.
//!
//! - `{"kind":"listening","addr":…}` — the server's bound address, which is how
//!   a runner learns the port of a `:0` bind instead of guessing it.
//! - `{"kind":"lane_setup","us":…}` — the mhd lane's own establishment, once per
//!   client: dial plus whatever the engine does before a stream exists. Counted
//!   apart from a stream open because a pool pays it once per lane while a
//!   stream pays its open once per visitor.
//! - `{"kind":"attempt",…}` — one request. `connect_us` (a fresh TCP socket) and
//!   `open_us` (a fresh carrier stream) are zero when the mode reuses one, which
//!   is what makes `fresh` and `reuse` comparable rather than merely similar.
//!   `post_us` is the request leaving the process, `first_us` the wait for the
//!   first response byte, `complete_us` the wait for the whole response. The
//!   three are separate because they fail separately: a queued request is a
//!   send-path defect, a late first byte is a recovery or wake-up one, and a
//!   slow tail is a fairness one.
//! - `{"kind":"served",…}` — the server's own view of one request: how long the
//!   complete request took to arrive and how long the echo took to write. The
//!   pair, joined by `seq`, is what separates "slow on the wire" from "slow in
//!   the peer".
//! - `{"kind":"summary",…}` — totals, including `failed`, which is the number a
//!   gate must read before it trusts a percentile.
//!
//! # Why the byte check is load-bearing
//!
//! Every echo is compared with what was sent. A carrier that truncates, or that
//! hands back a short read as if it were complete, would otherwise produce a
//! beautiful latency distribution for work it did not do — the same class of
//! error as measuring the backend instead of the tool.
//!
//! # Why one stream handle, not two halves
//!
//! Every exchange here is strictly sequential — write the request, flush, read
//! the reply — so it borrows one `&mut` stream instead of splitting it. That is
//! not a shortcut: a split would make `reuse` impossible (a socket cannot be
//! rejoined after `into_split`), and the fresh/reuse pair is the whole point of
//! the arrival-path measurement.

use std::net::SocketAddr;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Which carrier this process speaks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Raw TCP: one socket is one stream.
    Tcp,
    /// The mhd carrier's own stream over one UDP lane.
    Mhd,
}

/// Which end of the measurement this process is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Server,
    Client,
}

/// Whether a request pays for a new connection or stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StreamMode {
    /// A fresh socket or stream per request: the arrival path.
    Fresh,
    /// One socket or stream, reused: the steady-state path.
    Reuse,
}

/// The measurement's parameters, resolved from the command line once.
struct Args {
    mode: Mode,
    role: Role,
    addr: SocketAddr,
    size: usize,
    requests: usize,
    stream_mode: StreamMode,
}

/// Emit one NDJSON line.
///
/// Values arrive pre-rendered so a number stays a number and a string stays
/// quoted; every string this tool emits is an address or a mode name, neither of
/// which can need escaping. That is a property of the fields, not a hope — a
/// later field carrying free text would have to escape it.
fn emit(kind: &str, fields: &[(&str, String)]) {
    let mut out = String::with_capacity(96);
    out.push_str("{\"kind\":\"");
    out.push_str(kind);
    out.push('"');
    for (key, value) in fields {
        out.push_str(",\"");
        out.push_str(key);
        out.push_str("\":");
        out.push_str(value);
    }
    out.push('}');
    println!("{out}");
}

/// Microseconds, saturating: a run that stalled for hours must not wrap into a
/// fast-looking number.
fn us(elapsed: std::time::Duration) -> u64 {
    u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
}

/// Parse `--flag value` pairs, refusing anything unknown.
fn parse_args() -> Result<Args> {
    let mut mode = None;
    let mut role = None;
    let mut addr = None;
    let mut size = 64usize;
    let mut requests = 1000usize;
    let mut stream_mode = StreamMode::Fresh;

    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| anyhow!("{flag} needs a value"))?;
        match flag.as_str() {
            "--mode" => {
                mode = Some(match value.as_str() {
                    "tcp" => Mode::Tcp,
                    "mhd" => Mode::Mhd,
                    other => bail!("--mode takes tcp or mhd, got {other:?}"),
                });
            }
            "--role" => {
                role = Some(match value.as_str() {
                    "server" => Role::Server,
                    "client" => Role::Client,
                    other => bail!("--role takes server or client, got {other:?}"),
                });
            }
            "--addr" => {
                addr = Some(
                    value
                        .parse::<SocketAddr>()
                        .with_context(|| format!("--addr {value:?} is not host:port"))?,
                );
            }
            "--size" => size = value.parse().context("--size wants bytes")?,
            "--requests" => requests = value.parse().context("--requests wants a count")?,
            "--stream-mode" => {
                stream_mode = match value.as_str() {
                    "fresh" => StreamMode::Fresh,
                    "reuse" => StreamMode::Reuse,
                    other => bail!("--stream-mode takes fresh or reuse, got {other:?}"),
                };
            }
            other => bail!(
                "unknown argument {other:?}; the flags are --mode, --role, --addr, \
                 --size, --requests, --stream-mode"
            ),
        }
    }

    Ok(Args {
        mode: mode.ok_or_else(|| anyhow!("--mode is required"))?,
        role: role.ok_or_else(|| anyhow!("--role is required"))?,
        addr: addr.ok_or_else(|| anyhow!("--addr is required"))?,
        size: size.max(1),
        requests: requests.max(1),
        stream_mode,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    match (args.role, args.mode) {
        (Role::Server, Mode::Tcp) => serve_tcp(args.addr, args.size).await,
        (Role::Server, Mode::Mhd) => serve_mhd(args.addr, args.size).await,
        (Role::Client, Mode::Tcp) => client_tcp(&args).await,
        (Role::Client, Mode::Mhd) => client_mhd(&args).await,
    }
}

/// Read one request's worth of bytes and echo them, for as long as the peer
/// sends.
///
/// Answers the number of requests served, so a stream that ended without one is
/// the absence it is rather than a zero-millisecond success.
async fn echo_requests<S>(stream: &mut S, size: usize) -> Result<u64>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; size];
    let mut served = 0u64;
    loop {
        let started = Instant::now();
        if stream.read_exact(&mut buf).await.is_err() {
            return Ok(served);
        }
        let read_us = us(started.elapsed());
        let writing = Instant::now();
        stream
            .write_all(&buf)
            .await
            .with_context(|| format!("echo of request {served} failed"))?;
        stream.flush().await.ok();
        emit(
            "served",
            &[
                ("seq", served.to_string()),
                ("read_us", read_us.to_string()),
                ("write_us", us(writing.elapsed()).to_string()),
            ],
        );
        served += 1;
    }
}

/// L1's server: one socket, one stream, an echo per request.
async fn serve_tcp(addr: SocketAddr, size: usize) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind {addr}"))?;
    let bound = listener
        .local_addr()
        .context("the listener has no address")?;
    emit(
        "listening",
        &[
            ("addr", format!("\"{bound}\"")),
            ("mode", "\"tcp\"".to_owned()),
        ],
    );
    loop {
        let (mut socket, _) = listener.accept().await.context("accept failed")?;
        tokio::spawn(async move {
            if let Err(error) = echo_requests(&mut socket, size).await {
                eprintln!("harness: tcp stream ended: {error:#}");
            }
        });
    }
}

/// L2's server: one lane, many streams, an echo per request on each.
///
/// The selector byte is consumed first because the carrier's own open writes it
/// and the probe hands the stream over from that point: reading it as payload
/// would shift every request by one byte and turn a correct carrier into a
/// failing one.
async fn serve_mhd(addr: SocketAddr, size: usize) -> Result<()> {
    let mut listener = molehill_rathole::probe::Listener::bind(addr)
        .with_context(|| format!("cannot bind {addr}"))?;
    let bound = listener.local_addr();
    emit(
        "listening",
        &[
            ("addr", format!("\"{bound}\"")),
            ("mode", "\"mhd\"".to_owned()),
        ],
    );
    while let Some(lane) = listener.accept().await {
        tokio::spawn(async move {
            while let Some(mut stream) = lane.accept().await {
                tokio::spawn(async move {
                    let mut selector = [0u8; 1];
                    if stream.read_exact(&mut selector).await.is_err() {
                        return;
                    }
                    if let Err(error) = echo_requests(&mut stream, size).await {
                        eprintln!("harness: mhd stream ended: {error:#}");
                    }
                });
            }
        });
    }
    Ok(())
}

/// One client-side measurement, filled in as the request proceeds.
#[derive(Default)]
struct Attempt {
    connect_us: u64,
    open_us: u64,
    post_us: u64,
    first_us: u64,
    complete_us: u64,
    ok: bool,
}

impl Attempt {
    fn report(&self, seq: usize) {
        emit(
            "attempt",
            &[
                ("seq", seq.to_string()),
                ("connect_us", self.connect_us.to_string()),
                ("open_us", self.open_us.to_string()),
                ("post_us", self.post_us.to_string()),
                ("first_us", self.first_us.to_string()),
                ("complete_us", self.complete_us.to_string()),
                ("ok", u8::from(self.ok).to_string()),
            ],
        );
    }
}

/// Write the request and read the reply, filling `attempt` as each phase ends.
///
/// One function for both carriers, because the phases are the claim being made:
/// if the two clients timed their phases differently, a difference between them
/// could be the harness rather than the carrier.
async fn exchange<S>(
    stream: &mut S,
    payload: &[u8],
    reply: &mut [u8],
    attempt: &mut Attempt,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let posting = Instant::now();
    stream
        .write_all(payload)
        .await
        .context("posting the request")?;
    stream.flush().await.context("flushing the request")?;
    attempt.post_us = us(posting.elapsed());

    let first = Instant::now();
    stream
        .read_exact(&mut reply[..1])
        .await
        .context("waiting for the first response byte")?;
    attempt.first_us = us(first.elapsed());
    if reply.len() > 1 {
        stream
            .read_exact(&mut reply[1..])
            .await
            .context("waiting for the rest of the response")?;
    }
    attempt.complete_us = us(posting.elapsed());
    attempt.ok = reply == payload;
    if !attempt.ok {
        bail!("the reply did not match the request");
    }
    Ok(())
}

/// L1's client: `requests` echoes over one socket or over a socket each.
async fn client_tcp(args: &Args) -> Result<()> {
    let payload = vec![0xA5u8; args.size];
    let mut reply = vec![0u8; args.size];
    let mut held: Option<tokio::net::TcpStream> = None;
    let mut failed = 0usize;
    let reuse = args.stream_mode == StreamMode::Reuse;

    for seq in 0..args.requests {
        let mut attempt = Attempt::default();
        let mut socket = match held.take() {
            Some(socket) => socket,
            None => {
                let started = Instant::now();
                match tokio::net::TcpStream::connect(args.addr).await {
                    Ok(socket) => {
                        attempt.connect_us = us(started.elapsed());
                        socket
                    }
                    Err(error) => {
                        attempt.report(seq);
                        failed += 1;
                        eprintln!("harness: connect {seq} failed: {error}");
                        continue;
                    }
                }
            }
        };
        let outcome = exchange(&mut socket, &payload, &mut reply, &mut attempt).await;
        if let Err(error) = &outcome {
            eprintln!("harness: request {seq}: {error:#}");
        }
        attempt.report(seq);
        if !attempt.ok {
            failed += 1;
            continue;
        }
        // Only a completed exchange hands its socket back: a broken one must not
        // reach the next attempt looking healthy.
        if reuse {
            held = Some(socket);
        }
    }
    emit(
        "summary",
        &[
            ("requests", args.requests.to_string()),
            ("failed", failed.to_string()),
            ("size", args.size.to_string()),
            ("mode", "\"tcp\"".to_owned()),
        ],
    );
    if failed > 0 {
        bail!("{failed} of {} requests failed", args.requests);
    }
    Ok(())
}

/// L2's client: the same workload over the carrier's own streams.
async fn client_mhd(args: &Args) -> Result<()> {
    let setup = Instant::now();
    let lane = molehill_rathole::probe::Lane::dial(args.addr)
        .await
        .with_context(|| format!("cannot dial {} over mhd", args.addr))?;
    emit("lane_setup", &[("us", us(setup.elapsed()).to_string())]);

    let payload = vec![0xA5u8; args.size];
    let mut reply = vec![0u8; args.size];
    let mut held: Option<molehill_rathole::probe::Io> = None;
    let mut failed = 0usize;
    let reuse = args.stream_mode == StreamMode::Reuse;

    for seq in 0..args.requests {
        let mut attempt = Attempt::default();
        let mut stream = match held.take() {
            Some(stream) => stream,
            None => {
                let started = Instant::now();
                match lane.open().await {
                    Ok(stream) => {
                        attempt.open_us = us(started.elapsed());
                        stream
                    }
                    Err(refused) => {
                        attempt.report(seq);
                        failed += 1;
                        eprintln!("harness: stream {seq} refused: {refused}");
                        continue;
                    }
                }
            }
        };
        let outcome = exchange(&mut stream, &payload, &mut reply, &mut attempt).await;
        if let Err(error) = &outcome {
            eprintln!("harness: request {seq}: {error:#}");
        }
        attempt.report(seq);
        if !attempt.ok {
            failed += 1;
            continue;
        }
        if reuse {
            held = Some(stream);
        }
    }
    emit(
        "summary",
        &[
            ("requests", args.requests.to_string()),
            ("failed", failed.to_string()),
            ("size", args.size.to_string()),
            ("mode", "\"mhd\"".to_owned()),
        ],
    );
    if failed > 0 {
        bail!("{failed} of {} requests failed", args.requests);
    }
    Ok(())
}
