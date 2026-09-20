use crate::protocol::Error;

const MAGIC_HEADER_BCMEDIA_INFO_V1: u32 = 0x31303031;
const MAGIC_HEADER_BCMEDIA_INFO_V2: u32 = 0x32303031;
const MAGIC_HEADER_BCMEDIA_IFRAME: u32 = 0x63643030;
const MAGIC_HEADER_BCMEDIA_IFRAME_LAST: u32 = 0x63643039;
const MAGIC_HEADER_BCMEDIA_PFRAME: u32 = 0x63643130;
const MAGIC_HEADER_BCMEDIA_PFRAME_LAST: u32 = 0x63643139;
const MAGIC_HEADER_BCMEDIA_AAC: u32 = 0x62773530;

/// Media packets are padded to a multiple of this many bytes.
const PAD_SIZE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoType {
    H264,
    H265,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoFrame {
    pub video_type: VideoType,
    pub is_keyframe: bool,
    pub microseconds: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BcMediaMessage {
    Info { width: u32, height: u32 },
    Video(VideoFrame),
    /// One AAC audio frame (ADTS).
    Audio(Vec<u8>),
    /// A recognized unit we don't use. Fully consumed from the buffer even
    /// though we discard its contents.
    Skipped,
}

fn pad_len(payload_len: usize) -> usize {
    (PAD_SIZE - payload_len % PAD_SIZE) % PAD_SIZE
}

fn video_type_from_tag(tag: &[u8]) -> crate::protocol::Result<VideoType> {
    match tag {
        b"H264" => Ok(VideoType::H264),
        b"H265" => Ok(VideoType::H265),
        other => Err(Error::ProtocolError(format!(
            "unrecognised video type tag: {other:?}"
        ))),
    }
}

/// Parses exactly one BcMedia unit from the start of `buf`. Returns
/// `Ok(None)` if `buf` doesn't yet contain a complete unit.
pub fn parse_one(buf: &[u8]) -> crate::protocol::Result<Option<(BcMediaMessage, usize)>> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());

    match magic {
        MAGIC_HEADER_BCMEDIA_INFO_V1 | MAGIC_HEADER_BCMEDIA_INFO_V2 => {
            // The header_size field (checked implicitly by fixed offsets
            // below) is the total unit length including the 4-byte magic.
            const LEN: usize = 32;
            if buf.len() < LEN {
                return Ok(None);
            }
            let width = u32::from_le_bytes(buf[8..12].try_into().unwrap());
            let height = u32::from_le_bytes(buf[12..16].try_into().unwrap());
            Ok(Some((BcMediaMessage::Info { width, height }, LEN)))
        }
        MAGIC_HEADER_BCMEDIA_IFRAME..=MAGIC_HEADER_BCMEDIA_IFRAME_LAST
        | MAGIC_HEADER_BCMEDIA_PFRAME..=MAGIC_HEADER_BCMEDIA_PFRAME_LAST => {
            let is_keyframe = magic <= MAGIC_HEADER_BCMEDIA_IFRAME_LAST;
            // Fixed part: magic(4) + tag(4) + payload_size(4) + additional_header_size(4)
            //           + microseconds(4) + unknown(4) = 24 bytes.
            if buf.len() < 24 {
                return Ok(None);
            }
            let video_type = video_type_from_tag(&buf[4..8])?;
            let payload_size = u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize;
            let additional_header_size =
                u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
            let microseconds = u32::from_le_bytes(buf[16..20].try_into().unwrap());

            let data_start = 24 + additional_header_size;
            let pad = pad_len(payload_size);
            let total = data_start + payload_size + pad;
            if buf.len() < total {
                return Ok(None);
            }
            let data = buf[data_start..data_start + payload_size].to_vec();
            Ok(Some((
                BcMediaMessage::Video(VideoFrame {
                    video_type,
                    is_keyframe,
                    microseconds,
                    data,
                }),
                total,
            )))
        }
        MAGIC_HEADER_BCMEDIA_AAC => {
            // magic(4) + payload_size(2) + duplicate_payload_size(2) = 8 bytes.
            if buf.len() < 8 {
                return Ok(None);
            }
            let payload_size = u16::from_le_bytes(buf[4..6].try_into().unwrap()) as usize;
            let pad = pad_len(payload_size);
            let total = 8 + payload_size + pad;
            if buf.len() < total {
                return Ok(None);
            }
            let audio = buf[8..8 + payload_size].to_vec();
            Ok(Some((BcMediaMessage::Audio(audio), total)))
        }
        other => Err(Error::ProtocolError(format!(
            "unsupported bcmedia magic: {other:#x} (only Info/Iframe/Pframe/Aac are implemented)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info_v1_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC_HEADER_BCMEDIA_INFO_V1.to_le_bytes());
        buf.extend_from_slice(&32u32.to_le_bytes()); // header_size, always 32
        buf.extend_from_slice(&width.to_le_bytes());
        buf.extend_from_slice(&height.to_le_bytes());
        buf.push(0); // unknown
        buf.push(30); // fps
        buf.extend_from_slice(&[0u8; 12]); // start/end date-time fields
        buf.extend_from_slice(&0u16.to_le_bytes()); // trailing unknown
        buf
    }

    fn iframe_bytes(data: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC_HEADER_BCMEDIA_IFRAME.to_le_bytes());
        buf.extend_from_slice(b"H264");
        buf.extend_from_slice(&(data.len() as u32).to_le_bytes()); // payload_size
        buf.extend_from_slice(&0u32.to_le_bytes()); // additional_header_size
        buf.extend_from_slice(&123u32.to_le_bytes()); // microseconds
        buf.extend_from_slice(&0u32.to_le_bytes()); // unknown
        buf.extend_from_slice(data);
        let pad = (8 - data.len() % 8) % 8;
        buf.extend(std::iter::repeat(0u8).take(pad));
        buf
    }

    #[test]
    fn parses_info_v1() {
        let bytes = info_v1_bytes(1920, 1080);
        let (msg, consumed) = parse_one(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(msg, BcMediaMessage::Info { width: 1920, height: 1080 });
    }

    #[test]
    fn parses_an_h264_iframe() {
        let data = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42]; // fake NAL-ish bytes
        let bytes = iframe_bytes(&data);
        let (msg, consumed) = parse_one(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        let BcMediaMessage::Video(frame) = msg else { panic!("expected a video frame") };
        assert_eq!(frame.video_type, VideoType::H264);
        assert!(frame.is_keyframe);
        assert_eq!(frame.microseconds, 123);
        assert_eq!(frame.data, data);
    }

    #[test]
    fn returns_none_when_more_bytes_are_needed() {
        let bytes = iframe_bytes(&[1, 2, 3]);
        assert!(parse_one(&bytes[..bytes.len() - 1]).unwrap().is_none());
    }

    #[test]
    fn aac_unit_yields_its_payload_and_consumes_the_padding() {
        let payload = [0xFFu8, 0xF1, 0x50, 0x80, 0x01, 0x02, 0x03];
        let mut unit = Vec::new();
        unit.extend_from_slice(&MAGIC_HEADER_BCMEDIA_AAC.to_le_bytes());
        unit.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        unit.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        unit.extend_from_slice(&payload);
        unit.resize(unit.len() + pad_len(payload.len()), 0);
        let (msg, used) = parse_one(&unit).unwrap().unwrap();
        assert_eq!(msg, BcMediaMessage::Audio(payload.to_vec()));
        assert_eq!(used, unit.len());
    }

    #[test]
    fn rejects_unrecognised_magic() {
        let bytes = 0x11111111u32.to_le_bytes().to_vec();
        assert!(parse_one(&bytes).is_err());
    }
}
