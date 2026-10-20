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
//! The answer is deliberately two-layered: every configured service and claim
//! is listed with its configured shape beside the state the sessions actually
//! report (`disconnected` when none does), and every pool states its configured
//! width beside its live tunnels and stream distribution. A reader can tell
//! what is supposed to be there, what is there, and how loaded it is without
//! keeping the config open.
//!
//! The HTTP is written by hand on purpose: one GET path, fixed response
//! headers, no routing, no framework — an axum dependency for a single
//! endpoint is a dependency-graph cost the gates would audit and reject.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::config::{ServiceType, StatusConfig};
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

/// How long the report waits for one session's snapshot. A healthy session
/// answers from its own maps immediately; one that is between reconnects is a
/// gap in the answer, not a reason to hold the whole request open.
const ASK_BUDGET: std::time::Duration = std::time::Duration::from_millis(100);

/// One service as the client's live service table states it. The session
/// answers carry the observed state; this list is what lets a service that no
/// session reports appear as `disconnected` instead of disappearing.
#[derive(Clone)]
pub(crate) struct ConfiguredService {
    pub(crate) name: String,
    pub(crate) kind: &'static str,
    pub(crate) remote_addr: String,
    pub(crate) local_addr: String,
    /// Configured lanes for a transparent claim; `None` for a forwarding
    /// service.
    pub(crate) lanes: Option<u16>,
}

/// The configuration's name for a service kind, used by both the configured
/// list and the session's own view so the two spell it the same way.
pub(crate) fn kind_name(kind: ServiceType) -> &'static str {
    match kind {
        ServiceType::Tcp => "tcp",
        ServiceType::Udp => "udp",
        ServiceType::Transparent => "transparent",
    }
}

/// One service as a session reports it.
pub(crate) struct ServiceSnapshot {
    pub(crate) name: String,
    pub(crate) kind: &'static str,
    pub(crate) remote_addr: String,
    pub(crate) local_addr: String,
    /// `active`, `registering` or `rejected`.
    pub(crate) state: &'static str,
}

/// One L3 lane of one claim, as the status answer states it.
#[derive(Clone, Serialize)]
pub(crate) struct LaneStatus {
    pub(crate) slot: usize,
    pub(crate) flows: usize,
    /// Whether a handover to another carrier is in flight for this lane.
    pub(crate) handover: bool,
}

/// One L3 claim as a session reports it.
pub(crate) struct ClaimSnapshot {
    pub(crate) name: String,
    pub(crate) endpoint: String,
    /// The claim's configured lane share, when the session knows it.
    pub(crate) configured_lanes: Option<u16>,
    pub(crate) lanes: Vec<LaneStatus>,
}

/// One service in the answer: its configured shape, and what the sessions say.
#[derive(Serialize)]
struct ServiceConfigured {
    kind: &'static str,
    remote_addr: String,
    /// A claim has no local backend, so this is omitted for `transparent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    local_addr: Option<String>,
}

#[derive(Serialize)]
struct ServiceActual {
    /// `active`, `registering`, `rejected` or `disconnected`.
    state: &'static str,
}

#[derive(Serialize)]
struct ServiceReport {
    name: String,
    configured: ServiceConfigured,
    actual: ServiceActual,
}

/// One claim in the answer: the lanes it was configured with, and the lanes
/// that are actually attached.
#[derive(Serialize)]
struct ClaimConfigured {
    lanes: u16,
}

#[derive(Serialize)]
struct ClaimActual {
    /// The claim's service state; a lane set can be empty while the service is
    /// active if the lanes are still being re-established.
    state: &'static str,
    lanes_live: usize,
    flows: usize,
    lanes: Vec<LaneStatus>,
}

#[derive(Serialize)]
struct ClaimReport {
    name: String,
    endpoint: String,
    configured: ClaimConfigured,
    actual: ClaimActual,
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

#[derive(Serialize)]
struct PoolConfigured {
    /// The width the pool was asked for (`[client.data].tunnels`).
    size: usize,
}

#[derive(Serialize)]
struct PoolActual {
    /// Whether the session that owns this pool answered the snapshot. A pool
    /// still in the process registry after its session stopped is reported
    /// with `session_up: false` and no live tunnels, not with the stale
    /// counters the registry last held.
    session_up: bool,
    /// Live tunnels; `size < configured.size` is a pool repairing itself.
    size: usize,
    /// Sum of the per-tunnel live streams and pending opens.
    streams: usize,
    pending: usize,
    /// Tunnels carrying no stream, and the busiest tunnel's stream count:
    /// together they show whether the pool is spread or stacked.
    idle_tunnels: usize,
    busiest_streams: usize,
    /// Tunnels still accepting new streams (a handover drains the others).
    accepting_tunnels: usize,
    /// Cumulative handovers the pool has done in each direction while its
    /// session answers; zero when it does not.
    grows: u64,
    shrinks: u64,
    /// The handover in flight, if any.
    handover: Option<String>,
    tunnels: Vec<TunnelStatus>,
}

#[derive(Serialize)]
struct PoolReport {
    /// The pool's key in its session (`session:<data endpoint>`).
    key: String,
    configured: PoolConfigured,
    actual: PoolActual,
}

#[derive(Serialize)]
struct ServiceSummary {
    configured: usize,
    active: usize,
    down: usize,
}

#[derive(Serialize)]
struct ClaimSummary {
    configured: usize,
    active: usize,
    down: usize,
    lanes_configured: usize,
    lanes_live: usize,
}

#[derive(Serialize)]
struct PoolSummary {
    live: usize,
    tunnels_configured: usize,
    tunnels_live: usize,
    streams: usize,
}

/// The whole answer, with the counts a panel would otherwise have to add up.
#[derive(Serialize)]
struct Summary {
    services: ServiceSummary,
    claims: ClaimSummary,
    pools: PoolSummary,
}

#[derive(Serialize)]
struct StatusReport {
    summary: Summary,
    /// Every configured service, plus any service a session reports.
    services: Vec<ServiceReport>,
    /// Every live tunnel pool, with its configured width beside its actual
    /// utilization.
    pools: Vec<PoolReport>,
    /// The transparent claims and their lanes, when this build and config
    /// carry any; absent (not empty) when there are none.
    #[serde(skip_serializing_if = "Option::is_none")]
    claims: Option<Vec<ClaimReport>>,
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
    pub(crate) services: Vec<ServiceSnapshot>,
    /// Empty on a build without the transparent client; the report then has
    /// no claims to show.
    pub(crate) claims: Vec<ClaimSnapshot>,
    /// The session's one tunnel pool key (`session:<data endpoint>`), when it
    /// has a pool. The status task uses it to tell a live pool from a registry
    /// entry whose session has stopped answering.
    pub(crate) pool_key: Option<String>,
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
    configured: tokio::sync::watch::Receiver<Vec<ConfiguredService>>,
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
                let configured = configured.clone();
                tokio::spawn(async move {
                    serve(stream, &token, &asks, &configured).await;
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
    configured: &tokio::sync::watch::Receiver<Vec<ConfiguredService>>,
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

        // Take the configured list once per request; a hot reload during the
        // snapshot is the next request's business.
        let configured = configured.borrow().clone();
        match report(asks, &configured).await {
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
/// answer, merged against the configured services and claims.
///
/// A session that is reconnecting (or was never started) is missing from its
/// part of the answer rather than failing the whole report: the configured
/// list fills the gap with `disconnected`, the pools are a process registry
/// that answers regardless, and a service whose session is between
/// connections is a real state an operator needs to see, not an error.
async fn report(
    asks: &HashMap<String, mpsc::UnboundedSender<SessionAsk>>,
    configured: &[ConfiguredService],
) -> Result<String> {
    let mut observed_services: Vec<ServiceSnapshot> = Vec::new();
    let mut observed_claims: Vec<ClaimSnapshot> = Vec::new();
    let mut answered_pools: HashSet<String> = HashSet::new();
    for ask in asks.values() {
        let (tx, rx) = oneshot::channel();
        if ask.send(SessionAsk::Snapshot(tx)).is_err() {
            continue; // the session is gone; its services are, too
        }
        let Ok(Ok(view)) = tokio::time::timeout(ASK_BUDGET, rx).await else {
            continue; // reconnecting: a gap in the answer, not a hung request
        };
        if let Some(key) = view.pool_key {
            answered_pools.insert(key);
        }
        observed_services.extend(view.services);
        observed_claims.extend(view.claims);
    }

    let pools = pool_reports(&answered_pools);
    let services = service_reports(configured, &observed_services);
    let claims = claim_reports(configured, &services, &observed_claims);
    let summary = summarize(&services, &claims, &pools);
    let answer = StatusReport {
        summary,
        services,
        pools,
        claims: (!claims.is_empty()).then_some(claims),
    };
    serde_json::to_string(&answer).context("status API: serializing the report")
}

/// Merge configured services with what the sessions reported. A configured
/// service no session answers for is `disconnected`; a service a session
/// reports but the startup config did not list (a hot reload) is included with
/// its observed shape, so the answer never silently drops it.
fn service_reports(
    configured: &[ConfiguredService],
    observed: &[ServiceSnapshot],
) -> Vec<ServiceReport> {
    let mut reports = Vec::with_capacity(configured.len() + observed.len());
    for service in configured {
        let state = observed
            .iter()
            .find(|snapshot| snapshot.name == service.name)
            .map_or("disconnected", |snapshot| snapshot.state);
        reports.push(ServiceReport {
            name: service.name.clone(),
            configured: ServiceConfigured {
                kind: service.kind,
                remote_addr: service.remote_addr.clone(),
                local_addr: (service.kind != "transparent").then(|| service.local_addr.clone()),
            },
            actual: ServiceActual { state },
        });
    }
    for snapshot in observed {
        if configured
            .iter()
            .any(|service| service.name == snapshot.name)
        {
            continue;
        }
        reports.push(ServiceReport {
            name: snapshot.name.clone(),
            configured: ServiceConfigured {
                kind: snapshot.kind,
                remote_addr: snapshot.remote_addr.clone(),
                local_addr: (snapshot.kind != "transparent").then(|| snapshot.local_addr.clone()),
            },
            actual: ServiceActual {
                state: snapshot.state,
            },
        });
    }
    reports.sort_by(|a, b| a.name.cmp(&b.name));
    reports
}

/// Merge configured claims with the lanes the sessions actually hold. A claim
/// no session answers for still appears, with its configured lane count and
/// zero live lanes, so "this claim is gone" is visible instead of implicit.
fn claim_reports(
    configured: &[ConfiguredService],
    services: &[ServiceReport],
    observed: &[ClaimSnapshot],
) -> Vec<ClaimReport> {
    let mut reports = Vec::new();
    for claim in configured.iter().filter(|service| service.lanes.is_some()) {
        let lanes = observed
            .iter()
            .find(|snapshot| snapshot.endpoint == claim.remote_addr)
            .map_or_else(Vec::new, |snapshot| snapshot.lanes.clone());
        let state = services
            .iter()
            .find(|service| service.name == claim.name)
            .map_or("disconnected", |service| service.actual.state);
        reports.push(ClaimReport {
            name: claim.name.clone(),
            endpoint: claim.remote_addr.clone(),
            configured: ClaimConfigured {
                lanes: claim.lanes.unwrap_or(0),
            },
            actual: ClaimActual {
                state,
                lanes_live: lanes.len(),
                flows: lanes.iter().map(|lane| lane.flows).sum(),
                lanes,
            },
        });
    }
    for snapshot in observed {
        if configured
            .iter()
            .any(|service| service.lanes.is_some() && service.remote_addr == snapshot.endpoint)
        {
            continue;
        }
        let state = services
            .iter()
            .find(|service| service.name == snapshot.name)
            .map_or("disconnected", |service| service.actual.state);
        let configured_lanes = snapshot
            .configured_lanes
            .unwrap_or_else(|| u16::try_from(snapshot.lanes.len()).unwrap_or(u16::MAX));
        reports.push(ClaimReport {
            name: snapshot.name.clone(),
            endpoint: snapshot.endpoint.clone(),
            configured: ClaimConfigured {
                lanes: configured_lanes,
            },
            actual: ClaimActual {
                state,
                lanes_live: snapshot.lanes.len(),
                flows: snapshot.lanes.iter().map(|lane| lane.flows).sum(),
                lanes: snapshot.lanes.clone(),
            },
        });
    }
    reports.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.endpoint.cmp(&b.endpoint))
    });
    reports
}

/// The counts a reader needs at a glance: how many services and claims are
/// configured, active and down; how many lanes and pool tunnels exist against
/// how many were asked for.
fn summarize(services: &[ServiceReport], claims: &[ClaimReport], pools: &[PoolReport]) -> Summary {
    let services_active = services
        .iter()
        .filter(|service| service.actual.state == "active")
        .count();
    let claims_active = claims
        .iter()
        .filter(|claim| claim.actual.state == "active" && claim.actual.lanes_live > 0)
        .count();
    Summary {
        services: ServiceSummary {
            configured: services.len(),
            active: services_active,
            down: services.len().saturating_sub(services_active),
        },
        claims: ClaimSummary {
            configured: claims.len(),
            active: claims_active,
            down: claims.len().saturating_sub(claims_active),
            lanes_configured: claims
                .iter()
                .map(|claim| usize::from(claim.configured.lanes))
                .sum(),
            lanes_live: claims.iter().map(|claim| claim.actual.lanes_live).sum(),
        },
        pools: PoolSummary {
            live: pools.iter().filter(|pool| pool.actual.session_up).count(),
            tunnels_configured: pools.iter().map(|pool| pool.configured.size).sum(),
            tunnels_live: pools.iter().map(|pool| pool.actual.size).sum(),
            streams: pools.iter().map(|pool| pool.actual.streams).sum(),
        },
    }
}

/// Snapshot every live pool. Without `multiplex` there are none.
#[cfg(feature = "multiplex")]
fn pool_reports(answered_pools: &HashSet<String>) -> Vec<PoolReport> {
    let mut pools: Vec<PoolReport> = multiplex::live_pools()
        .into_iter()
        .map(|snapshot| {
            // A `session:<data endpoint>` pool belongs to the session that
            // answered with this key. If that session did not answer, the
            // registry may still hold its last counters; report the pool as
            // down rather than as a healthy pool that happens to be idle.
            let session_up = if snapshot.key.starts_with("session:") {
                answered_pools.contains(&snapshot.key)
            } else {
                true
            };
            let tunnels: Vec<TunnelStatus> = if session_up {
                snapshot
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
                    .collect()
            } else {
                Vec::new()
            };
            let size = if session_up { snapshot.size } else { 0 };
            let streams = tunnels.iter().map(|tunnel| tunnel.streams).sum();
            let pending = tunnels.iter().map(|tunnel| tunnel.pending).sum();
            let idle_tunnels = tunnels.iter().filter(|tunnel| tunnel.streams == 0).count();
            let busiest_streams = tunnels
                .iter()
                .map(|tunnel| tunnel.streams)
                .max()
                .unwrap_or(0);
            let accepting_tunnels = tunnels.iter().filter(|tunnel| tunnel.accepting).count();
            PoolReport {
                key: snapshot.key.clone(),
                configured: PoolConfigured {
                    size: snapshot.count,
                },
                actual: PoolActual {
                    session_up,
                    size,
                    streams,
                    pending,
                    idle_tunnels,
                    busiest_streams,
                    accepting_tunnels,
                    grows: if session_up { snapshot.grows } else { 0 },
                    shrinks: if session_up { snapshot.shrinks } else { 0 },
                    handover: if session_up {
                        snapshot.handover.map(|(tunnel, carrier, phase)| {
                            format!("tunnel {tunnel} to {}: {phase}", carrier.as_str())
                        })
                    } else {
                        None
                    },
                    tunnels,
                },
            }
        })
        .collect();
    pools.sort_by(|a, b| a.key.cmp(&b.key));
    pools
}

#[cfg(not(feature = "multiplex"))]
fn pool_reports(_answered_pools: &HashSet<String>) -> Vec<PoolReport> {
    Vec::new()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn configured(name: &str, kind: &'static str, lanes: Option<u16>) -> ConfiguredService {
        ConfiguredService {
            name: name.to_owned(),
            kind,
            remote_addr: format!("{name}:8443"),
            local_addr: format!("{name}:8443"),
            lanes,
        }
    }

    fn snapshot(name: &str, state: &'static str) -> ServiceSnapshot {
        ServiceSnapshot {
            name: name.to_owned(),
            kind: "tcp",
            remote_addr: format!("{name}:8443"),
            local_addr: format!("{name}:8443"),
            state,
        }
    }

    #[test]
    fn a_configured_service_no_session_answers_for_is_disconnected() {
        let configured = vec![
            configured("db", "tcp", None),
            configured("web", "tcp", None),
        ];
        let observed = vec![snapshot("web", "active")];
        let reports = service_reports(&configured, &observed);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].name, "db");
        assert_eq!(reports[0].actual.state, "disconnected");
        assert_eq!(reports[1].name, "web");
        assert_eq!(reports[1].actual.state, "active");
    }

    #[test]
    fn a_claim_shows_configured_and_live_lanes() {
        let configured = vec![configured("web", "transparent", Some(2))];
        let observed_service = ServiceSnapshot {
            name: "web".to_owned(),
            kind: "transparent",
            remote_addr: "web:8443".to_owned(),
            local_addr: "web:8443".to_owned(),
            state: "active",
        };
        let services = service_reports(&configured, &[observed_service]);
        assert!(
            services[0].configured.local_addr.is_none(),
            "a claim has no local backend to state"
        );
        let observed_claim = ClaimSnapshot {
            name: "web".to_owned(),
            endpoint: "web:8443".to_owned(),
            configured_lanes: Some(2),
            lanes: vec![LaneStatus {
                slot: 0,
                flows: 3,
                handover: false,
            }],
        };
        let claims = claim_reports(&configured, &services, &[observed_claim]);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].configured.lanes, 2);
        assert_eq!(claims[0].actual.lanes_live, 1);
        assert_eq!(claims[0].actual.flows, 3);
        assert_eq!(claims[0].actual.state, "active");

        let missing = claim_reports(&configured, &[], &[]);
        assert_eq!(missing[0].actual.state, "disconnected");
        assert_eq!(missing[0].actual.lanes_live, 0);
        assert_eq!(missing[0].configured.lanes, 2);
    }

    #[test]
    fn the_summary_counts_down_services_and_pool_utilization() {
        let services = vec![
            ServiceReport {
                name: "up".to_owned(),
                configured: ServiceConfigured {
                    kind: "tcp",
                    remote_addr: "up:8443".to_owned(),
                    local_addr: Some("up:8443".to_owned()),
                },
                actual: ServiceActual { state: "active" },
            },
            ServiceReport {
                name: "down".to_owned(),
                configured: ServiceConfigured {
                    kind: "tcp",
                    remote_addr: "down:8443".to_owned(),
                    local_addr: Some("down:8443".to_owned()),
                },
                actual: ServiceActual {
                    state: "disconnected",
                },
            },
        ];
        let claims = vec![ClaimReport {
            name: "claim".to_owned(),
            endpoint: "claim:8443".to_owned(),
            configured: ClaimConfigured { lanes: 2 },
            actual: ClaimActual {
                state: "active",
                lanes_live: 1,
                flows: 4,
                lanes: Vec::new(),
            },
        }];
        let pools = vec![PoolReport {
            key: "session:server:2333".to_owned(),
            configured: PoolConfigured { size: 4 },
            actual: PoolActual {
                session_up: true,
                size: 3,
                streams: 5,
                pending: 1,
                idle_tunnels: 1,
                busiest_streams: 3,
                accepting_tunnels: 2,
                grows: 2,
                shrinks: 1,
                handover: None,
                tunnels: Vec::new(),
            },
        }];
        let summary = summarize(&services, &claims, &pools);
        assert_eq!(summary.services.configured, 2);
        assert_eq!(summary.services.active, 1);
        assert_eq!(summary.services.down, 1);
        assert_eq!(summary.claims.configured, 1);
        assert_eq!(summary.claims.active, 1);
        assert_eq!(summary.claims.lanes_configured, 2);
        assert_eq!(summary.claims.lanes_live, 1);
        assert_eq!(summary.pools.live, 1);
        assert_eq!(summary.pools.tunnels_configured, 4);
        assert_eq!(summary.pools.tunnels_live, 3);
        assert_eq!(summary.pools.streams, 5);
    }

    #[test]
    fn a_pool_whose_session_is_gone_is_not_live() {
        let pools = vec![PoolReport {
            key: "session:server:2333".to_owned(),
            configured: PoolConfigured { size: 4 },
            actual: PoolActual {
                session_up: false,
                size: 0,
                streams: 0,
                pending: 0,
                idle_tunnels: 0,
                busiest_streams: 0,
                accepting_tunnels: 0,
                grows: 1,
                shrinks: 0,
                handover: None,
                tunnels: Vec::new(),
            },
        }];
        let summary = summarize(&[], &[], &pools);
        assert_eq!(summary.pools.live, 0);
        assert_eq!(summary.pools.tunnels_configured, 4);
        assert_eq!(summary.pools.tunnels_live, 0);
        assert_eq!(summary.pools.streams, 0);
    }
}
