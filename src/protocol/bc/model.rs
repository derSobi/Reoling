/// Every Bc message on the wire starts with this magic number.
pub const MAGIC_HEADER: u32 = 0x0abcdef0;

pub const MSG_ID_LOGIN: u32 = 1;
pub const MSG_ID_LOGOUT: u32 = 2;
pub const MSG_ID_VIDEO: u32 = 3;
pub const MSG_ID_VIDEO_STOP: u32 = 4;
pub const MSG_ID_VERSION: u32 = 80;
/// What a channel supports (siren, spotlight, ...), asked per channel.
pub const MSG_ID_ABILITY_SUPPORT: u32 = 58;
/// On-screen-display settings of a channel; carries the channel's name.
pub const MSG_ID_OSD: u32 = 44;
/// Two-way audio: ask what the camera accepts, stop, configure, send audio.
pub const MSG_ID_TALK_ABILITY: u32 = 10;
pub const MSG_ID_TALK_STOP: u32 = 11;
pub const MSG_ID_TALK_CONFIG: u32 = 201;
pub const MSG_ID_TALK_DATA: u32 = 202;
/// Moves to (or saves, or removes) a PTZ preset.
pub const MSG_ID_PTZ_PRESET: u32 = 19;
/// Lists the PTZ presets saved on the camera.
pub const MSG_ID_GET_PTZ_PRESET: u32 = 190;
/// Zoom and focus positions of a camera: read (294) and set (295).
pub const MSG_ID_GET_ZOOM_FOCUS: u32 = 294;
pub const MSG_ID_SET_ZOOM_FOCUS: u32 = 295;
/// Pan/tilt movement of a camera.
pub const MSG_ID_PTZ: u32 = 18;
/// PTZ Guard / "Monitor Point": sets, saves or moves to the saved home
/// position (write) and reads its current state (read). See
/// `.plans/reolink-baichuan-calibration-monitor-point.md`.
pub const MSG_ID_PTZ_GUARD: u32 = 331;
pub const MSG_ID_GET_PTZ_GUARD: u32 = 332;
/// Re-calibrates the pan/tilt mechanism.
pub const MSG_ID_PTZ_CALIBRATE: u32 = 341;
/// Reads a named image file the device keeps (Monitor Point's own
/// thumbnail, named "guard"). The reply is a small metadata message
/// followed by one or more `binaryData=1` chunks, ended by a response code
/// other than 200 (201, empty body) rather than a size field.
pub const MSG_ID_IMAGE_FILE: u32 = 421;
/// Takes a fresh live picture ("Snap"); the answer is a metadata message and
/// then `binaryData=1` chunks (AES-encrypted per `encryptLen`), ended like
/// `MSG_ID_IMAGE_FILE`'s by a code other than 200.
pub const MSG_ID_SNAP: u32 = 109;
/// Writes a named image file to the device: an `imageFileInfo` message, then
/// a `binaryData=1` message under the same `msg_num` carrying the JPEG in
/// the clear.
pub const MSG_ID_IMAGE_UPLOAD: u32 = 420;
/// Per-channel support table (which camera has a siren, spotlight, ...).
pub const MSG_ID_SUPPORT: u32 = 199;
/// Plays the siren once. Seen in a capture of the official app.
pub const MSG_ID_PLAY_SIREN: u32 = 263;
/// Switches the spotlight (floodlight) on or off.
pub const MSG_ID_SPOTLIGHT: u32 = 288;
/// Empty request the official app sends after login; the answer describes
/// the encoder settings per stream.
pub const MSG_ID_STREAM_INFO: u32 = 146;
/// Empty request the official app sends after login; the device then starts
/// pushing its channel list and alarm events.
pub const MSG_ID_SUBSCRIBE: u32 = 192;
/// Pushed by NVRs / Home Hubs: the channel list. Not requestable.
pub const MSG_ID_CHANNEL_INFO: u32 = 145;

/// Opens a recorded file for playback, by the `Id` a file search returned,
/// right after a `MSG_ID_REPLAY_SEEK`. There is no XML reply. The device
/// answers under the same id: first a 32-byte info block (class 0x0000,
/// code 200, `binaryData=1`), then the recording as BcMedia in messages whose
/// class and response code together hold the playback's sequence number
/// (see [`replay_seq`]). Nothing marks the end of the file: the data stop.
pub const MSG_ID_REPLAY: u32 = 5;
/// Ends a playback started with `MSG_ID_REPLAY` (a pause is a close too).
/// Answered with an empty 200; data of the closed playback may still trail
/// in after that reply.
pub const MSG_ID_REPLAY_CLOSE: u32 = 7;
/// Recorded-file search over one channel, stream and time range: start (14)
/// returns a search handle, next (15) returns one page of files per call,
/// end (16) releases the handle.
pub const MSG_ID_FILE_SEARCH_START: u32 = 14;
pub const MSG_ID_FILE_SEARCH_NEXT: u32 = 15;
pub const MSG_ID_FILE_SEARCH_END: u32 = 16;
/// Sets where the next `MSG_ID_REPLAY` starts and the sequence number its
/// data will carry. Answered with an empty 200.
pub const MSG_ID_REPLAY_SEEK: u32 = 123;
/// The recording calendar: which days of a month have recordings, per
/// channel.
pub const MSG_ID_RECORD_CALENDAR: u32 = 142;
/// Downloads a recorded time range. The reply is XML with a size estimate,
/// then BcMedia chunks (class 0x0000, code 200), then a header-only
/// [`RESPONSE_CODE_TRANSFER_DONE`].
pub const MSG_ID_DOWNLOAD: u32 = 143;
/// Header-only request that closes a download once its data have ended.
pub const MSG_ID_DOWNLOAD_END: u32 = 144;
/// Event windows (motion, person, ... periods) of a channel's day: start
/// (272) returns a file handle, next (273) returns pages of windows until
/// one says it is the last, end (274) releases the handle. All three carry
/// an Extension with the channel.
pub const MSG_ID_EVENT_SEARCH_START: u32 = 272;
pub const MSG_ID_EVENT_SEARCH_NEXT: u32 = 273;
pub const MSG_ID_EVENT_SEARCH_END: u32 = 274;
/// One still picture of a recorded file, for thumbnails: an empty 200, the
/// 32-byte info block, one main-stream I-frame, then a header-only
/// [`RESPONSE_CODE_TRANSFER_DONE`].
pub const MSG_ID_COVER_PREVIEW: u32 = 298;
/// Response code of the header-only message that ends the data of a
/// download or a cover preview.
pub const RESPONSE_CODE_TRANSFER_DONE: u16 = 300;

/// Header classes that carry an extra 4-byte `payload_offset` word after the
/// fixed 20-byte header. All other classes (0x6514 legacy, 0x6614 the
/// encrypted reply to legacy login) are 20 bytes with no payload_offset,
/// except recorded-media data, whose class is part of a sequence number:
/// [`header_has_payload_offset`] is the full rule.
pub fn has_payload_offset(class: u16) -> bool {
    class == 0x6414 || class == 0x0000
}

/// The header classes with a fixed meaning and layout. A recorded-media
/// message whose class is one of these is framed by its class as usual.
/// Every header seen on the wire has one of these classes, except
/// recorded-media data carrying a sequence number ([`replay_seq`]).
pub fn is_fixed_class(class: u16) -> bool {
    matches!(class, 0x6414 | 0x0000 | 0x6514 | 0x6614 | 0xffff)
}

/// Message ids whose data messages may carry a sequence number in place of
/// class and response code. 5 and 143 are seen on the wire. 8 has not been
/// seen, but it is framed the same way so that such a message, should one
/// arrive, cannot desynchronise the stream; the class and range checks in
/// [`replay_seq`] keep every other message out.
const SEQ_FRAMED_MSG_IDS: [u32; 3] = [MSG_ID_REPLAY, 8, MSG_ID_DOWNLOAD];

/// The sequence numbers accepted as such: Unix seconds from 2020-09-13 to
/// 2039-09-18. A playback's sequence number is chosen by the client, from
/// the current time; anything outside this window is not one.
pub const REPLAY_SEQ_RANGE: std::ops::RangeInclusive<u32> = 1_600_000_000..=2_200_000_000;

/// The sequence number a recorded-media data message carries, or `None` for
/// every other message.
///
/// The data of a playback put the 32-bit sequence number from its
/// `MSG_ID_REPLAY_SEEK` into the header: the class word is its high half and
/// the response-code word its low half, i.e. `seq = (class << 16) | code`.
/// Such a header is 24 bytes (it has the payload-offset word). A message
/// counts only when its id is a recorded-media id, its class is none of the
/// fixed classes, and the number lies in [`REPLAY_SEQ_RANGE`]; live video
/// (msg 3) and all control traffic are never matched.
pub fn replay_seq(msg_id: u32, class: u16, response_code: u16) -> Option<u32> {
    if !SEQ_FRAMED_MSG_IDS.contains(&msg_id) || is_fixed_class(class) {
        return None;
    }
    let seq = (u32::from(class) << 16) | u32::from(response_code);
    REPLAY_SEQ_RANGE.contains(&seq).then_some(seq)
}

/// Whether `seq` can be used as a playback's sequence number: inside
/// [`REPLAY_SEQ_RANGE`], and with a high half that is not one of the fixed
/// header classes (data carrying it would otherwise be framed by that
/// class). Every Unix time from 2024-04-09 to 2039 qualifies.
pub fn is_usable_replay_seq(seq: u32) -> bool {
    REPLAY_SEQ_RANGE.contains(&seq) && !is_fixed_class((seq >> 16) as u16)
}

/// Whether a header with these fields has the 4-byte payload-offset word
/// (24 bytes in all) rather than 20 bytes: the fixed classes that have it,
/// plus recorded-media data carrying a sequence number. Used for reading and
/// writing alike, so the two can never disagree.
pub fn header_has_payload_offset(msg_id: u32, class: u16, response_code: u16) -> bool {
    has_payload_offset(class) || replay_seq(msg_id, class, response_code).is_some()
}

/// Fixed part of every Bc header (20 bytes), plus the optional payload_offset
/// word (4 more bytes) present when `header_has_payload_offset` is true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BcHeader {
    pub msg_id: u32,
    pub body_len: u32,
    pub channel_id: u8,
    pub stream_type: u8,
    pub msg_num: u16,
    pub response_code: u16,
    pub class: u16,
    pub payload_offset: Option<u32>,
}

impl BcHeader {
    /// The playback sequence number this header carries, if it is a
    /// recorded-media data header (see [`replay_seq`]).
    pub fn replay_seq(&self) -> Option<u32> {
        replay_seq(self.msg_id, self.class, self.response_code)
    }
}

/// The parts of a header that describe the *meaning* of a message, as
/// opposed to `body_len`/`payload_offset` which describe its on-wire shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BcMeta {
    pub msg_id: u32,
    pub channel_id: u8,
    pub stream_type: u8,
    pub msg_num: u16,
    pub response_code: u16,
    pub class: u16,
}

impl BcMeta {
    /// The playback sequence number this message carries, if it is a
    /// recorded-media data message (see [`replay_seq`]). `None` for the
    /// 32-byte info block that opens a playback, which comes as class
    /// 0x0000 with code 200.
    pub fn replay_seq(&self) -> Option<u32> {
        replay_seq(self.msg_id, self.class, self.response_code)
    }
}

/// A parsed extension XML block (Task 7) precedes the main payload when
/// `payload_offset` is non-zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModernMsg {
    pub extension_xml: Option<Vec<u8>>,
    pub payload: Option<Vec<u8>>,
}

/// Legacy bodies are only used for the initial login upgrade handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyMsg {
    /// Header-only message asking the camera to upgrade to modern login.
    LoginUpgrade,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BcBody {
    Legacy(LegacyMsg),
    Modern(ModernMsg),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bc {
    pub meta: BcMeta,
    pub body: BcBody,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_payload_offset_matches_known_classes() {
        assert!(!has_payload_offset(0x6514)); // legacy login request
        assert!(!has_payload_offset(0x6614)); // encrypted reply to legacy login
        assert!(has_payload_offset(0x6414)); // modern login / most commands
        assert!(has_payload_offset(0x0000)); // most modern messages
    }

    #[test]
    fn replay_seq_is_class_high_and_code_low() {
        // Seen on the wire: class 0x6ad5, code 0x113d on every data message
        // of the playback whose seek used seq 1792348477.
        assert_eq!(replay_seq(MSG_ID_REPLAY, 0x6ad5, 0x113d), Some(1_792_348_477));
        assert_eq!(replay_seq(MSG_ID_DOWNLOAD, 0x6ad5, 0x113d), Some(1_792_348_477));
        assert_eq!(replay_seq(8, 0x6ad5, 0x113d), Some(1_792_348_477));
        assert!(header_has_payload_offset(MSG_ID_REPLAY, 0x6ad5, 0x113d));
    }

    #[test]
    fn replay_seq_never_matches_live_or_fixed_class_traffic() {
        // Live video and every other id keep the class rule.
        assert_eq!(replay_seq(MSG_ID_VIDEO, 0x6ad5, 0x113d), None);
        assert!(!header_has_payload_offset(MSG_ID_VIDEO, 0x6ad5, 0x113d));
        assert_eq!(replay_seq(MSG_ID_LOGIN, 0x6ad5, 0x113d), None);
        // The info block that opens a playback, and requests, are fixed
        // classes, although 0x6414/0x6514/0x6614 lie inside the seq window.
        for class in [0x0000, 0x6414, 0x6514, 0x6614, 0xffff] {
            assert_eq!(replay_seq(MSG_ID_REPLAY, class, 200), None);
        }
        assert!(!header_has_payload_offset(MSG_ID_REPLAY, 0x6514, 0));
        assert!(!header_has_payload_offset(MSG_ID_REPLAY, 0xffff, 0xffff));
        // Outside the plausible window: framed by class as before.
        assert_eq!(replay_seq(MSG_ID_REPLAY, 0x1234, 0x5678), None);
        assert_eq!(replay_seq(MSG_ID_REPLAY, 0x9000, 0), None);
        assert!(!header_has_payload_offset(MSG_ID_REPLAY, 0x1234, 0x5678));
    }

    #[test]
    fn usable_seq_avoids_the_fixed_classes() {
        assert!(is_usable_replay_seq(1_792_348_477));
        assert!(!is_usable_replay_seq(0x6414_0001));
        assert!(!is_usable_replay_seq(0x6614_ffff));
        assert!(!is_usable_replay_seq(1_000));
        // Whatever is usable is recognised on the way back in.
        let seq = 1_792_348_477u32;
        assert_eq!(replay_seq(MSG_ID_REPLAY, (seq >> 16) as u16, seq as u16), Some(seq));
    }

    #[test]
    fn header_and_meta_expose_the_seq() {
        let header = BcHeader {
            msg_id: MSG_ID_REPLAY,
            body_len: 0,
            channel_id: 0,
            stream_type: 0,
            msg_num: 0,
            response_code: 0x113d,
            class: 0x6ad5,
            payload_offset: Some(0),
        };
        assert_eq!(header.replay_seq(), Some(1_792_348_477));
        let meta = BcMeta {
            msg_id: MSG_ID_REPLAY,
            channel_id: 0,
            stream_type: 0,
            msg_num: 0,
            response_code: 200,
            class: 0x0000,
        };
        assert_eq!(meta.replay_seq(), None);
    }
}

/// A channel's encoder settings: read (56) and written (57) as `Compression`.
pub const MSG_ID_GET_COMPRESSION: u32 = 56;
pub const MSG_ID_SET_COMPRESSION: u32 = 57;
/// The encoder values one channel allows (a `StreamInfoList`), for channels
/// the login reply's list doesn't cover. A Home Hub answers it; an NVR,
/// whose login list covers every channel, refuses it (405).
pub const MSG_ID_CHANNEL_ENCODE_TABLE: u32 = 730;
/// The version info of the camera on one channel of an NVR or Home Hub:
/// its name, model (`type`) and firmware.
pub const MSG_ID_CHANNEL_VERSION: u32 = 318;
