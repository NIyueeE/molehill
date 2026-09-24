//! Engine-level tests for the owned write path (link M1).
//!
//! The engine has no vendored unit tests (the integration suite and the
//! bench are its coverage); these cover the one path whose failure mode
//! is silent — a frame body that crosses the command channel by
//! ownership. The borrowed path stays the reference: both must deliver
//! the same bytes with the same stream semantics.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests drive engines and duplexes they constructed themselves"
)]

use crate::mux::Connection;
use crate::mux::Mode;
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

#[tokio::test]
async fn owned_write_reaches_the_peer_stream() {
    use crate::transport::multiplex::{ClientTunnel, mux_config};

    let (client_io, server_io) = duplex(1024 * 1024);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let tunnel = ClientTunnel::start(client_io, mux_config(), shutdown_rx);

    // Server side: drive the connection and echo every accepted stream.
    let server_side = tokio::spawn(async move {
        let mut conn = Connection::new(server_io, mux_config(), Mode::Server);
        while let Some(Ok(mut stream)) = std::future::poll_fn(|cx| conn.poll_next_inbound(cx)).await
        {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });

    // Client side: open a stream (the SYN announcement rides a zero-length
    // borrowed write), then write through the owned path.
    let mut stream = tunnel.open_stream().await.unwrap();
    let payload: Vec<u8> = (0..(64 * 1024u32)).map(|i| (i % 251) as u8).collect();
    let owned = bytes::Bytes::from(payload.clone());
    // A partial write is normal (the split size and the window both cap
    // one call): loop like `write_all` does.
    let mut sent = 0;
    while sent < payload.len() {
        let n = std::future::poll_fn(|cx| {
            std::pin::Pin::new(&mut stream).poll_write_owned(cx, owned.slice(sent..))
        })
        .await
        .unwrap();
        assert!(n > 0, "the owned write stalled");
        sent += n;
    }
    stream.flush().await.unwrap();

    // The echo must come back byte for byte (this also exercises the read
    // path's owned `Chunks` buffer).
    let mut got = vec![0u8; payload.len()];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_exact(&mut got),
    )
    .await
    .expect("the owned write never reached the peer")
    .unwrap();
    assert_eq!(got, payload, "the owned write changed the byte stream");

    // Half-close and let both sides finish.
    stream.shutdown().await.unwrap();
    drop(tunnel);
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_side).await;
}

#[tokio::test]
async fn owned_and_borrowed_writes_interleave() {
    use crate::transport::multiplex::{ClientTunnel, mux_config};

    let (client_io, server_io) = duplex(1024 * 1024);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let tunnel = ClientTunnel::start(client_io, mux_config(), shutdown_rx);

    let server_side = tokio::spawn(async move {
        let mut conn = Connection::new(server_io, mux_config(), Mode::Server);
        while let Some(Ok(mut stream)) = std::future::poll_fn(|cx| conn.poll_next_inbound(cx)).await
        {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });

    let mut stream = tunnel.open_stream().await.unwrap();
    // Alternate the two write shapes on one stream: the frame bodies must
    // arrive in order and intact regardless of which shape carried them.
    let mut expected = Vec::new();
    for round in 0..8u8 {
        if round % 2 == 0 {
            let chunk: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
            expected.extend_from_slice(&chunk);
            stream.write_all(&chunk).await.unwrap();
        } else {
            let chunk =
                bytes::Bytes::from((0..5000u32).map(|i| (i % 251) as u8).collect::<Vec<_>>());
            expected.extend_from_slice(&chunk);
            let n = std::future::poll_fn(|cx| {
                std::pin::Pin::new(&mut stream).poll_write_owned(cx, chunk.clone())
            })
            .await
            .unwrap();
            assert_eq!(n, chunk.len());
        }
    }
    stream.flush().await.unwrap();

    let mut got = vec![0u8; expected.len()];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_exact(&mut got),
    )
    .await
    .expect("interleaved writes never arrived")
    .unwrap();
    assert_eq!(got, expected, "interleaved writes reordered or corrupted");

    stream.shutdown().await.unwrap();
    drop(tunnel);
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_side).await;
}
