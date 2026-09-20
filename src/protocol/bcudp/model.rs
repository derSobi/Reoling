pub const MAGIC_HEADER_UDP_NEGO: u32 = 0x2a87cf3a;
pub const MAGIC_HEADER_UDP_ACK: u32 = 0x2a87cf20;
pub const MAGIC_HEADER_UDP_DATA: u32 = 0x2a87cf10;

/// A negotiation packet, exchanged with relays/devices during UID resolution
/// and connection setup. `tid` doubles as the XML encryption offset.
#[derive(Debug, Clone, PartialEq)]
pub struct UdpDiscovery {
    pub tid: u32,
    pub payload: crate::protocol::bcudp::xml::UdpXml,
}

/// Acknowledges receipt of data packets up to and including `packet_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpAck {
    pub connection_id: i32,
    pub group_id: u32,
    pub packet_id: u32,
    /// **Payload bytes per second the sender of this ack received** over
    /// the previous whole second, refreshed once a second — not a latency.
    /// Confirmed 2026-09-20 from a real capture of the official client
    /// (field == measured `UdpData` payload rate, byte-exact), and the
    /// camera's send rate depends on it: reporting ~22000 (an inter-ack gap)
    /// or 0 kept it at ~6.3Mbit/s of an ~8.3Mbit/s stream; reporting the
    /// real rate gives the full stream. Historically named `maybe_latency`
    /// after `bairelay`'s guess.
    pub received_bytes_per_sec: u32,
    pub payload: Vec<u8>,
}

/// One (possibly fragmented) chunk of a Bc message sent over the negotiated
/// UDP connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpData {
    pub connection_id: i32,
    pub packet_id: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BcUdp {
    Discovery(UdpDiscovery),
    Ack(UdpAck),
    Data(UdpData),
}
