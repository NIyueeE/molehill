//! The socket pump: one task per connection that moves datagrams between the
//! state machine and the UDP socket.
//!
//! It is deliberately dumb. Every pass first drains whatever the connection
//! wants to send, then waits for input; the wait is bounded so a state change
//! made by a writer (a new range queued, a credit update) is noticed without the
//! pump having to be woken from outside. A readiness-driven pump belongs with
//! the pool attachment, where the wake-up path is designed once for both.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use super::lane::Shared;

/// How long one pass waits for a datagram before re-checking what it owes the
/// peer.
const IDLE_WAIT: Duration = Duration::from_millis(2);

/// Bind the local socket of a connection.
pub(crate) async fn bind() -> std::io::Result<Arc<UdpSocket>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    Ok(Arc::new(socket))
}

/// Start the pump for one connection.
pub(crate) fn spawn(
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    shared: Arc<Shared>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut out = [0u8; 2048];
        let mut inbox = [0u8; 2048];
        loop {
            // 1. Everything the connection wants to say.
            loop {
                let now = Instant::now();
                let packet = shared.with(|conn| conn.poll_transmit(now, &mut out));
                let Some(len) = packet else { break };
                if socket.send_to(&out[..len], peer).await.is_err() {
                    return;
                }
            }
            // 2. Whatever the peer has said, bounded so a local write is not
            //    left waiting for a silent socket.
            match tokio::time::timeout(IDLE_WAIT, socket.recv_from(&mut inbox)).await {
                Ok(Ok((len, _))) => {
                    let now = Instant::now();
                    shared.with(|conn| {
                        let _ = conn.on_datagram(now, &inbox[..len]);
                    });
                    shared.wake_readers();
                    shared.wake_writers();
                }
                Ok(Err(_)) => return,
                Err(_elapsed) => {}
            }
            shared.wake_pump();
        }
    })
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;
    use crate::transport::mhd::conn::Conn;
    use crate::transport::mhd::lane::StreamIo;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One stream across a real pair of loopback sockets: the pump, the wire
    /// format, the connection and the seam, with nothing faked between them.
    #[tokio::test]
    async fn a_stream_crosses_two_loopback_sockets() {
        let dialer = Shared::new(Conn::new(21, true));
        let acceptor = Shared::new(Conn::new(21, false));
        let dialer_socket = bind().await.unwrap();
        let acceptor_socket = bind().await.unwrap();
        let dialer_addr = dialer_socket.local_addr().unwrap();
        let acceptor_addr = acceptor_socket.local_addr().unwrap();
        let _a = spawn(
            Arc::clone(&dialer_socket),
            acceptor_addr,
            Arc::clone(&dialer),
        );
        let _b = spawn(
            Arc::clone(&acceptor_socket),
            dialer_addr,
            Arc::clone(&acceptor),
        );

        let id = dialer.with(|conn| conn.open().id);
        let mut writer = StreamIo::new(Arc::clone(&dialer), id);
        tokio::spawn(async move {
            writer.write_all(b"across the wire").await.unwrap();
            writer.shutdown().await.unwrap();
        });

        let mut peer_id = None;
        for _ in 0..2000 {
            if let Some(handle) = acceptor.with(crate::transport::mhd::conn::Conn::accept) {
                peer_id = Some(handle.id);
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let Some(peer_id) = peer_id else {
            unreachable!("the peer must see the stream")
        };
        let mut reader = StreamIo::new(Arc::clone(&acceptor), peer_id);
        let mut got = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(5), reader.read_to_end(&mut got)).await;
        let Ok(Ok(_)) = read else {
            unreachable!("the read must finish: got {got:?}")
        };
        assert_eq!(got, b"across the wire");
    }
}
