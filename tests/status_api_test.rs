//! The status API: what `GET /status` answers, and what it refuses.
//!
//! The endpoint is the operator's window on a running client — the split
//! mihomo's external-controller made standard (this daemon serves JSON, a
//! panel is somebody else's program) — and a window with a lock on it: the
//! tests here are the lock's. The security tests prove it answers for nobody
//! without the token and nowhere but its one path; the shape tests prove the
//! answer is real (services from the session, pools from the registry). The
//! parser's refusals live with the other config fixtures
//! (`tests/config_test/*/status_*.toml`), which is where every other
//! refusal is pinned.
mod common;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;

use crate::common::{PING, PONG, run_molehill_client, run_molehill_server};

const STATUS_ADDR: &str = "127.0.0.1:2399";
const STATUS_TOKEN: &str = "status-token";

/// One raw HTTP request over a fresh connection; the answer's status line,
/// headers and body as bytes.
async fn status_request(request: &str) -> Result<(String, String)> {
    let mut stream = tokio::net::TcpStream::connect(STATUS_ADDR).await?;
    stream.write_all(request.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let raw = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    Ok((head.to_owned(), body.to_owned()))
}

#[tokio::test]
async fn the_status_api_answers_the_token_and_refuses_everybody_else() -> Result<()> {
    // The local service the fixture forwards to (the same backends the main
    // suite spawns; each test binary is its own process, so this one starts
    // its own).
    tokio::spawn(common::tcp::pingpong_server("127.0.0.1:8080"));
    let (server_tx, server_rx) = broadcast::channel(1);
    let (client_tx, client_rx) = broadcast::channel(1);
    let server = tokio::spawn(run_molehill_server(
        "tests/for_tcp/status_api.toml",
        server_rx,
    ));
    let client = tokio::spawn(run_molehill_client(
        "tests/for_tcp/status_api.toml",
        client_rx,
    ));

    // The service is up: the same readiness the other suite's tests use.
    common::tcp::wait_for_ready("127.0.0.1:2334").await?;

    // No token, wrong scheme, wrong token: the 401 with the challenge, on
    // every shape an attacker or a lazy client tries.
    for request in [
        "GET /status HTTP/1.1\r\nHost: x\r\n\r\n".to_owned(),
        format!("GET /status HTTP/1.1\r\nHost: x\r\nAuthorization: Basic {STATUS_TOKEN}\r\n\r\n"),
        "GET /status HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer wrong\r\n\r\n".to_owned(),
    ] {
        let (head, body) = status_request(&request).await?;
        assert!(
            head.starts_with("HTTP/1.1 401"),
            "an unauthenticated request must be a 401, got: {head}"
        );
        assert!(head.contains("WWW-Authenticate: Bearer"));
        assert!(body.is_empty(), "a refusal carries no body: {body}");
    }

    // The right token: the report, with the service the config declared.
    let (head, body) = status_request(&format!(
        "GET /status HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {STATUS_TOKEN}\r\n\r\n"
    ))
    .await?;
    assert!(head.starts_with("HTTP/1.1 200"), "got: {head}");
    assert!(head.contains("application/json"));
    assert!(head.contains("Access-Control-Allow-Origin: *"));
    let report: serde_json::Value = serde_json::from_str(&body)?;
    let services = report["services"].as_array().unwrap_or(&Vec::new()).clone();
    let names: Vec<&str> = services.iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(
        names.contains(&"echo"),
        "the report must name the running service: {body}"
    );
    let Some(echo) = services
        .iter()
        .find(|s| s["name"] == "echo" && s["actual"]["state"] == "active")
    else {
        anyhow::bail!("the echo service is registered and active: {body}");
    };
    assert_eq!(echo["configured"]["remote_addr"], "0.0.0.0:2334");
    assert_eq!(echo["configured"]["local_addr"], "127.0.0.1:8080");
    assert_eq!(report["summary"]["services"]["active"], 1);
    assert_eq!(report["summary"]["services"]["down"], 0);

    // Anything but GET, and anywhere but /status: refused with a status
    // code, not an error page worth reading.
    let (head, _) = status_request("POST /status HTTP/1.1\r\nHost: x\r\n\r\n").await?;
    assert!(head.starts_with("HTTP/1.1 405"));
    let (head, _) = status_request(&format!(
        "GET / HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {STATUS_TOKEN}\r\n\r\n"
    ))
    .await?;
    assert!(head.starts_with("HTTP/1.1 404"));

    server_tx.send(true)?;
    client_tx.send(true)?;
    server.await??;
    client.await??;
    Ok(())
}

/// The report's pool block is the same registry `MOLEHILL_TUNNEL_STATS`
/// reads: present (empty is a valid state for a client between pools), and
/// its pools state their configured width honestly.
#[tokio::test]
async fn the_report_carries_the_pool_registry() -> Result<()> {
    tokio::spawn(common::tcp::pingpong_server("127.0.0.1:8080"));
    let (server_tx, server_rx) = broadcast::channel(1);
    let (client_tx, client_rx) = broadcast::channel(1);
    let server = tokio::spawn(run_molehill_server(
        "tests/for_tcp/status_api.toml",
        server_rx,
    ));
    let client = tokio::spawn(run_molehill_client(
        "tests/for_tcp/status_api.toml",
        client_rx,
    ));

    common::tcp::wait_for_ready("127.0.0.1:2334").await?;

    // The service answers end to end, so the answer below is about a live
    // forwarding path, not a config file.
    let reply = common::tcp::pingpong_round("127.0.0.1:2334").await?;
    assert_eq!(reply, PONG.as_bytes());

    let (head, body) = status_request(&format!(
        "GET /status HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {STATUS_TOKEN}\r\n\r\n"
    ))
    .await?;
    assert!(head.starts_with("HTTP/1.1 200"));
    let report: serde_json::Value = serde_json::from_str(&body)?;
    let Some(pools) = report["pools"].as_array() else {
        anyhow::bail!("the pools key is always present: {body}");
    };
    for pool in pools {
        // The configured width is stated even mid-repair; the live size
        // cannot exceed it, and the session that owns the pool answered.
        let (size, count) = (
            pool["actual"]["size"].as_u64().unwrap_or(0),
            pool["configured"]["size"].as_u64().unwrap_or(0),
        );
        assert_eq!(pool["actual"]["session_up"], true);
        assert!(
            size <= count,
            "live tunnels above the configured width: {pool}"
        );
        let tunnels = pool["actual"]["tunnels"].as_array().map_or(0, Vec::len);
        assert!(
            tunnels as u64 == size,
            "the per-tunnel block covers every live tunnel: {pool}"
        );
    }

    server_tx.send(true)?;
    client_tx.send(true)?;
    server.await??;
    client.await??;
    Ok(())
}

/// The backend this fixture forwards to: the pingpong server the other
/// suites use, so a status test that reads "active" has proven the whole
/// path, not just the registry.
mod backend {
    use super::*;

    #[tokio::test]
    #[ignore = "the fixture server, started by hand when debugging"]
    async fn pingpong() -> Result<()> {
        common::tcp::pingpong_server("127.0.0.1:8080").await
    }
}

const _: &str = PING;
