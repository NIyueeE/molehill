//! The status API: one read-only HTTP endpoint that answers "what is this
//! client carrying right now".
//!
//! The shape is the external-controller split (mihomo's): this process serves
//! **JSON only** — a panel is someone else's program, reading this one's
//! answer. What makes that split safe here is what the endpoint is bound to:
//! `127.0.0.1` by default, behind a required bearer token, on a port the
//! control protocol never touches. The v5 control session is a Noise-guarded
//! wire of its own; teaching its accept loop to sniff HTTP would put a
//! protocol parser on the one connection the whole data plane trusts, and the
//! token would either bypass the session's authentication or require the
//! observer to speak Noise — so the status listener is a separate, optional
//! listener, and a request to the control port stays a protocol error.
//!
//! Why the client and not the server: the client is where this branch's own
//! instruments already look — the tunnel pool (with its per-tunnel bytes and
//! KCP ARQ health) is the client's, the L3 lane allocator is the client's,
//! and the server's per-tunnel health arrives *at the client* as the tunnel
//! report. The one process that can answer "what are my services and tunnels
//! doing" without a new wire command is the one behind the NAT.
//!
//! The HTTP is written by hand on purpose: one GET path, fixed response
//! headers, no routing, no framework — an axum dependency for a single
//! endpoint is a dependency-graph cost the gates would audit and reject.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::config::StatusConfig;
use crate::protocol;
// The pool registry exists only where tunnels do; without `multiplex` the
// report is services only (the session ask still answers, the pools block
// is empty), which is the same shape a TCP-only deployment has.
#[cfg(feature = "multiplex")]
use crate::transport::multiplex;

/// The one path this API serves. A request for anything else is a 404 with
/// no body worth reading: the surface does not grow by accident.
const PATH: &str = "/status";

/// How long one request may take from accept to written response. A status
/// answer is built from in-memory counters; anything slower than this is a
/// stuck peer holding the accept loop open, not a slow query.
const REQUEST_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// One service this client carries, as the status answer states it.
#[derive(Serialize)]
pub(crate) struct ServiceStatus {
    pub(crate) name: String,
    /// The service's type as the configuration spelled it (`tcp`, `udp`,
    /// `transparent`).
    pub(crate) kind: String,
    /// The public `ip:port` the server holds for it.
    pub(crate) remote_addr: String,
    /// Where this client forwards to.
    pub(crate) local_addr: String,
    /// This session's id for the service, and where it stands.
    pub(crate) state: &'static str,
}

/// One tunnel of one pool, as the status answer states it.
#[derive(Serialize)]
struct TunnelStatus {
    /// Position in the pool (stable between calls, as the pool orders).
    index: usize,
    /// Live streams charged to the tunnel, and how many are pending.
    streams: usize,
    pending: usize,
    /// Whether the tunnel still accepts new streams.
    accepting: bool,
    /// The carrier this tunnel rides (a handover's replacement says so).
    carrier: String,
    /// Cumulative bytes: what the tunnel delivered, what was written in.
    delivered_bytes: u64,
    written_bytes: u64,
    /// KCP ARQ health, `null` for a TCP tunnel: datagrams sent and segments
    /// re-sent, cumulative for the tunnel's life. A re-send rate above
    /// nothing is the carrier losing traffic — the signal the carrier rule
    /// acts on.
    kcp_datagrams_out: Option<u64>,
    kcp_retransmits: Option<u64>,
}

/// One tunnel pool, as the status answer states it.
#[derive(Serialize)]
struct PoolStatus {
    /// The pool's key in its session (`session`, or `service:<id>`).
    key: String,
    carrier: String,
    /// Live tunnels, and the configured count (`size < count`: repairing).
    size: usize,
    count: usize,
    /// Cumulative handovers the pool has done, both directions.
    grows: u64,
    shrinks: u64,
    /// The handover in flight, if any.
    handover: Option<String>,
    tunnels: Vec<TunnelStatus>,
}

/// One L3 lane of one claim, as the status answer states it.
#[cfg(mh_l3_client)]
#[derive(Serialize)]
pub(crate) struct LaneStatus {
    pub(crate) slot: usize,
    pub(crate) flows: usize,
    /// Whether a handover to another carrier is in flight for this lane.
    pub(crate) handover: bool,
}

/// One L3 claim, as the status answer states it.
#[cfg(mh_l3_client)]
#[derive(Serialize)]
pub(crate) struct ClaimStatus {
    /// The claimed endpoint, the claim's identity.
    pub(crate) endpoint: String,
    pub(crate) lanes: Vec<LaneStatus>,
}

/// The whole answer: what this client is carrying right now.
#[derive(Serialize)]
struct StatusReport {
    /// Every service this client runs, by name.
    services: Vec<ServiceStatus>,
    /// Every live tunnel pool, with its tunnels.
    pools: Vec<PoolStatus>,
    /// The transparent claims and their lanes, when this build and config
    /// carry any; absent (not empty) when the feature is not compiled.
    #[cfg(mh_l3_client)]
    #[serde(skip_serializing_if = "Option::is_none")]
    claims: Option<Vec<ClaimStatus>>,
}

/// What one session was asked for and answered with, over the session's own
/// request channel.
///
/// The session task owns the pools, the service table and the lane pool; the
/// status task owns none of them. A snapshot is therefore a request the
/// session answers, not state the status task reads — the same direction the
/// register/deregister asks already travel, so no shared lock appears and the
/// write path is never contended by a reader.
pub(crate) enum SessionAsk {
    /// Ask for this session's own view.
    Snapshot(oneshot::Sender<SessionView>),
}

/// One session's answer: its services, and its transparent claims.
pub(crate) struct SessionView {
    pub(crate) services: Vec<ServiceStatus>,
    /// The session's L3 claims; empty without the transparent feature (the
    /// type itself does not exist on such a build, so the field is a unit —
    /// the report layer gates on the same cfg and never reads it).
    #[cfg(mh_l3_client)]
    pub(crate) claims: Vec<ClaimStatus>,
    #[cfg(not(mh_l3_client))]
    pub(crate) claims: (),
}

/// Run the status API until the process shuts down.
///
/// Called from the client's run loop once the sessions exist; the ask
/// channel is a fan the status task holds one end of, with a sender per
/// session (a session that is reconnecting simply does not answer for a
/// moment, and the report says so with what is still known).
pub async fn run(
    config: StatusConfig,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<bool>,
    asks: HashMap<String, mpsc::UnboundedSender<SessionAsk>>,
) -> Result<()> {
    let listener = TcpListener::bind(&config.bind_addr)
        .await
        .with_context(|| format!("status API: binding {}", config.bind_addr))?;
    info!("status API on {} at GET {PATH}", config.bind_addr);

    let token = config.token;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        warn!("status API: accept failed: {e}");
                        continue;
                    }
                };
                debug!(%peer, "status API: request");
                let token = token.clone();
                let asks = asks.clone();
                tokio::spawn(async move {
                    serve(stream, &token, &asks).await;
                });
            }
            _ = shutdown_rx.recv() => {
                info!("status API: stopping");
                return Ok(());
            }
        }
    }
}

/// Serve one connection: one request, one answer, then close.
///
/// Sequential by design — a status reader is not a load the client needs to
/// parallelize, and a connection that wants more than one request is a
/// crawler, which `Connection: close` is the answer to.
async fn serve(
    mut stream: tokio::net::TcpStream,
    token: &str,
    asks: &HashMap<String, mpsc::UnboundedSender<SessionAsk>>,
) {
    let _ = tokio::time::timeout(REQUEST_BUDGET, async {
        // The request line plus its headers: a status request is small, and
        // a larger "request" is somebody else's protocol arriving here.
        let mut buf = vec![0u8; 4096];
        let mut read = 0usize;
        loop {
            let n = stream.read(&mut buf[read..]).await?;
            read += n;
            if n == 0 || buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            if read == buf.len() {
                return answer(&mut stream, 400, "request too large").await;
            }
        }
        let head = std::str::from_utf8(&buf[..read]).unwrap_or("");
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or("");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");

        if method != "GET" {
            return answer(&mut stream, 405, "only GET is spoken here").await;
        }
        // A query string is not part of the path this API serves.
        let path = path.split('?').next().unwrap_or(path);
        if path != PATH {
            return answer(&mut stream, 404, "not found").await;
        }
        // The one credential check: the bearer token, compared by digest so
        // the comparison itself does not leak its length or its prefix.
        let authorized = lines.by_ref().take_while(|l| !l.is_empty()).any(|line| {
            let Some((name, value)) = line.split_once(':') else {
                return false;
            };
            let name = name.trim_ascii();
            if !name.eq_ignore_ascii_case("authorization") {
                return false;
            }
            let value = value.trim_ascii();
            match value.strip_prefix("Bearer ") {
                Some(presented) => {
                    protocol::digest(presented.as_bytes()) == protocol::digest(token.as_bytes())
                }
                None => false,
            }
        });
        if !authorized {
            // A plain 401 with the challenge header: the response a panel
            // needs to prompt for the token, and an attacker needs nothing
            // more from.
            let _ = stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\n\
                      WWW-Authenticate: Bearer realm=\"molehill status\"\r\n\
                      Content-Length: 0\r\n\
                      Connection: close\r\n\r\n",
                )
                .await;
            return Ok(());
        }

        match report(asks).await {
            Ok(body) => answer(&mut stream, 200, &body).await,
            Err(e) => {
                debug!("status API: building the report failed: {e}");
                answer(&mut stream, 503, "report unavailable").await
            }
        }
    })
    .await;
}

/// Build the report: what the pools registry knows, plus each session's own
/// answer.
///
/// A session that is reconnecting (or was never started) is missing from its
/// part of the answer rather than failing the whole report: the pools are a
/// process registry that answers regardless, and a service whose session is
/// between connections is a real state an operator needs to see, not an
/// error.
async fn report(asks: &HashMap<String, mpsc::UnboundedSender<SessionAsk>>) -> Result<String> {
    #[cfg(feature = "multiplex")]
    let pools: Vec<PoolStatus> = multiplex::live_pools()
        .into_iter()
        .map(|snapshot| PoolStatus {
            key: snapshot.key.clone(),
            carrier: snapshot.carrier.to_owned(),
            size: snapshot.size,
            count: snapshot.count,
            grows: snapshot.grows,
            shrinks: snapshot.shrinks,
            handover: snapshot.handover.map(|(tunnel, carrier, phase)| {
                format!("tunnel {tunnel} to {}: {phase}", carrier.as_str())
            }),
            tunnels: snapshot
                .tunnels
                .iter()
                .enumerate()
                .map(
                    |(index, (streams, pending, _pinned, carrier))| TunnelStatus {
                        index,
                        streams: *streams,
                        pending: *pending,
                        accepting: !snapshot.draining.get(index).copied().unwrap_or(false),
                        carrier: carrier.as_str().to_owned(),
                        delivered_bytes: snapshot.bytes.get(index).map_or(0, |b| b.0),
                        written_bytes: snapshot.bytes.get(index).map_or(0, |b| b.1),
                        kcp_datagrams_out: snapshot
                            .kcp_health
                            .get(index)
                            .and_then(Option::as_ref)
                            .map(|h| h.0),
                        kcp_retransmits: snapshot
                            .kcp_health
                            .get(index)
                            .and_then(Option::as_ref)
                            .map(|h| h.1),
                    },
                )
                .collect(),
        })
        .collect();

    #[cfg(not(feature = "multiplex"))]
    let pools: Vec<PoolStatus> = Vec::new();
    let mut services: Vec<ServiceStatus> = Vec::new();
    #[cfg(mh_l3_client)]
    let mut claims: Vec<ClaimStatus> = Vec::new();
    for ask in asks.values() {
        let (tx, rx) = oneshot::channel();
        if ask.send(SessionAsk::Snapshot(tx)).is_err() {
            continue; // the session is gone; its services are, too
        }
        let Ok(view) = rx.await else {
            continue; // reconnecting: answered next time
        };
        services.extend(view.services);
        #[cfg(mh_l3_client)]
        claims.extend(view.claims);
        #[cfg(not(mh_l3_client))]
        let () = view.claims;
    }
    services.sort_by(|a, b| a.name.cmp(&b.name));

    let answer = StatusReport {
        services,
        pools,
        #[cfg(mh_l3_client)]
        claims: (!claims.is_empty()).then_some(claims),
    };
    serde_json::to_string(&answer).context("status API: serializing the report")
}

/// One fixed-shape HTTP response with CORS open, because the consumer this
/// endpoint exists for is a browser panel that is not this origin.
async fn answer(stream: &mut tokio::net::TcpStream, code: u16, body: &str) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Service Unavailable",
    };
    let json = code == 200;
    let headers = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\r\n",
        if json {
            "application/json"
        } else {
            "text/plain"
        },
        body.len()
    );
    stream.write_all(headers.as_bytes()).await?;
    if !body.is_empty() {
        stream.write_all(body.as_bytes()).await?;
    }
    stream.shutdown().await
}
