/// Every Bc message on the wire starts with this magic number.
pub const MAGIC_HEADER: u32 = 0x0abcdef0;

pub const MSG_ID_LOGIN: u32 = 1;
pub const MSG_ID_LOGOUT: u32 = 2;
pub const MSG_ID_VIDEO: u32 = 3;
pub const MSG_ID_VIDEO_STOP: u32 = 4;

/// Header classes that carry an extra 4-byte `payload_offset` word after the
/// fixed 20-byte header. All other classes (0x6514 legacy, 0x6614 the
/// encrypted reply to legacy login) are 20 bytes with no payload_offset.
pub fn has_payload_offset(class: u16) -> bool {
    class == 0x6414 || class == 0x0000
}

/// Fixed part of every Bc header (20 bytes), plus the optional payload_offset
/// word (4 more bytes) present when `has_payload_offset(class)` is true.
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
}
