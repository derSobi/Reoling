use crate::protocol::time::CivilTime;
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

/// Length of the info block: the BcMedia info unit that starts a live
/// stream and, as the very first data, every playback, download and cover
/// preview.
pub const INFO_BLOCK_LEN: usize = 32;

/// What the 32-byte info block says.
///
/// Layout (little-endian): magic `1001` (or `2001`), the unit length (32),
/// width (u32 at 8), height (u32 at 12), two bytes whose meaning is not
/// known, then at 18..24 the recorded file's start and at 24..30 its end,
/// each as `year - 1900, month (1-based), day, hour, minute, second` in UTC,
/// and two more unknown bytes. Byte 17 is not the frame rate the data
/// arrive at, so it is not offered here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InfoBlock {
    pub width: u32,
    pub height: u32,
    /// Start of the recorded file in UTC, or `None` when the six bytes are
    /// not a valid date (live streams leave them zero).
    pub start_utc: Option<CivilTime>,
    /// End of the recorded file in UTC; `None` as for `start_utc`. The
    /// device stops sending at this point without any end marker.
    pub end_utc: Option<CivilTime>,
}

/// Reads the info block at the start of `buf` (at least
/// [`INFO_BLOCK_LEN`] bytes, starting with the info magic). `None` for
/// anything else. A playback's open succeeded when its first msg-5 reply
/// (class 0x0000, code 200) carries exactly such a 32-byte payload.
pub fn parse_info_block(buf: &[u8]) -> Option<InfoBlock> {
    if buf.len() < INFO_BLOCK_LEN {
        return None;
    }
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if magic != MAGIC_HEADER_BCMEDIA_INFO_V1 && magic != MAGIC_HEADER_BCMEDIA_INFO_V2 {
        return None;
    }
    let date = |b: &[u8]| {
        let t = CivilTime::new(1900 + u16::from(b[0]), b[1], b[2], b[3], b[4], b[5]);
        t.is_valid().then_some(t)
    };
    Some(InfoBlock {
        width: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
        height: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
        start_utc: date(&buf[18..24]),
        end_utc: date(&buf[24..30]),
    })
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

/// Where the next BcMedia unit could start after `buf` stopped making
/// sense at offset 0: the first later offset holding a known unit magic,
/// or, if there is none yet, how much can be dropped while keeping the last
/// three bytes (a magic may be split across chunks). Used when a stream
/// switch leaves the tail of the previous stream in front of the new one.
pub fn resync_offset(buf: &[u8]) -> usize {
    let known = |m: u32| {
        matches!(
            m,
            MAGIC_HEADER_BCMEDIA_INFO_V1
                | MAGIC_HEADER_BCMEDIA_INFO_V2
                | MAGIC_HEADER_BCMEDIA_IFRAME..=MAGIC_HEADER_BCMEDIA_IFRAME_LAST
                | MAGIC_HEADER_BCMEDIA_PFRAME..=MAGIC_HEADER_BCMEDIA_PFRAME_LAST
                | MAGIC_HEADER_BCMEDIA_AAC
        )
    };
    (1..buf.len().saturating_sub(3))
        .find(|&i| known(u32::from_le_bytes(buf[i..i + 4].try_into().unwrap())))
        .unwrap_or_else(|| buf.len().saturating_sub(3))
}

/// The device's time of the key frame unit at the start of `buf`: in
/// recorded footage, the extra header after the fixed 24 bytes starts with
/// the frame's time as a little-endian Unix time (UTC, whole seconds). Seen
/// with 8, 160 and 168 bytes of extra header on a Home Hub and an NVR, for
/// four camera models. `None` for anything but a key frame, or one whose
/// extra header is shorter than four bytes (or not complete in `buf`). The
/// value is not checked: the caller compares it with the file's span.
pub fn keyframe_unix_time(buf: &[u8]) -> Option<u32> {
    if buf.len() < 28 {
        return None;
    }
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if !(MAGIC_HEADER_BCMEDIA_IFRAME..=MAGIC_HEADER_BCMEDIA_IFRAME_LAST).contains(&magic) {
        return None;
    }
    let additional_header_size = u32::from_le_bytes(buf[12..16].try_into().unwrap());
    (additional_header_size >= 4).then(|| u32::from_le_bytes(buf[24..28].try_into().unwrap()))
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
    #[test]
    fn resync_skips_garbage_up_to_the_next_known_unit() {
        let mut buf = vec![0xd9, 0xba, 0x80, 0x5f, 1, 2, 3];
        buf.extend_from_slice(&super::MAGIC_HEADER_BCMEDIA_PFRAME.to_le_bytes());
        buf.extend_from_slice(&[0; 8]);
        assert_eq!(super::resync_offset(&buf), 7);
        // Nothing recognisable yet: keep the last three bytes for later.
        assert_eq!(super::resync_offset(&[9u8; 10]), 7);
        assert_eq!(super::resync_offset(&[9u8; 2]), 0);
    }

    use super::*;

    fn info_v1_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC_HEADER_BCMEDIA_INFO_V1.to_le_bytes());
        buf.extend_from_slice(&32u32.to_le_bytes()); // header_size, always 32
        buf.extend_from_slice(&width.to_le_bytes());
        buf.extend_from_slice(&height.to_le_bytes());
        buf.push(0); // unknown
        buf.push(30); // unknown (not the frame rate)
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
    fn info_block_gives_size_and_the_files_utc_span() {
        // Shape seen at the start of a playback: 640x360, file
        // 2026-10-17 06:21:07 to 07:21:05 UTC.
        let mut bytes = info_v1_bytes(640, 360);
        bytes[16] = 0x01;
        bytes[17] = 0x0a;
        bytes[18..24].copy_from_slice(&[0x7e, 0x0a, 0x11, 0x06, 0x15, 0x07]);
        bytes[24..30].copy_from_slice(&[0x7e, 0x0a, 0x11, 0x07, 0x15, 0x05]);
        let block = parse_info_block(&bytes).unwrap();
        assert_eq!((block.width, block.height), (640, 360));
        assert_eq!(block.start_utc, Some(CivilTime::new(2026, 10, 17, 6, 21, 7)));
        assert_eq!(block.end_utc, Some(CivilTime::new(2026, 10, 17, 7, 21, 5)));
        // `parse_one` still sees the same unit as plain Info.
        let (msg, used) = parse_one(&bytes).unwrap().unwrap();
        assert_eq!((msg, used), (BcMediaMessage::Info { width: 640, height: 360 }, INFO_BLOCK_LEN));
    }

    #[test]
    fn info_block_without_dates_or_with_another_magic() {
        let block = parse_info_block(&info_v1_bytes(1920, 1080)).unwrap();
        assert_eq!((block.start_utc, block.end_utc), (None, None));
        assert!(parse_info_block(&info_v1_bytes(1920, 1080)[..31]).is_none());
        let mut not_info = info_v1_bytes(1, 1);
        not_info[0..4].copy_from_slice(&MAGIC_HEADER_BCMEDIA_IFRAME.to_le_bytes());
        assert!(parse_info_block(&not_info).is_none());
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

    /// A key frame (or, with `key` false, a delta frame) with `extra` as its
    /// extra header.
    fn frame_with_extra_header(key: bool, extra: &[u8], data: &[u8]) -> Vec<u8> {
        let magic = if key { MAGIC_HEADER_BCMEDIA_IFRAME } else { MAGIC_HEADER_BCMEDIA_PFRAME };
        let mut buf = Vec::new();
        buf.extend_from_slice(&magic.to_le_bytes());
        buf.extend_from_slice(b"H264");
        buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(extra.len() as u32).to_le_bytes());
        buf.extend_from_slice(&1_968_952_017u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(extra);
        buf.extend_from_slice(data);
        buf.resize(buf.len() + pad_len(data.len()), 0);
        buf
    }

    #[test]
    fn a_key_frame_header_gives_the_devices_time_of_the_frame() {
        // 8 bytes (an NVR), as recorded: 2026-10-19 18:57:19 UTC.
        let nvr = [0x0f, 0x68, 0xd6, 0x6a, 0x2a, 0x01, 0xc3, 0x00];
        // 160 and 168 bytes (Home Hub cameras), 2026-10-19 17:18:10 UTC.
        let mut hub = vec![0xd2, 0x50, 0xd6, 0x6a, 0x2a, 0x01, 0xc3, 0x00, 0xff, 0x8f, 0x00, 0x01];
        for (extra, expected) in [
            (nvr.to_vec(), 1_792_436_239u32),
            ({ hub.resize(160, 0); hub.clone() }, 1_792_430_290),
            ({ hub.resize(168, 0); hub.clone() }, 1_792_430_290),
        ] {
            let unit = frame_with_extra_header(true, &extra, &[0, 0, 0, 1, 0x65]);
            assert_eq!(keyframe_unix_time(&unit), Some(expected), "{} bytes", extra.len());
            // `parse_one` is unchanged: the extra header is skipped.
            let (msg, used) = parse_one(&unit).unwrap().unwrap();
            assert_eq!(used, unit.len());
            let BcMediaMessage::Video(frame) = msg else { panic!("expected a video frame") };
            assert_eq!((frame.is_keyframe, frame.microseconds, frame.data.as_slice()), (true, 1_968_952_017, &[0, 0, 0, 1, 0x65][..]));
        }
        assert_eq!(
            CivilTime::from_epoch_seconds(1_792_436_239),
            CivilTime::new(2026, 10, 19, 18, 57, 19)
        );
    }

    #[test]
    fn no_time_without_a_key_frame_and_four_header_bytes() {
        let time = [0x0f, 0x68, 0xd6, 0x6a, 0x2a, 0x01, 0xc3, 0x00];
        assert_eq!(keyframe_unix_time(&frame_with_extra_header(false, &time, &[1])), None, "a delta frame");
        assert_eq!(keyframe_unix_time(&frame_with_extra_header(true, &[], &[1, 2, 3, 4])), None, "no extra header");
        assert_eq!(keyframe_unix_time(&frame_with_extra_header(true, &time[..3], &[1])), None, "three bytes");
        assert_eq!(keyframe_unix_time(&frame_with_extra_header(true, &time, &[1])[..27]), None, "cut short");
        assert_eq!(keyframe_unix_time(&info_v1_bytes(640, 360)), None);
        assert_eq!(keyframe_unix_time(&[]), None);
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
