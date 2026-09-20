use crate::protocol::bcudp::crc::calc_crc;
use crate::protocol::bcudp::model::*;
use crate::protocol::bcudp::xml_crypto;
use crate::protocol::Error;

pub fn write_bcudp(msg: &BcUdp) -> Vec<u8> {
    match msg {
        BcUdp::Discovery(disc) => {
            let plain_xml = disc.payload.to_bytes();
            let encrypted = xml_crypto::encrypt(disc.tid, &plain_xml);
            let checksum = calc_crc(&encrypted);
            let mut out = Vec::with_capacity(20 + encrypted.len());
            out.extend_from_slice(&MAGIC_HEADER_UDP_NEGO.to_le_bytes());
            out.extend_from_slice(&(encrypted.len() as u32).to_le_bytes());
            out.extend_from_slice(&1u32.to_le_bytes());
            out.extend_from_slice(&disc.tid.to_le_bytes());
            out.extend_from_slice(&checksum.to_le_bytes());
            out.extend_from_slice(&encrypted);
            out
        }
        BcUdp::Ack(ack) => {
            let mut out = Vec::with_capacity(28 + ack.payload.len());
            out.extend_from_slice(&MAGIC_HEADER_UDP_ACK.to_le_bytes());
            out.extend_from_slice(&ack.connection_id.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&ack.group_id.to_le_bytes());
            out.extend_from_slice(&ack.packet_id.to_le_bytes());
            out.extend_from_slice(&ack.received_bytes_per_sec.to_le_bytes());
            out.extend_from_slice(&(ack.payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&ack.payload);
            out
        }
        BcUdp::Data(data) => {
            let mut out = Vec::with_capacity(20 + data.payload.len());
            out.extend_from_slice(&MAGIC_HEADER_UDP_DATA.to_le_bytes());
            out.extend_from_slice(&data.connection_id.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&data.packet_id.to_le_bytes());
            out.extend_from_slice(&(data.payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&data.payload);
            out
        }
    }
}

pub fn read_bcudp(buf: &[u8]) -> crate::protocol::Result<Option<(BcUdp, usize)>> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    match magic {
        MAGIC_HEADER_UDP_NEGO => {
            if buf.len() < 20 {
                return Ok(None);
            }
            let payload_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
            let tid = u32::from_le_bytes(buf[12..16].try_into().unwrap());
            let total = 20 + payload_len;
            if buf.len() < total {
                return Ok(None);
            }
            let encrypted = &buf[20..total];
            let plain_xml = xml_crypto::decrypt(tid, encrypted);
            let payload = crate::protocol::bcudp::xml::UdpXml::from_bytes(&plain_xml)?;
            Ok(Some((BcUdp::Discovery(UdpDiscovery { tid, payload }), total)))
        }
        MAGIC_HEADER_UDP_ACK => {
            if buf.len() < 28 {
                return Ok(None);
            }
            let connection_id = i32::from_le_bytes(buf[4..8].try_into().unwrap());
            let group_id = u32::from_le_bytes(buf[12..16].try_into().unwrap());
            let packet_id = u32::from_le_bytes(buf[16..20].try_into().unwrap());
            let received_bytes_per_sec = u32::from_le_bytes(buf[20..24].try_into().unwrap());
            let payload_len = u32::from_le_bytes(buf[24..28].try_into().unwrap()) as usize;
            let total = 28 + payload_len;
            if buf.len() < total {
                return Ok(None);
            }
            let payload = buf[28..total].to_vec();
            Ok(Some((
                BcUdp::Ack(UdpAck {
                    connection_id,
                    group_id,
                    packet_id,
                    received_bytes_per_sec,
                    payload,
                }),
                total,
            )))
        }
        MAGIC_HEADER_UDP_DATA => {
            if buf.len() < 20 {
                return Ok(None);
            }
            let connection_id = i32::from_le_bytes(buf[4..8].try_into().unwrap());
            let packet_id = u32::from_le_bytes(buf[12..16].try_into().unwrap());
            let payload_len = u32::from_le_bytes(buf[16..20].try_into().unwrap()) as usize;
            let total = 20 + payload_len;
            if buf.len() < total {
                return Ok(None);
            }
            let payload = buf[20..total].to_vec();
            Ok(Some((
                BcUdp::Data(UdpData {
                    connection_id,
                    packet_id,
                    payload,
                }),
                total,
            )))
        }
        other => Err(Error::ProtocolError(format!(
            "unknown BcUdp magic: {other:#x}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::bcudp::xml::*;

    #[test]
    fn round_trips_discovery() {
        let msg = BcUdp::Discovery(UdpDiscovery {
            tid: 42,
            payload: UdpXml::C2mQ(C2mQ {
                uid: "9527000EXAMPLE01".to_string(),
                ver: 3,
                os: "WIN".to_string(),
            }),
        });
        let bytes = write_bcudp(&msg);
        let (parsed, consumed) = read_bcudp(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(parsed, msg);
    }

    #[test]
    fn round_trips_ack() {
        let msg = BcUdp::Ack(UdpAck {
            connection_id: 7,
            group_id: 0,
            packet_id: 3,
            received_bytes_per_sec: 0,
            payload: vec![0, 1, 1],
        });
        let bytes = write_bcudp(&msg);
        let (parsed, consumed) = read_bcudp(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(parsed, msg);
    }

    #[test]
    fn round_trips_data() {
        let msg = BcUdp::Data(UdpData {
            connection_id: 7,
            packet_id: 12,
            payload: b"hello".to_vec(),
        });
        let bytes = write_bcudp(&msg);
        let (parsed, consumed) = read_bcudp(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(parsed, msg);
    }

    #[test]
    fn returns_none_on_truncated_data_packet() {
        let msg = BcUdp::Data(UdpData {
            connection_id: 1,
            packet_id: 1,
            payload: b"hello".to_vec(),
        });
        let bytes = write_bcudp(&msg);
        assert!(read_bcudp(&bytes[..bytes.len() - 1]).unwrap().is_none());
    }
}
