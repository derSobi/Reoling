use crate::protocol::time::CivilTime;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const XML_VERSION: &str = "1.1";

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename = "body")]
pub struct BcXml {
    #[serde(rename = "Encryption", skip_serializing_if = "Option::is_none")]
    pub encryption: Option<Encryption>,
    #[serde(rename = "LoginUser", skip_serializing_if = "Option::is_none")]
    pub login_user: Option<LoginUser>,
    #[serde(rename = "LoginNet", skip_serializing_if = "Option::is_none")]
    pub login_net: Option<LoginNet>,
    #[serde(rename = "DeviceInfo", skip_serializing_if = "Option::is_none")]
    pub device_info: Option<DeviceInfo>,
    #[serde(rename = "Preview", skip_serializing_if = "Option::is_none")]
    pub preview: Option<Preview>,
    #[serde(rename = "VersionInfo", skip_serializing_if = "Option::is_none")]
    pub version_info: Option<VersionInfo>,
    #[serde(rename = "ChannelInfoList", skip_serializing_if = "Option::is_none")]
    pub channel_info_list: Option<ChannelInfoList>,
    #[serde(rename = "OsdChannelName", skip_serializing_if = "Option::is_none")]
    pub osd_channel_name: Option<OsdChannelName>,
    #[serde(rename = "Support", skip_serializing_if = "Option::is_none")]
    pub support: Option<Support>,
    #[serde(rename = "PtzZoomFocus", skip_serializing_if = "Option::is_none")]
    pub ptz_zoom_focus: Option<PtzZoomFocus>,
    #[serde(rename = "PtzPreset", skip_serializing_if = "Option::is_none")]
    pub ptz_preset: Option<PtzPresetXml>,
    #[serde(rename = "PtzGuard", skip_serializing_if = "Option::is_none")]
    pub ptz_guard: Option<PtzGuardXml>,
    /// Reply to `MSG_ID_RECORD_CALENDAR`.
    #[serde(rename = "DayRecords", skip_serializing_if = "Option::is_none")]
    pub day_records: Option<DayRecords>,
    /// Replies to the file search (14, 15) and to a download (143).
    #[serde(rename = "FileInfoList", skip_serializing_if = "Option::is_none")]
    pub file_info_list: Option<FileInfoList>,
    #[serde(rename = "ReplaySeek", skip_serializing_if = "Option::is_none")]
    pub replay_seek: Option<ReplaySeek>,
    /// Reply to `MSG_ID_EVENT_SEARCH_START` (the file handle).
    #[serde(rename = "findAlarmVideo", skip_serializing_if = "Option::is_none")]
    pub find_alarm_video: Option<FindAlarmVideo>,
    /// Reply to `MSG_ID_EVENT_SEARCH_NEXT` (one page of event windows).
    #[serde(rename = "alarmVideoInfo", skip_serializing_if = "Option::is_none")]
    pub alarm_video_info: Option<AlarmVideoInfo>,
    #[serde(rename = "CoverPreview", skip_serializing_if = "Option::is_none")]
    pub cover_preview: Option<CoverPreview>,
}

/// "Monitor Point" in the app's UI — PTZ Guard / Guard Point on the wire: a
/// saved home position the camera can be sent to, with an optional
/// automatic-return timer. Read via `MSG_ID_GET_PTZ_GUARD` (332). See
/// `.plans/reolink-baichuan-calibration-monitor-point.md`.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct PtzGuardXml {
    #[serde(rename = "channelId")]
    pub channel_id: Option<u8>,
    /// Automatic return enabled.
    pub benable: Option<u8>,
    /// Whether a point is actually saved. Different firmwares use different
    /// field names for this — both are read.
    pub bvalid: Option<u8>,
    #[serde(rename = "bexistPos")]
    pub bexist_pos: Option<u8>,
    /// Seconds before automatic return (10..=300 in the official app).
    pub timeout: Option<u32>,
    pub mode: Option<String>,
    #[serde(rename = "imageName")]
    pub image_name: Option<String>,
}

impl PtzGuardXml {
    pub fn into_monitor_point(self) -> crate::client::MonitorPoint {
        crate::client::MonitorPoint {
            enabled: self.benable.unwrap_or(0) != 0,
            valid: self.bvalid.or(self.bexist_pos).unwrap_or(0) != 0,
            timeout_seconds: self.timeout.unwrap_or(0),
        }
    }
}

/// A camera's saved PTZ presets, and the write form of one preset command
/// (`setPos` saves the current position, `toPos` moves to it, `delPos`
/// removes it — `setPos`/`toPos` are documented in neolink; `delPos` is
/// this project's best guess, matched against a capture of the official app:
/// one message per preset op with no retries, which fits `delPos` working in
/// one shot rather than the multi-attempt fallbacks other tools use for
/// deletion).
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct PtzPresetXml {
    #[serde(rename = "channelId")]
    pub channel_id: Option<u8>,
    #[serde(rename = "presetList")]
    pub preset_list: Option<PresetListXml>,
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct PresetListXml {
    #[serde(rename = "preset", default)]
    pub presets: Vec<PresetXml>,
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct PresetXml {
    pub id: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

impl PtzPresetXml {
    /// The saved presets: enabled and named, as the official app shows them
    /// (a preset with `enable=0` or an empty name is treated as deleted).
    pub fn into_presets(self) -> Vec<crate::client::PtzPreset> {
        self.preset_list
            .map(|l| l.presets)
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.enable.as_deref() != Some("0"))
            .filter_map(|p| {
                let name = p.name?;
                (!name.trim().is_empty()).then_some(crate::client::PtzPreset { id: p.id, name })
            })
            .collect()
    }
}

/// A camera's zoom and focus: the range of positions and where they are.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct PtzZoomFocus {
    #[serde(rename = "channelId")]
    pub channel_id: Option<u8>,
    pub zoom: Option<PositionRange>,
    pub focus: Option<PositionRange>,
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct PositionRange {
    #[serde(rename = "maxPos")]
    pub max_pos: Option<u32>,
    #[serde(rename = "minPos")]
    pub min_pos: Option<u32>,
    #[serde(rename = "curPos")]
    pub cur_pos: Option<u32>,
}

/// What the device supports; the per-channel part is what we read.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct Support {
    #[serde(rename = "item", default)]
    pub items: Vec<SupportItem>,
}

/// One channel's row of the support table (other `item`s, such as the smart
/// home entries, have no `chnID` and are ignored).
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct SupportItem {
    #[serde(rename = "chnID")]
    pub channel_id: Option<u8>,
    /// Non-zero when the camera has the siren.
    #[serde(rename = "audioVersion")]
    pub audio_version: Option<u32>,
    /// Bit mask of the camera's LEDs: bit 0 is the status LED, bits 1 and 2
    /// the floodlight (reolink_aio; matches a Home Hub with E1 Outdoor PoE
    /// and RLC-1224A cameras: 3 and 38 both have a spotlight, 0 has none).
    #[serde(rename = "ledCtrl")]
    pub led_ctrl: Option<u32>,
    #[serde(rename = "lightType")]
    pub light_type: Option<u32>,
    /// Non-zero when the camera can do two-way audio.
    #[serde(rename = "ipcAudioTalk")]
    pub ipc_audio_talk: Option<u32>,
    /// Non-zero for cameras with pan/tilt (and zoom) motors.
    #[serde(rename = "ptzType")]
    pub ptz_type: Option<u32>,
    /// Bit mask of PTZ features (bit 2 Monitor Point / Guard, bit 3
    /// calibration, bit 4 digital zoom, bit 6 speed control — reolink_aio;
    /// see `.plans/reolink-baichuan-calibration-monitor-point.md`).
    #[serde(rename = "ptzControl")]
    pub ptz_control: Option<u32>,
    /// The picture pipeline's generation; 219075 goes with the cameras
    /// that offer a binning mode.
    #[serde(rename = "newIspCfg")]
    pub new_isp_cfg: Option<u32>,
    /// 0 for cameras that call the third detection type "Pet", 1 "Animal".
    #[serde(rename = "aiAnimalType")]
    pub ai_animal_type: Option<u32>,
}

impl Support {
    /// The channels' abilities.
    pub fn into_abilities(self) -> Vec<(u8, crate::client::ChannelAbilities)> {
        self.items
            .into_iter()
            .filter_map(|i| {
                let channel = i.channel_id?;
                let on = |v: Option<u32>| v.unwrap_or(0) > 0;
                // Which motors a pan/tilt(/zoom) camera has, by `ptzType`
                // (values as reolink_aio decodes them).
                let ptz = i.ptz_type.unwrap_or(0);
                let physical_ptz = ptz != 0;
                let ptz_control = i.ptz_control.unwrap_or(0);
                Some((
                    channel,
                    crate::client::ChannelAbilities {
                        siren: on(i.audio_version),
                        spotlight: i.light_type.unwrap_or(0) >= 2 || i.led_ctrl.unwrap_or(0) & 0b110 != 0,
                        talk: on(i.ipc_audio_talk),
                        pan: matches!(ptz, 2 | 3 | 5 | 6 | 7),
                        tilt: matches!(ptz, 2 | 3 | 5 | 6),
                        zoom: matches!(ptz, 1 | 2 | 5),
                        monitor_point: physical_ptz && ptz_control & (1 << 2) != 0,
                        calibration: physical_ptz && ptz_control & (1 << 3) != 0,
                        status_led: i.led_ctrl.unwrap_or(0) & 1 != 0,
                        tracking: physical_ptz,
                        binning: i.new_isp_cfg == Some(219075),
                        pet_label: i.ai_animal_type == Some(0),
                    },
                ))
            })
            .collect()
    }
}

/// The channel's on-screen name, from the reply to `MSG_ID_OSD` (the reply
/// also carries an `OsdDatetime` element, which we ignore).

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct OsdChannelName {
    pub name: Option<String>,
}

/// An NVR / Home Hub's description of its channels.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct ChannelInfoList {
    #[serde(rename = "ChannelInfo", default)]
    pub channels: Vec<ChannelInfoXml>,
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct ChannelInfoXml {
    #[serde(rename = "channelId")]
    pub channel_id: Option<u8>,
    #[serde(rename = "devName")]
    pub name: Option<String>,
    pub state: Option<String>,
    /// Comma separated, e.g. `mainStream,subStream,externStream`.
    #[serde(rename = "streamSupport")]
    pub stream_support: Option<String>,
    /// The camera's UID, which every recorded-video request names the
    /// camera by. Never logged (its `Debug` is masked).
    pub uid: Option<CameraUid>,
}

impl ChannelInfoList {
    /// The channels that exist, skipping the "none" placeholders an NVR
    /// sends for empty slots. A channel that does not list its streams is
    /// given main and sub, which every camera has.
    pub fn into_channels(self) -> Vec<crate::client::ChannelInfo> {
        use crate::client::{ChannelInfo, StreamProfile};
        self.channels
            .into_iter()
            .filter_map(|c| {
                let state = c.state.unwrap_or_default().trim().to_lowercase();
                let name = c.name.unwrap_or_default().trim().to_string();
                if state == "none" && name.is_empty() {
                    return None;
                }
                let listed = c.stream_support.unwrap_or_default().to_lowercase();
                let mut streams: Vec<StreamProfile> = [
                    ("mainstream", StreamProfile::Main),
                    ("externstream", StreamProfile::Extern),
                    ("substream", StreamProfile::Sub),
                ]
                .into_iter()
                .filter(|(word, _)| listed.split(',').any(|s| s.trim() == *word))
                .map(|(_, p)| p)
                .collect();
                if streams.is_empty() {
                    streams = vec![StreamProfile::Main, StreamProfile::Sub];
                }
                Some(ChannelInfo {
                    channel_id: c.channel_id?,
                    name,
                    online: state == "connect",
                    streams,
                    uid: c.uid.filter(|u| !u.is_empty()),
                })
            })
            .collect()
    }
}

/// The device's own description of itself (reply to `MSG_ID_VERSION`, or
/// to `MSG_ID_CHANNEL_VERSION` for the camera on a channel): the name the
/// owner gave it, its model, and its build, hardware and firmware versions.
/// The serial number the reply also carries is never read.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct VersionInfo {
    #[serde(rename = "channelId")]
    pub channel_id: Option<u8>,
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub model: Option<String>,
    #[serde(rename = "firmwareVersion")]
    pub firmware_version: Option<String>,
    /// The build, as text (`build 2510172100`).
    #[serde(rename = "buildDay")]
    pub build_day: Option<String>,
    #[serde(rename = "hardwareVersion")]
    pub hardware_version: Option<String>,
    #[serde(rename = "cfgVersion")]
    pub cfg_version: Option<String>,
    /// The hardware detail string.
    pub detail: Option<String>,
    /// Only cameras with a separate detection model report it.
    #[serde(rename = "aiVersion")]
    pub ai_version: Option<String>,
}

impl BcXml {
    pub fn to_bytes(&self) -> Vec<u8> {
        let inner = quick_xml::se::to_string(self).expect("BcXml always serializes");
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>{inner}").into_bytes()
    }

    pub fn from_bytes(buf: &[u8]) -> crate::protocol::Result<Self> {
        Ok(quick_xml::de::from_reader(buf)?)
    }
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct Encryption {
    #[serde(rename = "@version")]
    pub version: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub nonce: String,
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct LoginUser {
    #[serde(rename = "@version")]
    pub version: String,
    #[serde(rename = "userName")]
    pub user_name: String,
    pub password: String,
    #[serde(rename = "userVer")]
    pub user_ver: u32,
}

#[derive(Debug, PartialEq, Deserialize, Serialize)]
pub struct LoginNet {
    #[serde(rename = "@version")]
    pub version: String,
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(rename = "udpPort")]
    pub udp_port: u16,
}

impl Default for LoginNet {
    fn default() -> Self {
        LoginNet {
            version: XML_VERSION.to_string(),
            type_: "LAN".to_string(),
            udp_port: 0,
        }
    }
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct DeviceInfo {
    #[serde(rename = "@version")]
    pub version: Option<String>,
    pub resolution: Option<Resolution>,
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct Resolution {
    pub name: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct Preview {
    #[serde(rename = "@version")]
    pub version: String,
    #[serde(rename = "channelId")]
    pub channel_id: u8,
    pub handle: u32,
    #[serde(rename = "streamType", skip_serializing_if = "Option::is_none")]
    pub stream_type: Option<String>,
}

/// Describes the payload that follows the payload_offset in a modern
/// message. We only need `binary_data`/`channel_id` for the MVP (video
/// stream frames); other fields real cameras may send are ignored by serde.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename = "Extension")]
pub struct Extension {
    #[serde(rename = "@version")]
    pub version: String,
    #[serde(rename = "binaryData", skip_serializing_if = "Option::is_none")]
    pub binary_data: Option<u32>,
    #[serde(rename = "channelId", skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<u8>,
    /// How many leading bytes of the *following* payload are actually
    /// AES-encrypted; the rest is sent as plaintext. Only present on the
    /// message that starts a new BcMedia video/audio unit (the one with
    /// `binary_data == Some(1)`) — confirmed against real hardware
    /// 2026-09-14: messages of the same unit that lack this field
    /// entirely (continuation chunks) are sent fully in plaintext, and
    /// AES-decrypting them anyway corrupts every frame past its first
    /// `encrypt_len` bytes.
    /// Cover-preview data spell the element `EncryptLen`; both spellings
    /// are read, and `encryptLen` is written.
    #[serde(rename = "encryptLen", alias = "EncryptLen", skip_serializing_if = "Option::is_none")]
    pub encrypt_len: Option<u32>,
    /// The camera's integrity self-check for this message: the 4 bytes at
    /// `check_pos` of the *decoded* payload, read little-endian, must equal
    /// `check_value`. In every observed message `check_pos` is 0, i.e. it
    /// covers the first dword of each message's payload (verified on all
    /// 1000 messages of a real TCP trace). A mismatch means the camera sent
    /// different bytes than it checksummed — seen once the camera's send
    /// buffer overruns because the client is too slow.
    #[serde(rename = "checkPos", skip_serializing_if = "Option::is_none", default)]
    pub check_pos: Option<u32>,
    #[serde(rename = "checkValue", skip_serializing_if = "Option::is_none", default)]
    pub check_value: Option<i32>,
}

/// Escapes the five XML special characters, for text inserted into a
/// hand-built XML string (a preset name, typed by the user).
pub fn xml_escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&apos;".to_string(),
            other => other.to_string(),
        })
        .collect()
}

impl Extension {
    pub fn to_bytes(&self) -> Vec<u8> {
        let inner = quick_xml::se::to_string(self).expect("Extension always serializes");
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>{inner}").into_bytes()
    }

    pub fn from_bytes(buf: &[u8]) -> crate::protocol::Result<Self> {
        Ok(quick_xml::de::from_reader(buf)?)
    }
}

// ---------------------------------------------------------------------------
// Recorded video: calendar, file search, playback, download, event windows
// and cover previews. Element names and their order are as seen on the wire;
// times are device-local `CivilTime`s.
// ---------------------------------------------------------------------------

/// A camera's UID as recorded-video requests carry it in `<uid>` (taken from
/// the channel list, `ChannelInfoXml::uid`). It identifies the user's camera
/// and must never reach a log: `Debug` hides it and there is no `Display`.
/// Use [`CameraUid::as_str`] only to put it on the wire.
#[derive(Clone, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct CameraUid(String);

impl CameraUid {
    pub fn new(uid: impl Into<String>) -> Self {
        CameraUid(uid.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for CameraUid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CameraUid(<hidden, {} chars>)", self.0.chars().count())
    }
}

/// A recorded file's `Id` as a file search returns it, passed back unchanged
/// to open the file. Do not build one: the layout differs per device, and on
/// a Home Hub it ends with the camera's UID. `Debug` therefore masks every
/// dash-separated part that contains a letter.
#[derive(Clone, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct RecordingId(String);

impl RecordingId {
    pub fn new(id: impl Into<String>) -> Self {
        RecordingId(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for RecordingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let masked: Vec<&str> = self
            .0
            .split('-')
            .map(|part| if part.chars().any(|c| c.is_ascii_alphabetic()) { "<hidden>" } else { part })
            .collect();
        write!(f, "RecordingId({:?})", masked.join("-"))
    }
}

/// Which recorded stream a request is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordStream {
    Main,
    Sub,
}

impl RecordStream {
    /// `mainStream` / `subStream`, the `<streamType>` of file searches,
    /// playback, close, download and cover-preview requests.
    pub fn xml_name(self) -> &'static str {
        match self {
            RecordStream::Main => "mainStream",
            RecordStream::Sub => "subStream",
        }
    }

    pub fn from_xml_name(name: &str) -> Option<Self> {
        match name.trim() {
            "mainStream" => Some(RecordStream::Main),
            "subStream" => Some(RecordStream::Sub),
            _ => None,
        }
    }

    /// `<supportSub>` of a playback or download request: 1 for the sub
    /// stream, 0 for the main stream.
    pub fn support_sub(self) -> u8 {
        match self {
            RecordStream::Main => 0,
            RecordStream::Sub => 1,
        }
    }

    /// `<streamType>` of an event-window search, which is a number there:
    /// 1 for the sub stream, 0 for the main stream (see [`FindAlarmVideo`]).
    pub fn event_stream_type(self) -> u8 {
        match self {
            RecordStream::Main => 0,
            RecordStream::Sub => 1,
        }
    }
}

/// A list of `<i>` items (record and event types).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ItemList {
    #[serde(rename = "i", default)]
    pub items: Vec<String>,
}

impl ItemList {
    pub fn of(items: &[&str]) -> Self {
        ItemList { items: items.iter().map(|s| s.to_string()).collect() }
    }
}

/// `logicChnBitmap` as sent in searches, downloads and some cover previews.
pub const ALL_LOGIC_CHANNELS: u32 = 255;

/// The record types a whole-day file search asks for, as a comma list...
pub const FILE_SEARCH_RECORD_TYPES: &str = "manual, sched, io, md, people, face, vehicle, dog_cat, visitor, other, package, crossline, intrusion, loitering, legacy, loss";
/// ...and as the item list sent alongside it (it has three more entries).
pub const FILE_SEARCH_RECORD_TYPE_ITEMS: [&str; 19] = [
    "manual", "sched", "io", "md", "people", "face", "vehicle", "dog_cat", "visitor", "other", "package",
    "crossline", "intrusion", "loitering", "legacy", "loss", "answer", "nonmotorveh", "IPCInputOutput",
];

/// The event types a whole-day event-window search asks for, as a comma
/// list and as the item list sent alongside it (sent by
/// `ReolinkClient::search_events`; the Playback page's `EventFilter` groups
/// them for the user).
pub const EVENT_SEARCH_ALARM_TYPES: &str = "md, pir, other, package, io, people, face, vehicle, dog_cat, visitor, cry, crossline, intrude, loitering, legacy, loss, answer";
pub const EVENT_SEARCH_ALARM_TYPE_ITEMS: [&str; 19] = [
    "md", "pir", "other", "io", "people", "face", "vehicle", "dog_cat", "visitor", "cry", "crossline", "intrude",
    "loitering", "legacy", "loss", "answer", "nonmotorveh", "tamper", "lmsg",
];

/// A file-search page holds at most this many files. A full page means
/// there may be more: ask for the next page until one is shorter.
pub const FILE_SEARCH_PAGE_SIZE: usize = 40;

/// Serializes `value` as the `root` element of a request body, in the layout
/// recorded-video requests are seen with on the wire: the XML declaration,
/// every element on its own line without indentation, a final newline.
fn request_body<T: Serialize>(root: &str, value: &T) -> Vec<u8> {
    let mut inner = String::new();
    let mut ser =
        quick_xml::se::Serializer::with_root(&mut inner, Some(root)).expect("a valid root element name");
    ser.indent(' ', 0);
    value.serialize(ser).expect("request bodies always serialize");
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n{inner}\n</body>\n").into_bytes()
}

/// `<FileInfoList version="1.1"><FileInfo>…</FileInfo></FileInfoList>`
/// around one request's `FileInfo`.
#[derive(Serialize)]
struct FileInfoEnvelope<'a, T> {
    #[serde(rename = "@version")]
    version: &'a str,
    #[serde(rename = "FileInfo")]
    file_info: &'a T,
}

fn file_info_request<T: Serialize>(file_info: &T) -> Vec<u8> {
    request_body("FileInfoList", &FileInfoEnvelope { version: XML_VERSION, file_info })
}

#[derive(Deserialize)]
struct FileInfoBody<T> {
    #[serde(rename = "FileInfoList")]
    list: FileInfoOne<T>,
}

#[derive(Deserialize)]
struct FileInfoOne<T> {
    #[serde(rename = "FileInfo")]
    file_info: T,
}

fn parse_file_info_request<T: DeserializeOwned>(buf: &[u8]) -> crate::protocol::Result<T> {
    let body: FileInfoBody<T> = quick_xml::de::from_reader(buf)?;
    Ok(body.list.file_info)
}

/// The recording calendar of a month (`MSG_ID_RECORD_CALENDAR`), request and
/// reply alike. The request lists one `DayRecord` per channel, with the
/// camera UID; the reply returns them with the days that have recordings.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct DayRecords {
    #[serde(rename = "@version", default)]
    pub version: String,
    #[serde(rename = "startTime", default)]
    pub start_time: CivilTime,
    #[serde(rename = "endTime", default)]
    pub end_time: CivilTime,
    #[serde(rename = "DayRecordList", default)]
    pub day_record_list: DayRecordList,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct DayRecordList {
    #[serde(rename = "DayRecord", default)]
    pub records: Vec<DayRecord>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct DayRecord {
    #[serde(default)]
    pub index: u32,
    #[serde(rename = "channelId", default)]
    pub channel_id: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<CameraUid>,
    /// Reply only, and empty (`<dayTypeList />`) when no day of the range
    /// has recordings. A request leaves it out.
    #[serde(rename = "dayTypeList", skip_serializing_if = "Option::is_none")]
    pub day_type_list: Option<DayTypeList>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct DayTypeList {
    #[serde(rename = "dayType", default)]
    pub days: Vec<DayType>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct DayType {
    /// Days after the range's first day (0 is the first day).
    #[serde(default)]
    pub index: u32,
    /// Only `normal` has been seen.
    #[serde(rename = "type", default)]
    pub kind: String,
}

impl DayRecords {
    /// The calendar request for one whole month of one channel.
    pub fn month(channel_id: u8, uid: Option<CameraUid>, year: u16, month: u8) -> Self {
        let last = CivilTime::days_in_month(year, month).max(1);
        DayRecords {
            version: XML_VERSION.to_string(),
            start_time: CivilTime::start_of_day(year, month, 1),
            end_time: CivilTime::end_of_day(year, month, last),
            day_record_list: DayRecordList {
                records: vec![DayRecord { index: 0, channel_id, uid, day_type_list: None }],
            },
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        request_body("DayRecords", self)
    }

    /// The days with recordings on `channel_id`, each as midnight of that
    /// (device-local) day, in the reply's order.
    pub fn recorded_days(&self, channel_id: u8) -> Vec<CivilTime> {
        let first = CivilTime::start_of_day(self.start_time.year, self.start_time.month, self.start_time.day);
        if !first.is_valid() {
            return Vec::new();
        }
        self.day_record_list
            .records
            .iter()
            .filter(|r| r.channel_id == channel_id)
            .filter_map(|r| r.day_type_list.as_ref())
            .flat_map(|l| l.days.iter())
            .map(|d| first.add_seconds(i64::from(d.index) * 86_400))
            .filter(|day| *day <= self.end_time)
            .collect()
    }
}

/// Starts a recorded-file search (`MSG_ID_FILE_SEARCH_START`). The reply's
/// single `FileInfo` carries the `handle` the pages are asked with.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct FileSearchStart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<CameraUid>,
    #[serde(rename = "searchAITrack")]
    pub search_ai_track: u8,
    #[serde(rename = "channelId")]
    pub channel_id: u8,
    #[serde(rename = "logicChnBitmap")]
    pub logic_chn_bitmap: u32,
    #[serde(rename = "streamType")]
    pub stream_type: String,
    #[serde(rename = "recordType")]
    pub record_type: String,
    #[serde(rename = "fileRecordType")]
    pub file_record_type: ItemList,
    #[serde(rename = "startTime")]
    pub start_time: CivilTime,
    #[serde(rename = "endTime")]
    pub end_time: CivilTime,
}

impl FileSearchStart {
    /// One whole device-local day of `stream` on `channel_id`, every record
    /// type.
    pub fn whole_day(channel_id: u8, uid: Option<CameraUid>, stream: RecordStream, year: u16, month: u8, day: u8) -> Self {
        FileSearchStart {
            uid,
            search_ai_track: 1,
            channel_id,
            logic_chn_bitmap: ALL_LOGIC_CHANNELS,
            stream_type: stream.xml_name().to_string(),
            record_type: FILE_SEARCH_RECORD_TYPES.to_string(),
            file_record_type: ItemList::of(&FILE_SEARCH_RECORD_TYPE_ITEMS),
            start_time: CivilTime::start_of_day(year, month, day),
            end_time: CivilTime::end_of_day(year, month, day),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        file_info_request(self)
    }

    pub fn from_bytes(buf: &[u8]) -> crate::protocol::Result<Self> {
        parse_file_info_request(buf)
    }
}

/// Asks for the next page of a file search (`MSG_ID_FILE_SEARCH_NEXT`), or
/// ends it (`MSG_ID_FILE_SEARCH_END`, same body).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct FileSearchPage {
    #[serde(rename = "channelId")]
    pub channel_id: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<CameraUid>,
    #[serde(rename = "searchAITrack")]
    pub search_ai_track: u8,
    pub handle: u32,
}

impl FileSearchPage {
    pub fn new(channel_id: u8, uid: Option<CameraUid>, handle: u32) -> Self {
        FileSearchPage { channel_id, uid, search_ai_track: 1, handle }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        file_info_request(self)
    }

    pub fn from_bytes(buf: &[u8]) -> crate::protocol::Result<Self> {
        parse_file_info_request(buf)
    }
}

/// Opens a recorded file for playback (`MSG_ID_REPLAY`), at the position and
/// with the sequence number set by the `ReplaySeek` sent just before.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ReplayOpen {
    #[serde(rename = "channelId")]
    pub channel_id: u8,
    #[serde(rename = "Id")]
    pub id: RecordingId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<CameraUid>,
    #[serde(rename = "supportSub")]
    pub support_sub: u8,
    /// 1, 2, 4, 8 or 16 (times real time). `None` leaves the element out,
    /// which is how slower-than-real-time play is asked for: the device then
    /// sends at 1x and the client slows down on its own. At 16 the device
    /// sends I-frames only and no audio.
    #[serde(rename = "playSpeed", skip_serializing_if = "Option::is_none")]
    pub play_speed: Option<u32>,
    #[serde(rename = "streamType")]
    pub stream_type: String,
}

impl ReplayOpen {
    pub fn new(channel_id: u8, id: RecordingId, uid: Option<CameraUid>, stream: RecordStream, play_speed: Option<u32>) -> Self {
        ReplayOpen {
            channel_id,
            id,
            uid,
            support_sub: stream.support_sub(),
            play_speed,
            stream_type: stream.xml_name().to_string(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        file_info_request(self)
    }

    pub fn from_bytes(buf: &[u8]) -> crate::protocol::Result<Self> {
        parse_file_info_request(buf)
    }
}

/// Closes a playback (`MSG_ID_REPLAY_CLOSE`). The file is named by its
/// `name`, not its `Id`, and no UID is sent.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ReplayClose {
    #[serde(rename = "channelId")]
    pub channel_id: u8,
    pub name: String,
    #[serde(rename = "streamType")]
    pub stream_type: String,
}

impl ReplayClose {
    pub fn new(channel_id: u8, name: impl Into<String>, stream: RecordStream) -> Self {
        ReplayClose { channel_id, name: name.into(), stream_type: stream.xml_name().to_string() }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        file_info_request(self)
    }

    pub fn from_bytes(buf: &[u8]) -> crate::protocol::Result<Self> {
        parse_file_info_request(buf)
    }
}

/// Downloads a recorded time range (`MSG_ID_DOWNLOAD`), given as device-local
/// start and end.
///
/// Not used by Reoling yet: groundwork for downloading recordings, kept with
/// its tests until that feature is built.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct DownloadRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<CameraUid>,
    #[serde(rename = "logicChnBitmap")]
    pub logic_chn_bitmap: u32,
    #[serde(rename = "channelId")]
    pub channel_id: u8,
    #[serde(rename = "supportSub")]
    pub support_sub: u8,
    #[serde(rename = "streamType")]
    pub stream_type: String,
    #[serde(rename = "startTime")]
    pub start_time: CivilTime,
    #[serde(rename = "endTime")]
    pub end_time: CivilTime,
}

impl DownloadRequest {
    /// Only a sub-stream download of exactly one file's span has been seen;
    /// `supportSub` follows the stream as in a playback request.
    pub fn new(channel_id: u8, uid: Option<CameraUid>, stream: RecordStream, start: CivilTime, end: CivilTime) -> Self {
        DownloadRequest {
            uid,
            logic_chn_bitmap: ALL_LOGIC_CHANNELS,
            channel_id,
            support_sub: stream.support_sub(),
            stream_type: stream.xml_name().to_string(),
            start_time: start,
            end_time: end,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        file_info_request(self)
    }

    pub fn from_bytes(buf: &[u8]) -> crate::protocol::Result<Self> {
        parse_file_info_request(buf)
    }
}

/// `FileInfoList` as the device sends it: the file-search start reply (one
/// entry with the handle), each file-search page (one entry per file), and
/// the download reply (one entry with the size estimate).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct FileInfoList {
    #[serde(rename = "@version", default)]
    pub version: String,
    #[serde(rename = "FileInfo", default)]
    pub files: Vec<FileInfo>,
}

/// One `FileInfo` entry of a reply. Every element is optional.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct FileInfo {
    #[serde(rename = "channelId", skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<u32>,
    /// The channel number plus one as two digits, then the device-local
    /// start as `YYYYMMDDhhmmss`. A close names the file by it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "Id", skip_serializing_if = "Option::is_none")]
    pub id: Option<RecordingId>,
    #[serde(rename = "streamType", skip_serializing_if = "Option::is_none")]
    pub stream_type: Option<String>,
    #[serde(rename = "containsAudio", skip_serializing_if = "Option::is_none")]
    pub contains_audio: Option<u8>,
    /// Comma list; `none` on every file entry seen. Event types come from
    /// the event-window search instead.
    #[serde(rename = "recordType", skip_serializing_if = "Option::is_none")]
    pub record_type: Option<String>,
    #[serde(rename = "sizeL", skip_serializing_if = "Option::is_none")]
    pub size_l: Option<u32>,
    #[serde(rename = "sizeH", skip_serializing_if = "Option::is_none")]
    pub size_h: Option<u32>,
    #[serde(rename = "supportSub", skip_serializing_if = "Option::is_none")]
    pub support_sub: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypt: Option<u8>,
    #[serde(rename = "startTime", skip_serializing_if = "Option::is_none")]
    pub start_time: Option<CivilTime>,
    #[serde(rename = "endTime", skip_serializing_if = "Option::is_none")]
    pub end_time: Option<CivilTime>,
    /// Download reply only.
    #[serde(rename = "FileCount", skip_serializing_if = "Option::is_none")]
    pub file_count: Option<u32>,
}

impl FileInfo {
    /// `sizeH:sizeL` as one byte count. The device rounds it up and it can
    /// exceed the media actually delivered, so it is an upper estimate.
    pub fn size_bytes(&self) -> Option<u64> {
        let low = self.size_l?;
        Some((u64::from(self.size_h.unwrap_or(0)) << 32) | u64::from(low))
    }

    /// This entry as a playable recording: it needs a name, an `Id` and a
    /// valid start no later than its end.
    pub fn recording(&self) -> Option<RecordedFile> {
        let name = self.name.as_deref().map(str::trim).filter(|n| !n.is_empty())?;
        let id = self.id.as_ref().filter(|id| !id.is_empty())?;
        let (start, end) = (self.start_time?, self.end_time?);
        if !start.is_valid() || !end.is_valid() || end < start {
            return None;
        }
        Some(RecordedFile {
            name: name.to_string(),
            id: id.clone(),
            stream: self.stream_type.as_deref().and_then(RecordStream::from_xml_name),
            start,
            end,
            size_bytes: self.size_bytes(),
            has_audio: self.contains_audio.unwrap_or(0) != 0,
            encrypted: self.encrypt.unwrap_or(0) != 0,
        })
    }
}

impl FileInfoList {
    /// The search handle in a file-search start reply.
    pub fn handle(&self) -> Option<u32> {
        self.files.first().and_then(|f| f.handle)
    }

    /// Whether this page is full, so that another page may follow.
    pub fn is_full_page(&self) -> bool {
        self.files.len() >= FILE_SEARCH_PAGE_SIZE
    }
}

/// A recorded file from a file search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedFile {
    /// What a close (`ReplayClose`) names the file by.
    pub name: String,
    /// What a playback (`ReplayOpen`) opens the file by.
    pub id: RecordingId,
    pub stream: Option<RecordStream>,
    /// Device-local start and end. A file can overlap its successor by a few
    /// seconds, so treat it as the half-open span `start..end`.
    pub start: CivilTime,
    pub end: CivilTime,
    /// Upper estimate, see [`FileInfo::size_bytes`].
    pub size_bytes: Option<u64>,
    pub has_audio: bool,
    pub encrypted: bool,
}

/// Sets the start of the next playback (`MSG_ID_REPLAY_SEEK`): its position
/// (device-local) and the sequence number its data will carry in their
/// headers (see `model::replay_seq`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct ReplaySeek {
    #[serde(rename = "@version", default)]
    pub version: String,
    #[serde(rename = "channelId", default)]
    pub channel_id: u8,
    #[serde(default)]
    pub seq: u32,
    #[serde(rename = "seekTime", default)]
    pub seek_time: CivilTime,
}

impl ReplaySeek {
    /// Channel 0 has been seen to seek correctly for every channel of a Home
    /// Hub and an NVR (the real channel goes in `ReplayOpen`); sending the
    /// real channel here is untested.
    pub fn new(channel_id: u8, seq: u32, seek_time: CivilTime) -> Self {
        ReplaySeek { version: XML_VERSION.to_string(), channel_id, seq, seek_time }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        request_body("ReplaySeek", self)
    }
}

/// Event-window search (`findAlarmVideo`): the start request (272, a whole
/// day), its reply (the `file_handle`), and the next/end requests (273/274,
/// channel and handle only). All of them go with an Extension naming the
/// channel. The pages come back as [`AlarmVideoInfo`]; the client's
/// `search_events` runs the whole exchange.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct FindAlarmVideo {
    #[serde(rename = "@version", default)]
    pub version: String,
    #[serde(rename = "channelId", skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<CameraUid>,
    #[serde(rename = "fileHandle", skip_serializing_if = "Option::is_none")]
    pub file_handle: Option<u32>,
    #[serde(rename = "logicChnBitmap", skip_serializing_if = "Option::is_none")]
    pub logic_chn_bitmap: Option<u32>,
    /// A number here: see [`RecordStream::event_stream_type`].
    #[serde(rename = "streamType", skip_serializing_if = "Option::is_none")]
    pub stream_type: Option<u8>,
    #[serde(rename = "notSearchVideo", skip_serializing_if = "Option::is_none")]
    pub not_search_video: Option<u8>,
    #[serde(rename = "startTime", skip_serializing_if = "Option::is_none")]
    pub start_time: Option<CivilTime>,
    #[serde(rename = "endTime", skip_serializing_if = "Option::is_none")]
    pub end_time: Option<CivilTime>,
    #[serde(rename = "alarmType", skip_serializing_if = "Option::is_none")]
    pub alarm_type: Option<String>,
    #[serde(rename = "eventAlarmType", skip_serializing_if = "Option::is_none")]
    pub event_alarm_type: Option<ItemList>,
}

impl FindAlarmVideo {
    /// The start request: every event type over one whole device-local day.
    pub fn whole_day(channel_id: u8, uid: Option<CameraUid>, stream: RecordStream, year: u16, month: u8, day: u8) -> Self {
        FindAlarmVideo {
            version: XML_VERSION.to_string(),
            channel_id: Some(channel_id),
            uid,
            file_handle: None,
            logic_chn_bitmap: Some(ALL_LOGIC_CHANNELS),
            stream_type: Some(stream.event_stream_type()),
            not_search_video: Some(0),
            start_time: Some(CivilTime::start_of_day(year, month, day)),
            end_time: Some(CivilTime::end_of_day(year, month, day)),
            alarm_type: Some(EVENT_SEARCH_ALARM_TYPES.to_string()),
            event_alarm_type: Some(ItemList::of(&EVENT_SEARCH_ALARM_TYPE_ITEMS)),
        }
    }

    /// The next-page (273) and end (274) requests.
    pub fn page(channel_id: u8, file_handle: u32) -> Self {
        FindAlarmVideo {
            version: XML_VERSION.to_string(),
            channel_id: Some(channel_id),
            file_handle: Some(file_handle),
            ..Default::default()
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        request_body("findAlarmVideo", self)
    }
}

/// One page of event windows (reply to `MSG_ID_EVENT_SEARCH_NEXT`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct AlarmVideoInfo {
    #[serde(rename = "@version", default)]
    pub version: String,
    #[serde(rename = "channelId", skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<u8>,
    #[serde(rename = "fileHandle", skip_serializing_if = "Option::is_none")]
    pub file_handle: Option<u32>,
    /// 1 on the last page.
    #[serde(rename = "bFinished", skip_serializing_if = "Option::is_none")]
    pub finished: Option<u8>,
    #[serde(rename = "alarmVideoList", skip_serializing_if = "Option::is_none")]
    pub alarm_video_list: Option<AlarmVideoList>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct AlarmVideoList {
    #[serde(rename = "alarmVideo", default)]
    pub entries: Vec<AlarmVideo>,
}

/// One event window. Home Hubs add the camera UID, the logical channel and a
/// deleted flag and leave `fileId` empty; NVRs fill `fileId` with the
/// recording's `Id`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct AlarmVideo {
    /// The `name` of the recorded file the window lies in.
    #[serde(rename = "fileName", skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<CameraUid>,
    #[serde(rename = "logicChn", skip_serializing_if = "Option::is_none")]
    pub logic_chn: Option<u8>,
    #[serde(rename = "bHasRecFile", skip_serializing_if = "Option::is_none")]
    pub has_rec_file: Option<u8>,
    #[serde(rename = "bEncrypted", skip_serializing_if = "Option::is_none")]
    pub encrypted: Option<u8>,
    #[serde(rename = "bDeleted", skip_serializing_if = "Option::is_none")]
    pub deleted: Option<u8>,
    /// One event type, e.g. `md`, `people`, `vehicle`, `dog_cat`.
    #[serde(rename = "alarmType", skip_serializing_if = "Option::is_none")]
    pub alarm_type: Option<String>,
    #[serde(rename = "fileId", skip_serializing_if = "Option::is_none")]
    pub file_id: Option<RecordingId>,
    #[serde(rename = "startTime", skip_serializing_if = "Option::is_none")]
    pub start_time: Option<CivilTime>,
    #[serde(rename = "endTime", skip_serializing_if = "Option::is_none")]
    pub end_time: Option<CivilTime>,
}

impl AlarmVideoInfo {
    pub fn is_finished(&self) -> bool {
        self.finished.unwrap_or(0) != 0
    }

    pub fn entries(&self) -> &[AlarmVideo] {
        self.alarm_video_list.as_ref().map(|l| l.entries.as_slice()).unwrap_or(&[])
    }
}

/// Asks for one still picture of a recorded file (`MSG_ID_COVER_PREVIEW`).
///
/// Not used by Reoling yet (nor is [`FrameList`]): groundwork for recording
/// thumbnails, kept with its tests until that feature is built.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct CoverPreview {
    #[serde(rename = "@version", default)]
    pub version: String,
    #[serde(rename = "channelId", default)]
    pub channel_id: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<CameraUid>,
    #[serde(rename = "logicChnBitmap", skip_serializing_if = "Option::is_none")]
    pub logic_chn_bitmap: Option<u32>,
    #[serde(default)]
    pub desc: u8,
    #[serde(rename = "streamType", default)]
    pub stream_type: String,
    #[serde(rename = "startTime", default)]
    pub start_time: CivilTime,
    #[serde(rename = "endTime", default)]
    pub end_time: CivilTime,
    #[serde(rename = "frameList", default)]
    pub frame_list: FrameList,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct FrameList {
    #[serde(rename = "frameNo", default)]
    pub frame_no: u32,
}

impl CoverPreview {
    /// The picture of the file spanning `start`..`end` (its times from the
    /// file search). The main stream is asked for even for a sub-stream
    /// file's times, which is what has been seen to work.
    pub fn new(channel_id: u8, uid: Option<CameraUid>, start: CivilTime, end: CivilTime) -> Self {
        CoverPreview {
            version: XML_VERSION.to_string(),
            channel_id,
            uid,
            logic_chn_bitmap: None,
            desc: 0,
            stream_type: RecordStream::Main.xml_name().to_string(),
            start_time: start,
            end_time: end,
            frame_list: FrameList { frame_no: 1 },
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        request_body("CoverPreview", self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_user_round_trips() {
        let xml = BcXml {
            login_user: Some(LoginUser {
                version: XML_VERSION.to_string(),
                user_name: "ADMINHASH".to_string(),
                password: "PASSWORDHASH".to_string(),
                user_ver: 1,
            }),
            login_net: Some(LoginNet::default()),
            ..Default::default()
        };
        let bytes = xml.to_bytes();
        let parsed = BcXml::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.login_user.unwrap().user_name, "ADMINHASH");
        assert_eq!(parsed.login_net.unwrap().type_, "LAN");
    }

    #[test]
    fn parses_a_real_shaped_encryption_reply() {
        // Self-authored sample shaped like the camera's legacy-login reply,
        // not copied from any reference implementation's test fixtures.
        let sample = br#"<?xml version="1.0" encoding="UTF-8"?><body><Encryption version="1.1"><type>md5</type><nonce>AAAABBBBCCCCDDDD</nonce></Encryption></body>"#;
        let parsed = BcXml::from_bytes(sample).unwrap();
        assert_eq!(parsed.encryption.unwrap().nonce, "AAAABBBBCCCCDDDD");
    }
}

#[cfg(test)]
mod channel_info_tests {
    use super::*;
    use crate::client::StreamProfile;

    #[test]
    fn osd_reply_gives_the_channel_name() {
        // Shape of a real NVR reply.
        let xml = br#"<?xml version="1.0" encoding="UTF-8" ?><body>
            <OsdDatetime version="1.1"><channelId>6</channelId><enable>1</enable></OsdDatetime>
            <OsdChannelName version="1.1"><channelId>6</channelId><name>Camera7</name>
            <enable>1</enable></OsdChannelName></body>"#;
        let name = BcXml::from_bytes(xml).unwrap().osd_channel_name.unwrap().name;
        assert_eq!(name.as_deref(), Some("Camera7"));
    }

    #[test]
    fn support_table_gives_each_channels_abilities() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" ?><body><Support version="1.1">
            <channelNum>24</channelNum>
            <smartHome><version>1</version><item><name>googleHome</name><ver>1</ver></item></smartHome>
            <item><chnID>0</chnID><audioVersion>31</audioVersion><ledCtrl>3</ledCtrl><ipcAudioTalk>1</ipcAudioTalk><ptzType>5</ptzType><ptzControl>47</ptzControl><lightType>0</lightType></item>
            <item><chnID>2</chnID><audioVersion>31</audioVersion><ledCtrl>38</ledCtrl><ipcAudioTalk>1</ipcAudioTalk><ptzType>0</ptzType><ptzControl>64</ptzControl><lightType>1</lightType></item>
            <item><chnID>3</chnID><audioVersion>0</audioVersion><ledCtrl>0</ledCtrl><ipcAudioTalk>0</ipcAudioTalk></item>
            </Support></body>"#;
        let abilities = BcXml::from_bytes(xml).unwrap().support.unwrap().into_abilities();
        assert_eq!(abilities.len(), 3);
        let of = |c: u8| abilities.iter().find(|(id, _)| *id == c).unwrap().1;
        assert!(of(0).siren && of(0).talk && of(0).pan && of(0).tilt && of(0).zoom && of(0).spotlight);
        assert!(of(0).monitor_point && of(0).calibration);
        assert!(of(2).siren && of(2).talk && of(2).spotlight && !of(2).pan);
        // ptzControl=64 has bits 2/3 clear, and no physical PTZ either.
        assert!(!of(2).monitor_point && !of(2).calibration);
        assert!(!of(3).siren && !of(3).talk && !of(3).spotlight);
    }

    #[test]
    fn support_table_gives_the_settings_gates() {
        // The four rows of a real NVR: a PTZ camera with a status LED, a
        // spotlight camera with the newer picture pipeline, a plain camera
        // and one that calls the third detection type "Pet".
        let xml = br#"<?xml version="1.0" encoding="UTF-8" ?><body><Support version="1.1">
            <item><chnID>0</chnID><ledCtrl>3</ledCtrl><ptzType>5</ptzType><ptzControl>47</ptzControl><newIspCfg>88003</newIspCfg><aiAnimalType>1</aiAnimalType></item>
            <item><chnID>1</chnID><ledCtrl>38</ledCtrl><ptzType>0</ptzType><ptzControl>64</ptzControl><newIspCfg>219075</newIspCfg><aiAnimalType>1</aiAnimalType></item>
            <item><chnID>2</chnID><ledCtrl>0</ledCtrl><newIspCfg>88003</newIspCfg><aiAnimalType>1</aiAnimalType></item>
            <item><chnID>3</chnID><ledCtrl>38</ledCtrl><newIspCfg>219075</newIspCfg><aiAnimalType>0</aiAnimalType></item>
            </Support></body>"#;
        let abilities = BcXml::from_bytes(xml).unwrap().support.unwrap().into_abilities();
        let of = |c: u8| abilities.iter().find(|(id, _)| *id == c).unwrap().1;
        assert!(of(0).status_led && of(0).tracking && !of(0).binning && !of(0).pet_label);
        assert!(!of(1).status_led && !of(1).tracking && of(1).binning && !of(1).pet_label);
        assert!(!of(2).status_led && !of(2).tracking && !of(2).binning && !of(2).pet_label);
        assert!(!of(3).status_led && !of(3).tracking && of(3).binning && of(3).pet_label);
    }

    #[test]
    fn monitor_point_reply_gives_state() {
        // Shape captured from the official app (see
        // .plans/reolink-baichuan-calibration-monitor-point.md section 7).
        let xml = br#"<?xml version="1.0" encoding="UTF-8" ?><body><PtzGuard version="1.1">
            <channelId>0</channelId>
            <timeout>68</timeout>
            <benable>1</benable>
            <bvalid>1</bvalid>
            <imageName>guard</imageName>
            <mode>global</mode>
            <xpos>0</xpos><ypos>0</ypos><height>0</height><width>0</width>
            </PtzGuard></body>"#;
        let state = BcXml::from_bytes(xml).unwrap().ptz_guard.unwrap().into_monitor_point();
        assert!(state.enabled && state.valid);
        assert_eq!(state.timeout_seconds, 68);
    }

    #[test]
    fn monitor_point_reply_accepts_bexistpos_instead_of_bvalid() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" ?><body><PtzGuard version="1.1">
            <channelId>0</channelId><timeout>60</timeout><benable>0</benable><bexistPos>1</bexistPos>
            </PtzGuard></body>"#;
        let state = BcXml::from_bytes(xml).unwrap().ptz_guard.unwrap().into_monitor_point();
        assert!(!state.enabled && state.valid);
    }

    #[test]
    fn zoom_focus_reply_gives_ranges_and_positions() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" ?><body><PtzZoomFocus version="1.1">
            <channelId>0</channelId>
            <zoom><maxPos>3200</maxPos><minPos>1</minPos><curPos>120</curPos></zoom>
            <focus><maxPos>3000</maxPos><minPos>0</minPos><curPos>800</curPos></focus>
            </PtzZoomFocus></body>"#;
        let zf = BcXml::from_bytes(xml).unwrap().ptz_zoom_focus.unwrap();
        let zoom = zf.zoom.unwrap();
        assert_eq!((zoom.min_pos, zoom.max_pos, zoom.cur_pos), (Some(1), Some(3200), Some(120)));
        assert_eq!(zf.focus.unwrap().cur_pos, Some(800));
    }

    #[test]
    fn preset_list_skips_disabled_and_unnamed_entries() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" ?><body><PtzPreset version="1.1">
            <channelId>0</channelId>
            <presetList>
            <preset><id>1</id><name>test1</name><enable>1</enable></preset>
            <preset><id>2</id><name></name><enable>0</enable></preset>
            <preset><id>3</id><enable>1</enable></preset>
            </presetList></PtzPreset></body>"#;
        let presets = BcXml::from_bytes(xml).unwrap().ptz_preset.unwrap().into_presets();
        assert_eq!(presets, vec![crate::client::PtzPreset { id: 1, name: "test1".to_string() }]);
    }

    #[test]
    fn channel_list_gives_names_state_and_streams() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?><body><ChannelInfoList version="1.1">
            <ChannelInfo><channelId>0</channelId><devName>Garden</devName><state>connect</state>
              <streamSupport>
                mainStream,subStream,externStream
              </streamSupport></ChannelInfo>
            <ChannelInfo><channelId>1</channelId><devName>Door</devName><state>connect</state></ChannelInfo>
            <ChannelInfo><channelId>2</channelId><state>none</state><streamSupport>none</streamSupport></ChannelInfo>
            </ChannelInfoList></body>"#;
        let channels = BcXml::from_bytes(xml).unwrap().channel_info_list.unwrap().into_channels();
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].name, "Garden");
        assert!(channels[0].online);
        assert_eq!(
            channels[0].streams,
            [StreamProfile::Main, StreamProfile::Extern, StreamProfile::Sub]
        );
        assert_eq!(channels[1].streams, [StreamProfile::Main, StreamProfile::Sub]);
    }
}

#[cfg(test)]
mod replay_xml_tests {
    //! Bodies of the recorded-video messages, in the shapes seen on the wire.
    //! UIDs are synthetic 16-character stand-ins, so the captured body
    //! lengths still apply.
    use super::*;

    const UID: &str = "TESTCAMUID000001";

    fn uid() -> Option<CameraUid> {
        Some(CameraUid::new(UID))
    }

    /// A request/reply body: declaration, `<body>`, one element per line.
    fn body(parts: &[String]) -> String {
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n{}\n</body>\n", parts.join("\n"))
    }

    fn l(s: &str) -> String {
        s.to_string()
    }

    fn time(tag: &str, [y, mo, d, h, mi, s]: [u32; 6]) -> String {
        format!(
            "<{tag}>\n<year>{y}</year>\n<month>{mo}</month>\n<day>{d}</day>\n<hour>{h}</hour>\n<minute>{mi}</minute>\n<second>{s}</second>\n</{tag}>"
        )
    }

    fn items(tag: &str, list: &str) -> String {
        let inner: Vec<String> = list.split(' ').map(|i| format!("<i>{i}</i>")).collect();
        format!("<{tag}>\n{}\n</{tag}>", inner.join("\n"))
    }

    fn text(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn calendar_request_matches_the_wire() {
        let expected = body(&[
            l("<DayRecords version=\"1.1\">"),
            time("startTime", [2026, 10, 1, 0, 0, 0]),
            time("endTime", [2026, 10, 31, 23, 59, 59]),
            l("<DayRecordList>\n<DayRecord>\n<index>0</index>\n<channelId>0</channelId>"),
            format!("<uid>{UID}</uid>"),
            l("</DayRecord>\n</DayRecordList>\n</DayRecords>"),
        ]);
        let request = DayRecords::month(0, uid(), 2026, 10);
        assert_eq!(text(request.to_bytes()), expected);
        assert_eq!(expected.len(), 478);
        assert_eq!(BcXml::from_bytes(expected.as_bytes()).unwrap().day_records, Some(request));
        // A 30-day month ends on the 30th.
        assert_eq!(DayRecords::month(0, None, 2026, 9).end_time, CivilTime::new(2026, 9, 30, 23, 59, 59));
    }

    #[test]
    fn calendar_reply_gives_the_recorded_days() {
        let reply = body(&[
            l("<DayRecords version=\"1.1\">"),
            time("startTime", [2026, 10, 1, 0, 0, 0]),
            time("endTime", [2026, 10, 31, 23, 59, 59]),
            l("<DayRecordList>\n<DayRecord>\n<index>0</index>\n<channelId>0</channelId>\n<dayTypeList>"),
            l("<dayType>\n<index>9</index>\n<type>normal</type>\n</dayType>"),
            l("<dayType>\n<index>10</index>\n<type>normal</type>\n</dayType>"),
            l("<dayType>\n<index>17</index>\n<type>normal</type>\n</dayType>"),
            l("</dayTypeList>\n</DayRecord>\n</DayRecordList>\n</DayRecords>"),
        ]);
        let parsed = BcXml::from_bytes(reply.as_bytes()).unwrap().day_records.unwrap();
        assert_eq!(
            parsed.recorded_days(0),
            vec![
                CivilTime::start_of_day(2026, 10, 10),
                CivilTime::start_of_day(2026, 10, 11),
                CivilTime::start_of_day(2026, 10, 18)
            ]
        );
        assert!(parsed.recorded_days(1).is_empty());
        assert_eq!(text(request_body("DayRecords", &parsed)), reply);
    }

    #[test]
    fn calendar_reply_for_a_month_without_recordings() {
        let reply = body(&[
            l("<DayRecords version=\"1.1\">"),
            time("startTime", [2026, 9, 1, 0, 0, 0]),
            time("endTime", [2026, 9, 30, 23, 59, 59]),
            l("<DayRecordList>\n<DayRecord>\n<index>0</index>\n<channelId>0</channelId>\n<dayTypeList />"),
            l("</DayRecord>\n</DayRecordList>\n</DayRecords>"),
        ]);
        let parsed = BcXml::from_bytes(reply.as_bytes()).unwrap().day_records.unwrap();
        assert_eq!(parsed.day_record_list.records[0].day_type_list, Some(DayTypeList::default()));
        assert!(parsed.recorded_days(0).is_empty());
        let again = BcXml::from_bytes(&request_body("DayRecords", &parsed)).unwrap().day_records.unwrap();
        assert_eq!(again, parsed);
    }

    #[test]
    fn file_search_start_matches_the_wire() {
        let expected = body(&[
            l("<FileInfoList version=\"1.1\">\n<FileInfo>"),
            format!("<uid>{UID}</uid>"),
            l("<searchAITrack>1</searchAITrack>\n<channelId>0</channelId>\n<logicChnBitmap>255</logicChnBitmap>"),
            l("<streamType>subStream</streamType>"),
            l("<recordType>manual, sched, io, md, people, face, vehicle, dog_cat, visitor, other, package, crossline, intrusion, loitering, legacy, loss</recordType>"),
            items(
                "fileRecordType",
                "manual sched io md people face vehicle dog_cat visitor other package crossline intrusion loitering legacy loss answer nonmotorveh IPCInputOutput",
            ),
            time("startTime", [2026, 10, 18, 0, 0, 0]),
            time("endTime", [2026, 10, 18, 23, 59, 59]),
            l("</FileInfo>\n</FileInfoList>"),
        ]);
        let request = FileSearchStart::whole_day(0, uid(), RecordStream::Sub, 2026, 10, 18);
        assert_eq!(text(request.to_bytes()), expected);
        assert_eq!(expected.len(), 1000);
        assert_eq!(FileSearchStart::from_bytes(expected.as_bytes()).unwrap(), request);
    }

    #[test]
    fn file_search_start_reply_gives_the_handle() {
        let reply = body(&[
            l("<FileInfoList version=\"1.1\">\n<FileInfo>\n<channelId>0</channelId>\n<handle>0</handle>\n<name></name>"),
            l("<containsAudio>0</containsAudio>\n<recordType>manual ,io ,md ,sched</recordType>"),
            l("<supportSub>1</supportSub>\n<sizeL>0</sizeL>\n<sizeH>0</sizeH>"),
            time("startTime", [2026, 10, 18, 0, 0, 0]),
            time("endTime", [2026, 10, 18, 23, 59, 59]),
            l("</FileInfo>\n</FileInfoList>"),
        ]);
        let list = BcXml::from_bytes(reply.as_bytes()).unwrap().file_info_list.unwrap();
        assert_eq!(list.handle(), Some(0));
        assert!(list.files.iter().all(|f| f.recording().is_none()), "the handle entry is not a file");
        let again = BcXml::from_bytes(&request_body("FileInfoList", &list)).unwrap().file_info_list.unwrap();
        assert_eq!(again, list);
    }

    #[test]
    fn file_search_page_and_end_match_the_wire() {
        let expected = body(&[
            l("<FileInfoList version=\"1.1\">\n<FileInfo>\n<channelId>0</channelId>"),
            format!("<uid>{UID}</uid>"),
            l("<searchAITrack>1</searchAITrack>\n<handle>0</handle>\n</FileInfo>\n</FileInfoList>"),
        ]);
        let request = FileSearchPage::new(0, uid(), 0);
        assert_eq!(text(request.to_bytes()), expected);
        assert_eq!(expected.len(), 228);
        assert_eq!(FileSearchPage::from_bytes(expected.as_bytes()).unwrap(), request);
    }

    fn file_entry(name: &str, id: &str, size_l: u32, start: [u32; 6], end: [u32; 6]) -> Vec<String> {
        vec![
            l("<FileInfo>\n<channelId>0</channelId>\n<handle>0</handle>"),
            format!("<name>{name}</name>\n<Id>{id}</Id>"),
            l("<streamType>subStream</streamType>\n<containsAudio>1</containsAudio>\n<recordType>none</recordType>"),
            format!("<sizeL>{size_l}</sizeL>\n<sizeH>0</sizeH>\n<supportSub>1</supportSub>\n<encrypt>0</encrypt>"),
            time("startTime", start),
            time("endTime", end),
            l("</FileInfo>"),
        ]
    }

    #[test]
    fn file_search_page_reply_gives_recordings() {
        let mut parts = vec![l("<FileInfoList version=\"1.1\">")];
        parts.extend(file_entry(
            "0120261018001947",
            "1-0-0-01260917211947-00000",
            22_020_096,
            [2026, 10, 18, 0, 19, 47],
            [2026, 10, 18, 0, 22, 16],
        ));
        parts.extend(file_entry(
            "0120261018002446",
            "1-0-0-01260917212446-00000",
            10_485_760,
            [2026, 10, 18, 0, 24, 46],
            [2026, 10, 18, 0, 25, 56],
        ));
        parts.push(l("</FileInfoList>"));
        let reply = body(&parts);
        let list = BcXml::from_bytes(reply.as_bytes()).unwrap().file_info_list.unwrap();
        assert!(!list.is_full_page());
        let files: Vec<RecordedFile> = list.files.iter().filter_map(FileInfo::recording).collect();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "0120261018001947");
        assert_eq!(files[0].id.as_str(), "1-0-0-01260917211947-00000");
        assert_eq!(files[0].stream, Some(RecordStream::Sub));
        assert_eq!(files[0].start, CivilTime::new(2026, 10, 18, 0, 19, 47));
        assert_eq!(files[0].end, CivilTime::new(2026, 10, 18, 0, 22, 16));
        assert_eq!(files[0].size_bytes, Some(22_020_096));
        assert!(files[0].has_audio && !files[0].encrypted);
        // The entry order of the reply is reproduced exactly.
        assert_eq!(text(request_body("FileInfoList", &list)), reply);
    }

    #[test]
    fn hub_recording_ids_keep_the_camera_uid_out_of_debug_output() {
        let id = format!("1-0-01260917062107-00000-{UID}");
        let mut parts = vec![l("<FileInfoList version=\"1.1\">")];
        parts.extend(file_entry(
            "0120261017092107",
            &id,
            84_410_368,
            [2026, 10, 17, 9, 21, 7],
            [2026, 10, 17, 10, 21, 5],
        ));
        parts.push(l("</FileInfoList>"));
        let list = BcXml::from_bytes(body(&parts).as_bytes()).unwrap().file_info_list.unwrap();
        let file = &list.files[0].recording().expect("a playable file");
        assert_eq!(file.id.as_str(), id);
        let shown = format!("{list:?} {file:?}");
        assert!(!shown.contains(UID), "{shown}");
        assert!(shown.contains("1-0-01260917062107-00000-<hidden>"));
    }

    #[test]
    fn replay_seek_matches_the_wire() {
        let expected = body(&[
            l("<ReplaySeek version=\"1.1\">\n<channelId>0</channelId>\n<seq>1792348477</seq>"),
            time("seekTime", [2026, 10, 17, 9, 59, 48]),
            l("</ReplaySeek>"),
        ]);
        let request = ReplaySeek::new(0, 1_792_348_477, CivilTime::new(2026, 10, 17, 9, 59, 48));
        assert_eq!(text(request.to_bytes()), expected);
        assert_eq!(expected.len(), 271);
        assert_eq!(BcXml::from_bytes(expected.as_bytes()).unwrap().replay_seek, Some(request));
    }

    #[test]
    fn replay_open_matches_the_wire() {
        let open = |id: &str, speed: Option<&str>| {
            let mut parts = vec![
                l("<FileInfoList version=\"1.1\">\n<FileInfo>\n<channelId>0</channelId>"),
                format!("<Id>{id}</Id>\n<uid>{UID}</uid>\n<supportSub>1</supportSub>"),
            ];
            if let Some(speed) = speed {
                parts.push(format!("<playSpeed>{speed}</playSpeed>"));
            }
            parts.push(l("<streamType>subStream</streamType>\n</FileInfo>\n</FileInfoList>"));
            body(&parts)
        };
        let nvr_id = "1-0-0-01260917065850-00000";
        let hub_id = format!("1-0-01260917062107-00000-{UID}");
        for (id, speed, wire_len) in [(nvr_id, Some(1), 299), (hub_id.as_str(), Some(1), 314), (hub_id.as_str(), None, 289)] {
            let expected = open(id, speed.map(|_| "1"));
            let request = ReplayOpen::new(0, RecordingId::new(id), uid(), RecordStream::Sub, speed);
            assert_eq!(text(request.to_bytes()), expected);
            assert_eq!(expected.len(), wire_len);
            assert_eq!(ReplayOpen::from_bytes(expected.as_bytes()).unwrap(), request);
        }
        // Main stream: supportSub 0.
        let main = ReplayOpen::new(2, RecordingId::new(nvr_id), None, RecordStream::Main, Some(16));
        let main_text = text(main.to_bytes());
        assert!(main_text.contains("<supportSub>0</supportSub>\n<playSpeed>16</playSpeed>\n<streamType>mainStream</streamType>"));
        assert!(!main_text.contains("<uid>"));
    }

    #[test]
    fn replay_close_matches_the_wire() {
        let expected = body(&[l(
            "<FileInfoList version=\"1.1\">\n<FileInfo>\n<channelId>0</channelId>\n<name>0120261017092107</name>\n<streamType>subStream</streamType>\n</FileInfo>\n</FileInfoList>",
        )]);
        let request = ReplayClose::new(0, "0120261017092107", RecordStream::Sub);
        assert_eq!(text(request.to_bytes()), expected);
        assert_eq!(expected.len(), 213);
        assert_eq!(ReplayClose::from_bytes(expected.as_bytes()).unwrap(), request);
    }

    #[test]
    fn download_request_and_reply_match_the_wire() {
        let expected = body(&[
            l("<FileInfoList version=\"1.1\">\n<FileInfo>"),
            format!("<uid>{UID}</uid>"),
            l("<logicChnBitmap>255</logicChnBitmap>\n<channelId>0</channelId>\n<supportSub>1</supportSub>\n<streamType>subStream</streamType>"),
            time("startTime", [2026, 10, 17, 0, 48, 5]),
            time("endTime", [2026, 10, 17, 0, 49, 13]),
            l("</FileInfo>\n</FileInfoList>"),
        ]);
        let request = DownloadRequest::new(
            0,
            uid(),
            RecordStream::Sub,
            CivilTime::new(2026, 10, 17, 0, 48, 5),
            CivilTime::new(2026, 10, 17, 0, 49, 13),
        );
        assert_eq!(text(request.to_bytes()), expected);
        assert_eq!(expected.len(), 530);
        assert_eq!(DownloadRequest::from_bytes(expected.as_bytes()).unwrap(), request);

        let reply = body(&[l(
            "<FileInfoList version=\"1.1\">\n<FileInfo>\n<sizeL>10485760</sizeL>\n<sizeH>0</sizeH>\n<FileCount>1</FileCount>\n</FileInfo>\n</FileInfoList>",
        )]);
        assert_eq!(reply.len(), 189);
        let list = BcXml::from_bytes(reply.as_bytes()).unwrap().file_info_list.unwrap();
        assert_eq!(list.files[0].size_bytes(), Some(10_485_760));
        assert_eq!(list.files[0].file_count, Some(1));
        assert_eq!(text(request_body("FileInfoList", &list)), reply);
    }

    #[test]
    fn event_window_requests_match_the_wire() {
        let expected = body(&[
            l("<findAlarmVideo version=\"1.1\">\n<channelId>0</channelId>"),
            format!("<uid>{UID}</uid>"),
            l("<logicChnBitmap>255</logicChnBitmap>\n<streamType>1</streamType>\n<notSearchVideo>0</notSearchVideo>"),
            time("startTime", [2026, 10, 18, 0, 0, 0]),
            time("endTime", [2026, 10, 18, 23, 59, 59]),
            l("<alarmType>md, pir, other, package, io, people, face, vehicle, dog_cat, visitor, cry, crossline, intrude, loitering, legacy, loss, answer</alarmType>"),
            items(
                "eventAlarmType",
                "md pir other io people face vehicle dog_cat visitor cry crossline intrude loitering legacy loss answer nonmotorveh tamper lmsg",
            ),
            l("</findAlarmVideo>"),
        ]);
        let start = FindAlarmVideo::whole_day(0, uid(), RecordStream::Sub, 2026, 10, 18);
        assert_eq!(text(start.to_bytes()), expected);
        assert_eq!(expected.len(), 956);
        assert_eq!(BcXml::from_bytes(expected.as_bytes()).unwrap().find_alarm_video, Some(start));

        // The start reply and the next/end requests share one shape.
        for (handle, wire_len) in [(1, 156), (13, 157)] {
            let page = body(&[format!(
                "<findAlarmVideo version=\"1.1\">\n<channelId>0</channelId>\n<fileHandle>{handle}</fileHandle>\n</findAlarmVideo>"
            )]);
            assert_eq!(text(FindAlarmVideo::page(0, handle).to_bytes()), page);
            assert_eq!(page.len(), wire_len);
            let parsed = BcXml::from_bytes(page.as_bytes()).unwrap().find_alarm_video.unwrap();
            assert_eq!(parsed.file_handle, Some(handle));
            assert_eq!(parsed, FindAlarmVideo::page(0, handle));
        }
    }

    #[test]
    fn event_window_pages_from_an_nvr_and_a_hub() {
        let nvr = body(&[
            l("<alarmVideoInfo version=\"1.1\">\n<channelId>0</channelId>\n<fileHandle>1</fileHandle>\n<bFinished>0</bFinished>\n<alarmVideoList>"),
            l("<alarmVideo>\n<fileName>0120261018001947</fileName>\n<bHasRecFile>1</bHasRecFile>\n<bEncrypted>0</bEncrypted>"),
            l("<alarmType>dog_cat</alarmType>\n<fileId>1-0-0-01260917211947-00000</fileId>"),
            time("startTime", [2026, 10, 18, 0, 19, 47]),
            time("endTime", [2026, 10, 18, 0, 20, 3]),
            l("</alarmVideo>\n</alarmVideoList>\n</alarmVideoInfo>"),
        ]);
        let page = BcXml::from_bytes(nvr.as_bytes()).unwrap().alarm_video_info.unwrap();
        assert!(!page.is_finished());
        let entry = &page.entries()[0];
        assert_eq!(entry.alarm_type.as_deref(), Some("dog_cat"));
        assert_eq!(entry.file_id.as_ref().map(RecordingId::as_str), Some("1-0-0-01260917211947-00000"));
        assert_eq!(entry.start_time, Some(CivilTime::new(2026, 10, 18, 0, 19, 47)));
        assert_eq!(text(request_body("alarmVideoInfo", &page)), nvr);

        let hub = body(&[
            l("<alarmVideoInfo version=\"1.1\">\n<channelId>0</channelId>\n<fileHandle>13</fileHandle>\n<bFinished>1</bFinished>\n<alarmVideoList>"),
            l("<alarmVideo>\n<fileName>0120261018015959</fileName>"),
            format!("<uid>{UID}</uid>"),
            l("<logicChn>0</logicChn>\n<bHasRecFile>1</bHasRecFile>\n<bEncrypted>0</bEncrypted>\n<bDeleted>0</bDeleted>"),
            l("<alarmType>md</alarmType>\n<fileId></fileId>"),
            time("startTime", [2026, 10, 18, 2, 11, 21]),
            time("endTime", [2026, 10, 18, 2, 11, 55]),
            l("</alarmVideo>\n</alarmVideoList>\n</alarmVideoInfo>"),
        ]);
        let page = BcXml::from_bytes(hub.as_bytes()).unwrap().alarm_video_info.unwrap();
        assert!(page.is_finished());
        let entry = &page.entries()[0];
        assert_eq!(entry.uid.as_ref().map(CameraUid::as_str), Some(UID));
        assert!(entry.file_id.as_ref().is_some_and(RecordingId::is_empty));
        assert!(!format!("{page:?}").contains(UID));
        let again = BcXml::from_bytes(&request_body("alarmVideoInfo", &page)).unwrap().alarm_video_info.unwrap();
        assert_eq!(again, page);
    }

    #[test]
    fn cover_preview_requests_match_the_wire() {
        let preview = |bitmap: bool, start: [u32; 6], end: [u32; 6]| {
            let mut parts = vec![l("<CoverPreview version=\"1.1\">\n<channelId>0</channelId>"), format!("<uid>{UID}</uid>")];
            if bitmap {
                parts.push(l("<logicChnBitmap>255</logicChnBitmap>"));
            }
            parts.push(l("<desc>0</desc>\n<streamType>mainStream</streamType>"));
            parts.push(time("startTime", start));
            parts.push(time("endTime", end));
            parts.push(l("<frameList>\n<frameNo>1</frameNo>\n</frameList>\n</CoverPreview>"));
            body(&parts)
        };
        let hub = preview(false, [2026, 10, 16, 23, 59, 59], [2026, 10, 17, 1, 0, 0]);
        let request = CoverPreview::new(0, uid(), CivilTime::new(2026, 10, 16, 23, 59, 59), CivilTime::new(2026, 10, 17, 1, 0, 0));
        assert_eq!(text(request.to_bytes()), hub);
        assert_eq!(hub.len(), 505);
        assert_eq!(BcXml::from_bytes(hub.as_bytes()).unwrap().cover_preview, Some(request));

        let nvr = preview(true, [2026, 10, 17, 0, 22, 6], [2026, 10, 17, 0, 23, 41]);
        let request = CoverPreview {
            logic_chn_bitmap: Some(ALL_LOGIC_CHANNELS),
            ..CoverPreview::new(0, uid(), CivilTime::new(2026, 10, 17, 0, 22, 6), CivilTime::new(2026, 10, 17, 0, 23, 41))
        };
        assert_eq!(text(request.to_bytes()), nvr);
        assert_eq!(nvr.len(), 542);
    }

    #[test]
    fn channel_list_carries_each_cameras_uid_without_showing_it() {
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<ChannelInfoList version=\"1.1\">\n\
             <ChannelInfo>\n<channelId>0</channelId>\n<state>connect</state>\n<changed>0</changed>\n<uid>{UID}</uid>\n\
             <fakeUid>NVRUIDPLACEHOLDR_00</fakeUid>\n<streamSupport>mainStream,subStream</streamSupport>\n</ChannelInfo>\n\
             <ChannelInfo>\n<channelId>1</channelId>\n<state>none</state>\n<uid></uid>\n</ChannelInfo>\n\
             <ChannelInfo>\n<channelId>2</channelId>\n<state>connect</state>\n<uid></uid>\n</ChannelInfo>\n\
             </ChannelInfoList>\n</body>\n"
        );
        let parsed = BcXml::from_bytes(xml.as_bytes()).unwrap();
        assert!(!format!("{parsed:?}").contains(UID));
        let channels = parsed.channel_info_list.unwrap().into_channels();
        // The empty slot is skipped; an empty uid is no uid.
        assert_eq!(channels.iter().map(|c| c.channel_id).collect::<Vec<_>>(), vec![0, 2]);
        assert_eq!(channels[0].uid.as_ref().map(CameraUid::as_str), Some(UID));
        assert_eq!(channels[1].uid, None);
        assert!(!format!("{channels:?}").contains(UID));
    }

    #[test]
    fn extension_reads_both_spellings_of_encrypt_len() {
        let lower = b"<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<Extension version=\"1.1\">\n<binaryData>1</binaryData>\n<encryptLen>528</encryptLen>\n</Extension>\n";
        let upper = b"<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<Extension version=\"1.1\">\n<binaryData>1</binaryData>\n<EncryptLen>1024</EncryptLen>\n</Extension>\n";
        assert_eq!(Extension::from_bytes(lower).unwrap().encrypt_len, Some(528));
        let ext = Extension::from_bytes(upper).unwrap();
        assert_eq!((ext.binary_data, ext.encrypt_len), (Some(1), Some(1024)));
        let written = String::from_utf8(ext.to_bytes()).unwrap();
        assert!(written.contains("<encryptLen>1024</encryptLen>"), "{written}");
    }

    #[test]
    fn record_stream_names() {
        assert_eq!(RecordStream::from_xml_name(" subStream "), Some(RecordStream::Sub));
        assert_eq!(RecordStream::from_xml_name("mainStream"), Some(RecordStream::Main));
        assert_eq!(RecordStream::from_xml_name("externStream"), None);
        assert_eq!((RecordStream::Sub.support_sub(), RecordStream::Main.support_sub()), (1, 0));
        assert_eq!((RecordStream::Sub.event_stream_type(), RecordStream::Main.event_stream_type()), (1, 0));
    }
}
