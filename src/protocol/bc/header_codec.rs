use crate::protocol::bc::model::{header_has_payload_offset, BcHeader, MAGIC_HEADER};
use crate::protocol::Error;

/// Serializes a `BcHeader` to its 20 or 24 byte wire form (all fields
/// little-endian): magic, msg_id, body_len, channel_id, stream_type,
/// msg_num, response_code, class, [payload_offset].
pub fn write_header(header: &BcHeader) -> Vec<u8> {
    let mut buf = Vec::with_capacity(24);
    buf.extend_from_slice(&MAGIC_HEADER.to_le_bytes());
    buf.extend_from_slice(&header.msg_id.to_le_bytes());
    buf.extend_from_slice(&header.body_len.to_le_bytes());
    buf.push(header.channel_id);
    buf.push(header.stream_type);
    buf.extend_from_slice(&header.msg_num.to_le_bytes());
    buf.extend_from_slice(&header.response_code.to_le_bytes());
    buf.extend_from_slice(&header.class.to_le_bytes());
    if let Some(offset) = header.payload_offset {
        buf.extend_from_slice(&offset.to_le_bytes());
    }
    buf
}

/// The number of bytes a message with this header (parsed as `header_len`
/// bytes) takes on the wire: header and body, plus 4 more after a class
/// 0xffff reply, whose device leaves 4 zero bytes that its `body_len` does
/// not count (see `read_bc`).
pub fn wire_len(header: &BcHeader, header_len: usize) -> usize {
    let total = header_len + header.body_len as usize;
    if header.class == 0xffff {
        total + 4
    } else {
        total
    }
}

/// Parses a `BcHeader` from the start of `buf`. Returns `Ok(None)` if there
/// are not yet enough bytes (caller should read more and retry) rather than
/// an error, since this is meant to be called on a growing network buffer.
///
/// The header is 24 bytes (with the payload-offset word) for classes 0x6414
/// and 0x0000, and for recorded-media data, whose class and response code
/// hold a playback sequence number instead (see
/// [`crate::protocol::bc::model::replay_seq`]); 20 bytes otherwise.
pub fn read_header(buf: &[u8]) -> crate::protocol::Result<Option<(BcHeader, usize)>> {
    if buf.len() < 20 {
        return Ok(None);
    }
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if magic != MAGIC_HEADER {
        return Err(Error::ProtocolError(format!(
            "bad magic header: {magic:#x}"
        )));
    }
    let msg_id = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    let body_len = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    let channel_id = buf[12];
    let stream_type = buf[13];
    let msg_num = u16::from_le_bytes(buf[14..16].try_into().unwrap());
    let response_code = u16::from_le_bytes(buf[16..18].try_into().unwrap());
    let class = u16::from_le_bytes(buf[18..20].try_into().unwrap());

    if header_has_payload_offset(msg_id, class, response_code) {
        if buf.len() < 24 {
            return Ok(None);
        }
        let payload_offset = u32::from_le_bytes(buf[20..24].try_into().unwrap());
        Ok(Some((
            BcHeader {
                msg_id,
                body_len,
                channel_id,
                stream_type,
                msg_num,
                response_code,
                class,
                payload_offset: Some(payload_offset),
            },
            24,
        )))
    } else {
        Ok(Some((
            BcHeader {
                msg_id,
                body_len,
                channel_id,
                stream_type,
                msg_num,
                response_code,
                class,
                payload_offset: None,
            },
            20,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::bc::model::BcHeader;

    #[test]
    fn round_trips_a_modern_header_with_payload_offset() {
        let header = BcHeader {
            msg_id: 1,
            body_len: 42,
            channel_id: 0,
            stream_type: 0,
            msg_num: 7,
            response_code: 0,
            class: 0x6414,
            payload_offset: Some(0),
        };
        let bytes = write_header(&header);
        assert_eq!(bytes.len(), 24);
        let (parsed, consumed) = read_header(&bytes).unwrap().unwrap();
        assert_eq!(consumed, 24);
        assert_eq!(parsed, header);
    }

    #[test]
    fn round_trips_a_legacy_header_without_payload_offset() {
        let header = BcHeader {
            msg_id: 1,
            body_len: 0,
            channel_id: 0,
            stream_type: 0,
            msg_num: 1,
            response_code: 0xdc12,
            class: 0x6514,
            payload_offset: None,
        };
        let bytes = write_header(&header);
        assert_eq!(bytes.len(), 20);
        let (parsed, consumed) = read_header(&bytes).unwrap().unwrap();
        assert_eq!(consumed, 20);
        assert_eq!(parsed, header);
    }

    #[test]
    fn returns_none_when_buffer_too_short() {
        let short = [0u8; 10];
        assert!(read_header(&short).unwrap().is_none());
    }
}
