use crate::protocol::bc::codec::{read_bc, write_bc};
use crate::protocol::bc::model::Bc;
use crate::protocol::bcudp::codec::{read_bcudp, write_bcudp};
use crate::protocol::bcudp::model::{BcUdp, UdpAck, UdpData};
use crate::protocol::crypto::EncryptionProtocol;
use crate::transport::discovery::PeerHandle;
use crate::Error;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

/// `1350` (the MTU the P2P register/relay handshake actually negotiates —
/// see `M2C_Q_R`'s `mtu` field) minus the 20-byte `UdpData` header. Real
/// hardware Wireshark capture of the official Windows client 2026-09-16
/// (`.plans/reoling-baichuan-p2p-audit-2026-09-16.md`) confirms 1330 is
/// what it actually sends per fragment, matching `bairelay` and
/// `reolink_aio`'s own constants byte-for-byte; this file previously used
/// 1300, a plausible-looking number that was never checked against real
/// traffic.
const MAX_FRAGMENT_SIZE: usize = 1330;

/// Upper bound on how many entries an ack's missing-packet bitmap covers
/// (see `build_ack_payload`) — a stray/malicious `packet_id` far ahead of
/// `next_expected_packet_id` must not drive a multi-GiB allocation here.
const ACK_BITMAP_CAP: u32 = 4096;

/// How often the ack-flush tick (`recv_bc_loop_udp`) sends the current
/// cumulative ack — unconditionally, whether or not anything has changed
/// since the last one. Real hardware Wireshark capture of the official
/// Windows client 2026-09-16 found its own ack cadence is a true periodic
/// timer, median ~31ms, with the *same* `packet_id` repeated in roughly
/// 60% of consecutive acks (i.e. it re-sends the identical cumulative ack
/// even when nothing has arrived since the last one) — not the
/// arrival-triggered/batched scheme this file used before, which only
/// sent an ack from inside the handler for a newly-arrived packet. That
/// scheme could stall indefinitely if the device paused sending while
/// waiting on an ack we hadn't yet decided to send (see the ack-flush
/// tick's own doc comment in `recv_bc_loop_udp` for the deadlock this
/// caused); a real, unconditional periodic timer is simpler and is what
/// the reference client actually does, so this file now matches it
/// instead of re-deriving a different scheme.
const ACK_INTERVAL: Duration = Duration::from_millis(31);

/// How often unacknowledged outgoing `UdpData` (see `Socket::Udp`'s `sent`
/// field) are retransmitted. Matches `bairelay`'s own resend cadence
/// (`bc_protocol/connection/udpsource.rs`) — no independent measurement of
/// the official Windows client's resend timing exists yet (the captures
/// analyzed 2026-09-16 didn't include a loss/resend event to observe), so
/// this is inherited from the one reference that documents it rather than
/// independently derived.
const RESEND_INTERVAL: Duration = Duration::from_millis(500);

/// Cap on outstanding out-of-order packets in `udp_rx_task`'s reorder
/// buffer. Matches `bairelay`'s own `REORDER_CAP`
/// (`bc_protocol/connection/udpsource.rs`) — without a bound, a
/// stray/malicious far-future `packet_id` could grow the buffer to
/// unbounded heap before any error surfaces. 1024 is plenty of slack for
/// legitimate reordering; past that the camera should retransmit anyway
/// once it sees the gap in our ack.
const REORDER_CAP: usize = 1024;

/// Capacity of the channel `udp_rx_task` hands reassembled byte chunks
/// (or its terminal error) to `recv_bc_loop_udp` through. This is the
/// buffering that replaces the old consumer-driven design — see
/// `udp_rx_task`'s own doc comment for why that mattered. 64 chunks is
/// generous slack (each chunk is one UDP datagram's worth of already
/// contiguous, reassembled bytes) without letting a fully stalled
/// consumer grow memory unboundedly.
const RX_CHANNEL_CAPACITY: usize = 64;

/// Upper bound on `udp_rx_task`'s local `pending` overflow queue — see its
/// doc comment for why that queue exists. Deliberately small (a fraction
/// of a second at this camera's ~6-8Mbit/s rate, on top of
/// `RX_CHANNEL_CAPACITY`'s own similarly modest ~85KB of natural
/// buffering) — `pending` exists only to survive `try_send` returning
/// `Full` for a brief moment, not to be a second real buffering strategy
/// stacked in front of `video_view.rs`'s own GStreamer `queue`. Real
/// hardware testing 2026-09-16 found an earlier, much larger value
/// (8MB, several seconds — sized to roughly match that GStreamer queue)
/// was actively harmful: network delivery stayed rock-steady throughout
/// (confirmed via `tshark io,stat` on a real capture — no decline across
/// 108s), yet playback still degraded progressively, because that much
/// upstream slack let bytes sit buffered *here*, invisible to
/// `video_view.rs`'s own `REOLING_DEBUG_PTS` timing, on top of whatever
/// GStreamer's queue was separately doing — doubling the effective
/// buffering depth instead of giving GStreamer (which already does its
/// own pacing/backpressure) a clean signal. A consumer falling behind by
/// more than this small cushion is a real, downstream bottleneck (most
/// likely decode) — the task ends rather than hiding it behind more
/// buffering.
const PENDING_QUEUE_CAP_BYTES: usize = 256 * 1024;

/// `recv_bc`'s receive loop skips discovery-channel packets it doesn't act
/// on (see below) rather than erroring — without a bound on the whole loop,
/// a peer that only ever sends chatter (or nothing at all) on that channel
/// hangs the call forever. Matches `transport::discovery`'s own
/// `OVERALL_TIMEOUT`. Applies to both variants.
const RECV_TIMEOUT: Duration = Duration::from_secs(15);

/// Builds the ack payload documented in `UdpAck`: a `00`/`01` truth table
/// for every packet_id after the one this ack covers, saying which of them
/// have *already* been received out of order. Real hardware resends
/// anything past the acked `packet_id` it doesn't otherwise hear was
/// received — without this, our own acks (previously always empty) gave it
/// no way to tell an already-reassembled out-of-order packet apart from a
/// genuinely lost one, which is one plausible explanation for the roughly
/// one-second bursty stutter seen on the relay-only (non-direct) path;
/// matches `bairelay`'s own `build_send_ack`, the only reference among the
/// three read for this project that actually populates this field instead
/// of leaving it empty like `neolink` does.
fn build_ack_payload(next_expected: u32, out_of_order: &BTreeMap<u32, Vec<u8>>) -> Vec<u8> {
    let Some(&highest) = out_of_order.keys().next_back() else {
        return Vec::new();
    };
    let end_exclusive = highest.saturating_add(1).min(next_expected.saturating_add(ACK_BITMAP_CAP));
    (next_expected..end_exclusive).map(|id| u8::from(out_of_order.contains_key(&id))).collect()
}

/// Receiver-side throughput meter, reported in every outgoing ack's
/// `received_bytes_per_sec` field (`UdpAck::received_bytes_per_sec`).
///
/// **This field is not a latency.** A 2026-09-20 capture of the official
/// Windows client streaming the main profile (`/tmp/official-main.pcap`,
/// 79s) shows it is the *payload bytes per second the client received*
/// over the previous whole second, refreshed once a second: at t=26.38s
/// the ack field was 1077307, and the `UdpData` payload received in the
/// preceding second was exactly 1077307 bytes (the two match to the byte
/// on every one of the 14 samples checked). The camera appears to use it
/// as receiver feedback for its send rate. Our earlier value — the mean
/// gap between incoming acks in microseconds, ~22000, copied from
/// `bairelay`'s `AckLatency` — told the camera we were receiving ~22KB/s;
/// a second, independent client (a rewrite that sent `0`) sat at the same
/// ~6.3Mbit/s ceiling, while the official client, reporting its real
/// ~1.08MB/s, is sent the encoder's full ~8.3+Mbit/s.
struct RateMeter {
    bytes: u64,
    window_start: std::time::Instant,
    /// Bytes/second measured over the last completed window; `0` until the
    /// first second has elapsed (the official client also starts at 0).
    reported: u32,
}

impl RateMeter {
    fn new() -> Self {
        Self { bytes: 0, window_start: std::time::Instant::now(), reported: 0 }
    }

    fn add(&mut self, payload_bytes: usize) {
        self.bytes += payload_bytes as u64;
    }

    /// Call once a second; closes the current window.
    fn roll(&mut self) {
        let now = std::time::Instant::now();
        let secs = now.duration_since(self.window_start).as_secs_f64();
        if secs > 0.0 {
            self.reported = (self.bytes as f64 / secs).min(u32::MAX as f64) as u32;
        }
        self.bytes = 0;
        self.window_start = now;
    }

    fn get_value(&self) -> u32 {
        self.reported
    }
}

/// Outgoing-side UDP state shared between `send_bc` (the consumer's own
/// thread, via `&mut BcConnection`) and `udp_rx_task` (a spawned
/// background task): `send_bc` appends to `sent` as it fragments a
/// message onto the wire; the task removes entries as the camera acks
/// them and retransmits whatever's still here on `resend_tick`. Behind a
/// `tokio::sync::Mutex` (async-aware — held briefly across the socket
/// send inside `send_bc`) rather than duplicated per-side, since both
/// sides need a consistent view of what's still unacknowledged.
struct UdpSendState {
    send_packet_id: u32,
    sent: BTreeMap<u32, UdpData>,
}

/// The two ways a `BcConnection` can actually be talking to a device.
/// `Bc`-level framing (`read_bc`/`write_bc`) and encryption are identical
/// either way — only the raw bytes-on-the-wire mechanics differ, so this
/// enum is the only place that knows the difference.
enum Socket {
    /// P2P (UID-resolved, direct or relay). Needs the `BcUdp` envelope:
    /// fragmentation into `MAX_FRAGMENT_SIZE` chunks, ACKs, out-of-order
    /// reassembly by `packet_id`, and a negotiated peer to validate
    /// incoming datagrams against. The actual receive/ack/resend/reorder
    /// work happens in a background task (`udp_rx_task`, spawned by
    /// `BcConnection::new`) — see its doc comment for why.
    Udp {
        socket: Arc<UdpSocket>,
        peer: PeerHandle,
        send_state: Arc<tokio::sync::Mutex<UdpSendState>>,
        /// Reassembled, contiguous byte chunks from `udp_rx_task` (or the
        /// terminal error that ended it) — `recv_bc_loop_udp` just reads
        /// from this and runs `read_bc` over the accumulated bytes,
        /// exactly like `recv_bc_loop_tcp` already does with its own
        /// incrementally-filled buffer.
        rx: tokio::sync::mpsc::Receiver<crate::Result<Vec<u8>>>,
        /// Kept only so `Drop` can abort it — a dropped `JoinHandle`
        /// doesn't stop a tokio task, it just detaches. Never polled
        /// directly.
        rx_task: tokio::task::JoinHandle<()>,
    },
    /// Direct TCP (Baichuan's "Basic Service", typically port 9000). A
    /// plain ordered byte stream — `write_bc`'s output goes straight on
    /// the wire with no envelope, and `read_bc` already knows how to wait
    /// for "not enough bytes yet" (`Ok(None)`), which is exactly what an
    /// incrementally-filled TCP buffer needs.
    Tcp { stream: TcpStream },
}

pub struct BcConnection {
    socket: Socket,
    reassembly: Vec<u8>,
    // msg_nums a prior message told us (via <binaryData>) are mid video/audio
    // stream — see `read_bc`'s own doc comment for why this matters for
    // decryption of the chunks that follow.
    bin_mode: HashSet<u16>,
}

impl BcConnection {
    pub fn new(socket: Arc<UdpSocket>, peer: PeerHandle) -> Self {
        let send_state = Arc::new(tokio::sync::Mutex::new(UdpSendState {
            send_packet_id: 0,
            sent: BTreeMap::new(),
        }));
        let (tx, rx) = tokio::sync::mpsc::channel(RX_CHANNEL_CAPACITY);
        let rx_task = tokio::spawn(udp_rx_task(socket.clone(), peer.clone(), send_state.clone(), tx));
        Self {
            socket: Socket::Udp { socket, peer, send_state, rx, rx_task },
            reassembly: Vec::new(),
            bin_mode: HashSet::new(),
        }
    }

    /// Best-effort notice to the device that this session is ending, so it
    /// stops streaming immediately instead of continuing until its own
    /// idle timeout — see `UdpXml::C2dDisc`, already modeled in
    /// the `protocol` module but never sent anywhere until now. Confirmed
    /// real 2026-09-16: closing the official Windows app makes the camera
    /// stop sending immediately; closing this app (before this fix) left
    /// the camera sending until its own TTL expired, because nothing here
    /// ever told it we were leaving. No TCP equivalent — a clean TCP EOF
    /// (see `recv_bc_loop_tcp`'s own doc comment) is the only "goodbye"
    /// that transport has, and closing the stream already sends one.
    pub async fn disconnect(&self) {
        if let Socket::Udp { socket, peer, .. } = &self.socket {
            let msg = BcUdp::Discovery(crate::protocol::bcudp::model::UdpDiscovery {
                tid: rand::random::<u32>().max(1),
                payload: crate::protocol::bcudp::xml::UdpXml::C2dDisc(
                    crate::protocol::bcudp::xml::C2dDisc {
                        cid: peer.local_connection_id,
                        did: peer.remote_connection_id,
                    },
                ),
            });
            let _ = socket.send_to(&write_bcudp(&msg), peer.addr).await;
        }
    }

    /// The login nonce delivered during the relay handshake, if any — see
    /// [`PeerHandle::nonce`]. Always `None` on a direct TCP connection —
    /// that path has no P2P handshake to carry one; `login()` falls back
    /// to the legacy nonce exchange exactly as it does for a nonce-less
    /// UDP connection.
    pub fn peer_nonce(&self) -> Option<&str> {
        match &self.socket {
            Socket::Udp { peer, .. } => peer.nonce.as_deref(),
            Socket::Tcp { .. } => None,
        }
    }

    /// The peer's address this connection is actually talking to —
    /// diagnostic only, to tell a direct connection apart from a relay
    /// one. `TcpStream::peer_addr()` succeeds on any connected socket —
    /// every real construction of `Socket::Tcp` (via `from_tcp`, whether
    /// fed a `TcpStream::connect` result in `ReolinkClient::connect_by_ip`
    /// or a `TcpListener::accept()` result in tests) already holds a
    /// connected stream, so the `.expect()` here can't actually fail.
    pub fn peer_addr(&self) -> std::net::SocketAddr {
        match &self.socket {
            Socket::Udp { peer, .. } => peer.addr,
            Socket::Tcp { stream } => stream
                .peer_addr()
                .expect("peer_addr always succeeds after a successful TcpStream::connect"),
        }
    }

    /// Wraps an already-connected `TcpStream` (Baichuan's direct TCP
    /// "Basic Service", typically port 9000) as a `BcConnection`.
    pub fn from_tcp(stream: TcpStream) -> Self {
        Self {
            socket: Socket::Tcp { stream },
            reassembly: Vec::new(),
            bin_mode: HashSet::new(),
        }
    }

    /// On a direct (non-relay) **UDP** connection, spawns a background task
    /// that sends `C2D_HB` to the device once a second for as long as the
    /// returned handle is held. No-op (returns `None`) on a relay
    /// connection, and always `None` on TCP — `C2D_HB` is a P2P/UDP NAT
    /// keepalive with no TCP equivalent; a stable TCP connection needs no
    /// such mechanism and must not send one. Real hardware, confirmed
    /// 2026-09-13: without this on the UDP direct path, the device just
    /// keeps retransmitting its `D2C_C_R` handshake reply every ~500ms and
    /// never processes any BC data sent to it — see `UdpXml::C2dHb`.
    pub fn spawn_direct_keepalive(&self) -> Option<tokio::task::JoinHandle<()>> {
        let Socket::Udp { socket, peer, .. } = &self.socket else {
            return None;
        };
        if !peer.is_direct {
            return None;
        }
        let socket = socket.clone();
        let addr = peer.addr;
        let cid = peer.local_connection_id;
        let did = peer.remote_connection_id;
        Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let hb = BcUdp::Discovery(crate::protocol::bcudp::model::UdpDiscovery {
                    tid: rand::random::<u32>().max(1),
                    payload: crate::protocol::bcudp::xml::UdpXml::C2dHb(crate::protocol::bcudp::xml::C2dHb { cid, did }),
                });
                if socket.send_to(&write_bcudp(&hb), addr).await.is_err() {
                    break;
                }
            }
        }))
    }

    pub async fn send_bc(&mut self, bc: &Bc, enc: &EncryptionProtocol) -> crate::Result<()> {
        let bytes = write_bc(bc, enc);
        match &mut self.socket {
            Socket::Udp { socket, peer, send_state, .. } => {
                // Held across the sends below (all on the same `Arc<UdpSocket>`,
                // no contention with `udp_rx_task`'s own `recv_from`) so a
                // concurrent `resend_tick` can't retransmit a packet_id this
                // loop hasn't finished inserting into `sent` yet.
                let mut state = send_state.lock().await;
                for chunk in bytes.chunks(MAX_FRAGMENT_SIZE) {
                    let packet = UdpData {
                        connection_id: peer.remote_connection_id,
                        packet_id: state.send_packet_id,
                        payload: chunk.to_vec(),
                    };
                    socket.send_to(&write_bcudp(&BcUdp::Data(packet.clone())), peer.addr).await?;
                    // Kept until acked (see `udp_rx_task`'s handling of
                    // incoming `BcUdp::Ack`) so its `resend_tick` can
                    // retransmit it if the camera never confirms receipt.
                    state.sent.insert(packet.packet_id, packet);
                    state.send_packet_id += 1;
                }
                Ok(())
            }
            Socket::Tcp { stream } => {
                stream.write_all(&bytes).await?;
                Ok(())
            }
        }
    }

    pub async fn recv_bc(&mut self, enc: &EncryptionProtocol) -> crate::Result<Bc> {
        // A message already fully reassembled from a previous call?
        if let Some((bc, used)) = read_bc(&self.reassembly, enc, &mut self.bin_mode)? {
            self.reassembly.drain(..used);
            return Ok(bc);
        }

        tokio::time::timeout(RECV_TIMEOUT, self.recv_bc_loop(enc))
            .await
            .map_err(|_| Error::ProtocolError("timed out waiting for a reply".to_string()))?
    }

    async fn recv_bc_loop(&mut self, enc: &EncryptionProtocol) -> crate::Result<Bc> {
        if matches!(self.socket, Socket::Tcp { .. }) {
            self.recv_bc_loop_tcp(enc).await
        } else {
            self.recv_bc_loop_udp(enc).await
        }
    }

    async fn recv_bc_loop_tcp(&mut self, enc: &EncryptionProtocol) -> crate::Result<Bc> {
        // A single Baichuan video message can be tens of KB (confirmed
        // against real hardware 2026-09-15: ~35KB average for a main-stream
        // I-frame chunk). At the old 2048-byte size, assembling one such
        // message needed on the order of ~17 separate `.await` reads, and
        // a full ~700KB keyframe (spread across ~18 such messages) needed
        // several hundred — real-hardware timing instrumentation pointed
        // at per-await scheduling overhead, multiplied by that many
        // iterations, as a leading candidate for the multi-hundred-ms to
        // ~1s+ delay seen only on keyframes (small P-frames need only a
        // handful of reads either way, matching their sub-40ms gaps).
        // 2048 was never sized for that — it was just a plausible-looking
        // default. Large enough that a normal-sized chunk arrives in one
        // read even under some fragmentation, without being wastefully
        // oversized for the ~20-byte header-only reads this also serves.
        let mut buf = [0u8; 65536];
        loop {
            // Scoped to just this read: the mutable borrow of `self.socket`
            // must not overlap the `self.reassembly` access below it, or
            // the borrow checker sees two live mutable borrows of `self`
            // across the same `.await`.
            let n = {
                let Socket::Tcp { stream } = &mut self.socket else {
                    unreachable!("recv_bc_loop_tcp called on a non-TCP connection");
                };
                stream.read(&mut buf).await?
            };
            if n == 0 {
                // The peer closed the connection — there is no BC-level
                // "goodbye" on this transport the way `D2C_DISC` is one on
                // UDP; a clean TCP EOF just means the session is over.
                return Err(Error::ConnectionLost);
            }
            self.reassembly.extend_from_slice(&buf[..n]);
            if let Some((bc, used)) = read_bc(&self.reassembly, enc, &mut self.bin_mode)? {
                self.reassembly.drain(..used);
                return Ok(bc);
            }
        }
    }

    /// Just reads reassembled byte chunks off `udp_rx_task`'s channel and
    /// runs the same `read_bc` accumulation loop `recv_bc_loop_tcp` uses —
    /// all the transport-level work (recv_from, ack, resend, out-of-order
    /// reassembly) already happened in that background task. See its doc
    /// comment for why this is split out rather than done inline here.
    async fn recv_bc_loop_udp(&mut self, enc: &EncryptionProtocol) -> crate::Result<Bc> {
        loop {
            let chunk = {
                let Socket::Udp { rx, .. } = &mut self.socket else {
                    unreachable!("recv_bc_loop_udp called on a non-UDP connection");
                };
                rx.recv().await.ok_or(Error::ConnectionLost)??
            };
            self.reassembly.extend_from_slice(&chunk);
            if let Some((bc, used)) = read_bc(&self.reassembly, enc, &mut self.bin_mode)? {
                self.reassembly.drain(..used);
                return Ok(bc);
            }
        }
    }
}

impl Drop for BcConnection {
    fn drop(&mut self) {
        // A dropped `JoinHandle` does not stop a tokio task on its own —
        // it just detaches, leaving `udp_rx_task` running (and holding the
        // socket) for as long as the process lives otherwise. Explicit
        // abort matches the `sent`/`out_of_order` state going away with
        // this connection.
        if let Socket::Udp { rx_task, .. } = &self.socket {
            rx_task.abort();
        }
    }
}

/// Background task that owns a UDP `BcConnection`'s entire receive side —
/// `recv_from`, periodic acks/resends, and out-of-order reassembly —
/// decoupled from the consumer via `tx` instead of being driven by the
/// consumer's own `recv_bc()` calls.
///
/// **Why this exists** (found 2026-09-16: `.plans/reoling-baichuan-p2p-audit-2026-09-16.md`
/// sections 32-33, cross-checked against `bairelay`'s
/// `bc_protocol/connection/udpsource.rs`, which splits its own raw-socket
/// task from the consumer-facing stream the exact same way). The previous
/// design ran this same logic inline inside what was then
/// `recv_bc_loop_udp`, called synchronously by the consumer once per `Bc`
/// message it wanted. Between calls — while the consumer was busy parsing
/// `BcMedia`, feeding GStreamer's `appsrc`, or otherwise processing the
/// message it just got — nothing was reading the socket at all. Real
/// hardware pushes the main stream continuously at up to the camera's
/// full ~8Mbit/s encoder rate; any consumer-side stall (even a brief one,
/// e.g. a keyframe push blocking on a full GStreamer queue) let the OS
/// kernel receive buffer fill and then drop datagrams — read live as
/// smooth playback for the first several seconds (kernel buffer had
/// slack) degrading into stutter once it didn't. This task keeps draining
/// the socket regardless of consumer pace; `RX_CHANNEL_CAPACITY` provides
/// the buffering instead.
///
/// Only the raw reassembled bytes go through `tx`, not parsed `Bc`
/// messages — `read_bc` needs the connection's current
/// `EncryptionProtocol`, which can change mid-connection (unencrypted
/// until login negotiates AES) and this task has no way to learn about
/// that. `recv_bc_loop_udp` does the actual `read_bc` parsing, exactly
/// like `recv_bc_loop_tcp` already does with its own incrementally-filled
/// buffer — this task's job ends at "contiguous bytes, in order".
///
/// **A first version of this task `.await`ed `tx.send()` directly inside
/// the `recv_from` arm** — confirmed on real hardware 2026-09-16 to be a
/// real bug, not just a theoretical one: when `RX_CHANNEL_CAPACITY` filled
/// (consumer briefly behind), that await parked the *entire* `select!`,
/// including `ack_tick`. `bairelay`'s own comment on its equivalent timer
/// is blunt about why that matters: "Offical Client does ack every 10ms
/// if we don't also do this the camera seems to think we have a poor
/// connection and will abort." A stalled ack tick reads to the camera as
/// exactly that, and it responds by throttling/resending — a
/// self-reinforcing slowdown that didn't exist in the old (consumer-
/// driven) design, whose ack tick lived in the same loop but the loop
/// never blocked on a channel. Dropping the data instead (matching the
/// kernel buffer's old behavior) isn't a safe fix either: unlike a
/// datagram the kernel drops before we ever see it, a chunk here has
/// already been reassembled and (implicitly, via our own ack bitmap)
/// promised to the camera as received — dropping it after that point
/// corrupts `recv_bc_loop_udp`'s byte-stream framing permanently, with no
/// retransmission able to recover it. `pending` is the fix: an
/// always-synchronous local queue the recv arm and `ack_tick` both try to
/// flush via non-blocking `try_send`, so a slow consumer delays delivery
/// (bounded by `PENDING_QUEUE_CAP_BYTES`) without ever blocking the
/// select loop and without ever losing a byte short of that cap.
async fn udp_rx_task(
    socket: Arc<UdpSocket>,
    peer: PeerHandle,
    send_state: Arc<tokio::sync::Mutex<UdpSendState>>,
    tx: tokio::sync::mpsc::Sender<crate::Result<Vec<u8>>>,
) {
    let mut buf = [0u8; 2048];
    let mut next_expected_packet_id: u32 = 0;
    let mut out_of_order: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut rate_meter = RateMeter::new();
    let mut rate_tick = tokio::time::interval(Duration::from_secs(1));
    rate_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    rate_tick.reset();

    // Overflow queue for chunks `tx` couldn't immediately accept — see
    // this function's own doc comment. `pending_bytes` is a running total
    // so checking the cap doesn't need to re-sum the queue every time.
    let mut pending: std::collections::VecDeque<Vec<u8>> = std::collections::VecDeque::new();
    let mut pending_bytes: usize = 0;

    // Same cadences and rationale as the inline loop this replaced — see
    // `ACK_INTERVAL`/`RESEND_INTERVAL`'s own doc comments.
    let mut ack_tick = tokio::time::interval(ACK_INTERVAL);
    ack_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut resend_tick = tokio::time::interval(RESEND_INTERVAL);
    resend_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            recv_result = socket.recv_from(&mut buf) => {
                let (n, from) = match recv_result {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = tx.send(Err(e.into())).await;
                        return;
                    }
                };
                if from != peer.addr {
                    // Ignore datagrams from anyone but our negotiated peer —
                    // the socket is unconnected, so without this check any host
                    // that can reach our ephemeral port could inject or
                    // overwrite fragments (spoofed duplicate packet_id).
                    continue;
                }
                // A discovery-channel packet we don't (yet) model fails to
                // parse inside read_bcudp itself; that shouldn't kill an
                // otherwise-healthy connection, so skip it rather than
                // propagating — real hardware sends several kinds of chatter on
                // this channel we don't need to act on. `D2C_DISC` (below) is
                // the one variant worth surfacing, since it means the session
                // is actually gone.
                let Ok(Some((msg, _))) = read_bcudp(&buf[..n]) else {
                    continue;
                };
                match msg {
                    BcUdp::Data(data) => {
                        rate_meter.add(data.payload.len());
                        // `packet_id == u32::MAX` is refused outright, matching
                        // bairelay's own `UdpFlowState::handle_data` — accepting
                        // it would let `build_ack_payload`'s `highest.saturating_add(1)`
                        // land back on 0, corrupting the ack-range arithmetic.
                        // `>= next_expected_packet_id` rejects already-consumed
                        // duplicates; like bairelay's equivalent `packets_want`
                        // check, this compares two `u32`s that both wrap at
                        // `u32::MAX` in the same direction, so it stays correct
                        // across a wrap — only a pathological reorder spanning
                        // more than half the entire `u32` space could fool it.
                        if data.packet_id != u32::MAX
                            && data.packet_id >= next_expected_packet_id
                            && out_of_order.len() < REORDER_CAP
                        {
                            out_of_order.insert(data.packet_id, data.payload);
                        }
                        // else: already-consumed duplicate, or the reorder
                        // buffer is at REORDER_CAP — drop it; the camera
                        // will retransmit once our next ack shows the gap.
                        while let Some(chunk) = out_of_order.remove(&next_expected_packet_id) {
                            next_expected_packet_id = next_expected_packet_id.wrapping_add(1);
                            pending_bytes += chunk.len();
                            pending.push_back(chunk);
                        }
                        if pending_bytes > PENDING_QUEUE_CAP_BYTES {
                            // The consumer is durably behind, not just
                            // momentarily — more buffering can't fix that.
                            // End the connection cleanly rather than
                            // growing memory without bound or silently
                            // corrupting the byte stream by dropping.
                            let _ = tx
                                .try_send(Err(Error::ProtocolError(
                                    "consumer fell too far behind (pending queue over cap)".to_string(),
                                )));
                            return;
                        }
                        if !flush_pending(&tx, &mut pending, &mut pending_bytes) {
                            return; // consumer dropped the connection
                        }
                    }
                    BcUdp::Ack(ack) => {
                        // `0xffffffff` is the peer's own "nothing acked
                        // yet" sentinel (see `send_ack`'s doc comment) —
                        // not a real cumulative packet_id to act on.
                        if ack.packet_id != 0xffffffff {
                            let start = ack.packet_id;
                            // `start + 1 + idx` guarded against overflow the
                            // same way `handle_ack` in bairelay's own
                            // `UdpFlowState` is — `ack.payload` is wire data
                            // (bounded by one UDP datagram in practice, but
                            // not something to trust blindly for arithmetic).
                            let payload_len = ack.payload.len() as u64;
                            let mut state = send_state.lock().await;
                            state.sent.retain(|&k, _| k > start);
                            if (start as u64).saturating_add(1).saturating_add(payload_len)
                                <= u32::MAX as u64
                            {
                                for (idx, &value) in ack.payload.iter().enumerate() {
                                    if value > 0 {
                                        let packet_id = start.wrapping_add(1).wrapping_add(idx as u32);
                                        state.sent.remove(&packet_id);
                                    }
                                }
                            }
                        }
                    }
                    // Other discovery-channel chatter (our own echoed C2D_DISC,
                    // keepalives, etc.) is not relevant here; only D2C_DISC —
                    // the device ending the session — is.
                    BcUdp::Discovery(disc) => {
                        if let crate::protocol::bcudp::xml::UdpXml::D2cDisc(_) = disc.payload {
                            let _ = tx
                                .send(Err(Error::ProtocolError(
                                    "device disconnected the session (D2C_DISC)".to_string(),
                                )))
                                .await;
                            return;
                        }
                    }
                }
            }
            _ = rate_tick.tick() => {
                rate_meter.roll();
            }
            _ = ack_tick.tick() => {
                // Retry flushing `pending` here too, not just when new
                // data arrives — the consumer may have freed up channel
                // capacity during a quiet network moment with nothing new
                // to trigger a flush otherwise. This never blocks (see
                // `flush_pending`), so it can't delay the ack this tick
                // exists to guarantee.
                if !flush_pending(&tx, &mut pending, &mut pending_bytes) {
                    return;
                }
                if send_ack(&socket, &peer, next_expected_packet_id, &out_of_order, rate_meter.get_value())
                    .await
                    .is_err()
                {
                    return;
                }
            }
            _ = resend_tick.tick() => {
                // Snapshot-then-send rather than holding the lock across
                // every send_to: resending is best-effort and shouldn't
                // block send_bc's own lock acquisition for the whole batch.
                let packets: Vec<UdpData> = send_state.lock().await.sent.values().cloned().collect();
                for packet in packets {
                    if socket.send_to(&write_bcudp(&BcUdp::Data(packet)), peer.addr).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

/// Drains as much of `pending` into `tx` as it will immediately accept,
/// via non-blocking `try_send` — never `.await`s, so a caller inside
/// `udp_rx_task`'s `select!` can call this from any arm without risking
/// starving the others (see `udp_rx_task`'s own doc comment for the real
/// bug this replaced). Returns `false` if `tx`'s receiver is gone
/// (`recv_bc_loop_udp`/`BcConnection` dropped) — callers should end the
/// task in that case, matching every other "consumer gone" exit here.
fn flush_pending(
    tx: &tokio::sync::mpsc::Sender<crate::Result<Vec<u8>>>,
    pending: &mut std::collections::VecDeque<Vec<u8>>,
    pending_bytes: &mut usize,
) -> bool {
    while let Some(chunk) = pending.pop_front() {
        let len = chunk.len();
        match tx.try_send(Ok(chunk)) {
            Ok(()) => {
                *pending_bytes -= len;
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(item)) => {
                // Put it back at the front — order must be preserved,
                // this is a reliable byte stream, not just a best-effort
                // frame queue.
                if let Ok(chunk) = item {
                    pending.push_front(chunk);
                }
                break;
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return false,
        }
    }
    true
}

/// Builds and sends one ack reflecting the current reorder-buffer state —
/// the single send path for `recv_bc_loop_udp`'s periodic `ack_tick` (see
/// `ACK_INTERVAL`'s doc comment). `next_expected_packet_id == 0` (nothing
/// received yet this session) is a real, distinct wire state, not just the
/// first ordinary value the counter happens to pass through: `bairelay`
/// represents it with `packet_id`/`group_id` both `0xffffffff` rather than
/// the ordinary cumulative-ack encoding — this file previously always used
/// the ordinary encoding (which happens to also produce `packet_id =
/// 0xffffffff` via `wrapping_sub(1)`, but left `group_id` at its default
/// `0` instead of matching the reference's sentinel), a real, if minor,
/// wire-format mismatch worth fixing while touching this function anyway.
async fn send_ack(
    socket: &UdpSocket,
    peer: &PeerHandle,
    next_expected_packet_id: u32,
    out_of_order: &BTreeMap<u32, Vec<u8>>,
    received_bytes_per_sec: u32,
) -> crate::Result<()> {
    let ack = if next_expected_packet_id == 0 {
        UdpAck {
            connection_id: peer.remote_connection_id,
            group_id: 0xffffffff,
            packet_id: 0xffffffff,
            received_bytes_per_sec,
            payload: Vec::new(),
        }
    } else {
        UdpAck {
            connection_id: peer.remote_connection_id,
            group_id: 0,
            packet_id: next_expected_packet_id.wrapping_sub(1),
            received_bytes_per_sec,
            payload: build_ack_payload(next_expected_packet_id, out_of_order),
        }
    };
    socket.send_to(&write_bcudp(&BcUdp::Ack(ack)), peer.addr).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::bc::model::*;
    use crate::protocol::crypto::EncryptionProtocol;
    use crate::transport::discovery::PeerHandle;
    use std::sync::Arc;
    use tokio::net::UdpSocket;

    #[tokio::test]
    async fn two_connections_round_trip_a_bc_message() {
        let socket_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let socket_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_a = socket_a.local_addr().unwrap();
        let addr_b = socket_b.local_addr().unwrap();

        let mut conn_a = BcConnection::new(
            socket_a,
            PeerHandle { addr: addr_b, local_connection_id: 1, remote_connection_id: 2, nonce: None, is_direct: false },
        );
        let mut conn_b = BcConnection::new(
            socket_b,
            PeerHandle { addr: addr_a, local_connection_id: 2, remote_connection_id: 1, nonce: None, is_direct: false },
        );

        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: 1,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(b"<?xml version=\"1.0\"?><body/>".to_vec()),
            }),
        };

        conn_a.send_bc(&bc, &EncryptionProtocol::Unencrypted).await.unwrap();
        let received = conn_b.recv_bc(&EncryptionProtocol::Unencrypted).await.unwrap();
        assert_eq!(received, bc);
    }

    #[tokio::test]
    async fn a_large_message_is_fragmented_and_reassembled() {
        let socket_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let socket_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_a = socket_a.local_addr().unwrap();
        let addr_b = socket_b.local_addr().unwrap();

        let mut conn_a = BcConnection::new(
            socket_a,
            PeerHandle { addr: addr_b, local_connection_id: 1, remote_connection_id: 2, nonce: None, is_direct: false },
        );
        let mut conn_b = BcConnection::new(
            socket_b,
            PeerHandle { addr: addr_a, local_connection_id: 2, remote_connection_id: 1, nonce: None, is_direct: false },
        );

        let big_payload = vec![0xABu8; 5000]; // several times MAX_FRAGMENT_SIZE
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_VIDEO,
                channel_id: 0,
                stream_type: 0,
                msg_num: 2,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(big_payload),
            }),
        };

        conn_a.send_bc(&bc, &EncryptionProtocol::Unencrypted).await.unwrap();
        let received = conn_b.recv_bc(&EncryptionProtocol::Unencrypted).await.unwrap();
        assert_eq!(received, bc);
    }

    fn login_bc(msg_num: u16, payload: Vec<u8>) -> Bc {
        Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: Some(payload) }),
        }
    }

    /// Regression test for the consumer-driven-recv architecture gap found
    /// via the 2026-09-16 audit: before `udp_rx_task` existed, nothing read
    /// the socket in the gap between one `recv_bc()` call returning and the
    /// next one starting. Sends three messages back-to-back with no
    /// `recv_bc()` call in between, waits well past the point where the
    /// old design would have left them sitting unread in the OS socket
    /// buffer, then confirms all three are still delivered in order — proof
    /// the background task kept draining and reassembling independently of
    /// consumer pace.
    #[tokio::test]
    async fn udp_rx_task_keeps_receiving_while_the_consumer_is_not_calling_recv_bc() {
        let socket_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let socket_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_a = socket_a.local_addr().unwrap();
        let addr_b = socket_b.local_addr().unwrap();

        let mut conn_a = BcConnection::new(
            socket_a,
            PeerHandle { addr: addr_b, local_connection_id: 1, remote_connection_id: 2, nonce: None, is_direct: false },
        );
        let mut conn_b = BcConnection::new(
            socket_b,
            PeerHandle { addr: addr_a, local_connection_id: 2, remote_connection_id: 1, nonce: None, is_direct: false },
        );

        let messages: Vec<Bc> = (0..3u16).map(|i| login_bc(i, vec![b'a' + i as u8; 10])).collect();
        for bc in &messages {
            conn_a.send_bc(bc, &EncryptionProtocol::Unencrypted).await.unwrap();
        }

        // Give udp_rx_task plenty of time to receive and reassemble all
        // three — conn_b.recv_bc() is not called at all during this sleep.
        tokio::time::sleep(Duration::from_millis(200)).await;

        for expected in &messages {
            let received = conn_b.recv_bc(&EncryptionProtocol::Unencrypted).await.unwrap();
            assert_eq!(&received, expected);
        }
    }

    /// Regression test for the real bug found on real hardware 2026-09-16
    /// (see `udp_rx_task`'s doc comment): an earlier version of the RX
    /// task `.await`ed the channel send directly inside the `recv_from`
    /// arm, so once `RX_CHANNEL_CAPACITY` filled, `ack_tick` stopped
    /// firing entirely until the consumer caught up — read by the camera
    /// as a bad connection, causing it to throttle/resend and making the
    /// stall worse. Floods a connection with far more `Data` packets than
    /// `RX_CHANNEL_CAPACITY` without ever calling `recv_bc()` to drain
    /// them, then confirms acks keep arriving on the peer's raw socket at
    /// roughly `ACK_INTERVAL`'s cadence regardless.
    #[tokio::test]
    async fn ack_tick_keeps_firing_even_when_the_channel_to_the_consumer_is_full() {
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sender_addr = sender.local_addr().unwrap();
        let socket_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_b = socket_b.local_addr().unwrap();

        // conn_b's recv_bc() is deliberately never called — its rx channel
        // fills and stays full for the rest of this test.
        let _conn_b = BcConnection::new(
            socket_b,
            PeerHandle { addr: sender_addr, local_connection_id: 2, remote_connection_id: 1, nonce: None, is_direct: false },
        );

        // Flood well past RX_CHANNEL_CAPACITY (64) with raw Data packets —
        // real Bc framing doesn't matter here, nothing ever parses these.
        for i in 0..200u32 {
            let packet = UdpData { connection_id: 2, packet_id: i, payload: vec![b'x'; 5] };
            sender.send_to(&write_bcudp(&BcUdp::Data(packet)), addr_b).await.unwrap();
        }

        let mut buf = [0u8; 2048];
        let mut ack_count = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        while tokio::time::Instant::now() < deadline {
            let Ok(Ok((n, _))) =
                tokio::time::timeout(Duration::from_millis(50), sender.recv_from(&mut buf)).await
            else {
                continue;
            };
            if let Ok(Some((BcUdp::Ack(_), _))) = read_bcudp(&buf[..n]) {
                ack_count += 1;
            }
        }
        // At ACK_INTERVAL (31ms) over 300ms, a healthy tick fires roughly
        // 9-10 times; the old bug would have delivered zero once the
        // channel filled. 5 leaves headroom for scheduling jitter while
        // still failing hard if the tick stalled.
        assert!(ack_count >= 5, "expected several acks despite a full rx channel, got {ack_count}");
    }

    /// `disconnect()` sends `C2D_DISC` on the discovery channel — see its
    /// doc comment for the real-hardware symptom this fixes (the camera
    /// kept streaming after this app closed, because nothing ever told it
    /// to stop).
    #[tokio::test]
    async fn disconnect_sends_c2d_disc_to_the_peer() {
        let socket_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let peer_stub = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer_stub.local_addr().unwrap();

        let conn_a = BcConnection::new(
            socket_a,
            PeerHandle { addr: peer_addr, local_connection_id: 5, remote_connection_id: 9, nonce: None, is_direct: true },
        );

        conn_a.disconnect().await;

        let mut buf = [0u8; 2048];
        let (n, _from) =
            tokio::time::timeout(Duration::from_secs(1), peer_stub.recv_from(&mut buf)).await.unwrap().unwrap();
        let (BcUdp::Discovery(disc), _) = read_bcudp(&buf[..n]).unwrap().unwrap() else {
            panic!("expected a discovery packet");
        };
        let crate::protocol::bcudp::xml::UdpXml::C2dDisc(c2d_disc) = disc.payload else {
            panic!("expected C2D_DISC");
        };
        assert_eq!(c2d_disc.cid, 5);
        assert_eq!(c2d_disc.did, 9);
    }

    #[test]
    fn rate_meter_reports_bytes_per_second_of_the_last_window() {
        let mut m = RateMeter::new();
        assert_eq!(m.get_value(), 0, "reports 0 until the first window closes, like the official client");
        m.add(60_000);
        m.add(40_000);
        std::thread::sleep(Duration::from_millis(200));
        m.roll();
        // 100_000 bytes over ~0.2s is ~500_000 B/s; allow generous scheduler slack.
        assert!((300_000..=500_000).contains(&m.get_value()), "got {}", m.get_value());
        m.roll();
        assert_eq!(m.get_value(), 0, "an empty window reports zero");
    }

    #[test]
    fn ack_payload_marks_out_of_order_packets_received_and_gaps_missing() {
        // next_expected_packet_id is 6 (the gap); 7 and 9 arrived out of
        // order, 8 did not.
        let mut out_of_order = BTreeMap::new();
        out_of_order.insert(7u32, vec![]);
        out_of_order.insert(9u32, vec![]);
        assert_eq!(build_ack_payload(6, &out_of_order), vec![0, 1, 0, 1]);
    }

    #[test]
    fn ack_payload_is_empty_when_nothing_arrived_out_of_order() {
        assert_eq!(build_ack_payload(6, &BTreeMap::new()), Vec::<u8>::new());
    }

    #[test]
    fn ack_payload_is_capped_against_a_wild_far_ahead_packet_id() {
        let mut out_of_order = BTreeMap::new();
        out_of_order.insert(6u32, vec![]);
        out_of_order.insert(u32::MAX, vec![]); // absurdly far ahead
        assert_eq!(build_ack_payload(6, &out_of_order).len(), ACK_BITMAP_CAP as usize);
    }

    #[tokio::test]
    async fn two_tcp_connections_round_trip_a_bc_message() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut conn = BcConnection::from_tcp(stream);
            conn.recv_bc(&EncryptionProtocol::Unencrypted).await.unwrap()
        });

        let client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut client_conn = BcConnection::from_tcp(client_stream);

        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: 1,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(b"<?xml version=\"1.0\"?><body/>".to_vec()),
            }),
        };
        client_conn.send_bc(&bc, &EncryptionProtocol::Unencrypted).await.unwrap();

        let received = server.await.unwrap();
        assert_eq!(received, bc);
    }

    #[tokio::test]
    async fn a_large_tcp_message_is_read_incrementally_and_reassembled() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut conn = BcConnection::from_tcp(stream);
            conn.recv_bc(&EncryptionProtocol::Unencrypted).await.unwrap()
        });

        let client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut client_conn = BcConnection::from_tcp(client_stream);

        // Several times the TCP read buffer (2048 bytes in recv_bc_loop_tcp),
        // forcing multiple `stream.read()` calls through the reassembly loop
        // — the same value the UDP fragmentation test uses.
        let big_payload = vec![0xABu8; 5000];
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_VIDEO,
                channel_id: 0,
                stream_type: 0,
                msg_num: 2,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(big_payload),
            }),
        };
        client_conn.send_bc(&bc, &EncryptionProtocol::Unencrypted).await.unwrap();

        let received = server.await.unwrap();
        assert_eq!(received, bc);
    }

    #[tokio::test]
    async fn tcp_connection_closed_by_peer_surfaces_as_connection_lost() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream); // close without sending anything
        });

        let client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut client_conn = BcConnection::from_tcp(client_stream);

        server.await.unwrap();
        let result = client_conn.recv_bc(&EncryptionProtocol::Unencrypted).await;
        assert!(matches!(result, Err(Error::ConnectionLost)));
    }
}
