//! UID resolution, P2P connect and the fragmented UDP connection. Filled in by Tasks 12-15.

pub mod discovery;
pub mod connection;

/// A live P2P session's video stream can burst well above 1MB/s (the
/// main/4K stream's actual configured bitrate — confirmed against real
/// hardware 2026-09-14 via the camera's own `GetEnc` HTTP API: 8192
/// kbit/s). The kernel's default UDP receive buffer (`SO_RCVBUF`, ~208KB
/// on Linux) holds under 200ms of that at full rate; any processing
/// stall longer than that — confirmed to happen, up to ~1.8s at a time,
/// via this client's own GStreamer pipeline diagnostics — overflows it,
/// and the OS silently drops datagrams before this process ever sees
/// them, invisible to any of our own retransmission/reassembly logic. A
/// side-by-side capture against the official Windows app on the exact
/// same relay session measured this client receiving only ~38% of the
/// official app's throughput on the identical nominal stream — this is
/// the leading explanation. Requests a socket buffer generous enough to
/// absorb multi-second stalls at full main-stream bitrate; the OS caps
/// this at `net.core.rmem_max` regardless of what's requested (best
/// effort, not a guarantee — `set_recv_buffer_size`'s own error is
/// deliberately ignored rather than failing the connection over it).
pub(crate) async fn bind_udp_socket_with_large_rcvbuf() -> std::io::Result<tokio::net::UdpSocket> {
    const RCVBUF_SIZE: usize = 4 * 1024 * 1024; // 4MiB
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    socket.set_nonblocking(true)?;
    let _ = socket.set_recv_buffer_size(RCVBUF_SIZE);
    socket.bind(&"0.0.0.0:0".parse::<std::net::SocketAddr>().unwrap().into())?;
    tokio::net::UdpSocket::from_std(socket.into())
}
