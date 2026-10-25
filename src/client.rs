use crate::protocol::bc::model::*;
use crate::protocol::bc::xml::*;
use crate::protocol::bcmedia::model::{parse_one, BcMediaMessage};
use crate::protocol::crypto::{aes_key_from_password, EncryptionProtocol};
use crate::media_guard::{MediaGuard, Verdict};
use crate::transport::connection::BcConnection;
use crate::transport::discovery::{connect_by_uid, PeerHandle};
use crate::Error;
use md5::{Digest, Md5};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::mpsc::channel;
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;

pub use crate::protocol::bcmedia::model::{VideoFrame, VideoType};

mod replay;
pub mod settings;
pub use settings::{
    probe_channel as settings_probe_channel, CameraDetectionPage, CameraDisplayPage, CameraImagePage, CameraInfoPage,
    CameraLightSoundPage, CameraPage, CameraSchedulesPage, DeviceAlertsPage, DeviceInfoPage, DeviceKind,
    DeviceLightSoundPage, DeviceMaintenancePage, DeviceNetworkPage, DevicePage, DeviceStoragePage, DeviceTimePage,
    DeviceUsersPage, NetworkRows, Read, ReadFailure, SettingsGates, SettingsPage, SettingsTarget, SETTINGS_REPLY_TIMEOUT,
};
pub use replay::{
    file_for_time, next_file, sort_and_dedup_files, DownloadCancel, DownloadProgress, DownloadStream, DownloadSummary,
    DownloadUnit, FootageAnchor, RecordedEvent, RecordingFile, ReplayKeyframe, ReplayProgress, ReplayResume,
    ReplaySpeed, ReplayStatus, ReplayStream, ReplayUnit,
};

#[derive(Debug, Clone, Default)]
pub struct DeviceInfoSummary {
    pub resolution_name: Option<String>,
}

/// A saved PTZ preset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtzPreset {
    pub id: u8,
    pub name: String,
}

/// "Monitor Point" — a saved home position a PTZ camera can automatically
/// (or on demand) return to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MonitorPoint {
    /// Automatic return after `timeout_seconds`.
    pub enabled: bool,
    /// Whether a point is actually saved.
    pub valid: bool,
    pub timeout_seconds: u32,
}

/// What one channel's camera can do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelAbilities {
    pub siren: bool,
    pub spotlight: bool,
    /// Two-way audio.
    pub talk: bool,
    /// Motors: pan, tilt and zoom.
    pub pan: bool,
    pub tilt: bool,
    pub zoom: bool,
    /// PTZ Guard / "Monitor Point" — a saved home position the camera can
    /// return to, on demand or automatically after a timeout.
    pub monitor_point: bool,
    /// Re-calibrates the pan/tilt mechanism.
    pub calibration: bool,
    /// Has a status LED that can be switched (the settings pages' Status
    /// LED row).
    pub status_led: bool,
    /// Can follow a moving object (pan/tilt motors): the tracking rows.
    pub tracking: bool,
    /// Offers a binning mode in its picture settings.
    pub binning: bool,
    /// Calls the third smart-detection type "Pet" rather than "Animal".
    pub pet_label: bool,
}

impl ChannelAbilities {
    /// Has any motor.
    pub fn ptz(&self) -> bool {
        self.pan || self.tilt || self.zoom
    }
}

/// Something the device told us without being asked, or as the answer to a
/// request whose reader was busy streaming.
#[derive(Debug, Clone)]
pub enum DeviceUpdate {
    Channels(Vec<ChannelInfo>),
    ChannelName { channel_id: u8, name: String },
    /// What each channel's camera can do.
    Abilities(Vec<(u8, ChannelAbilities)>),
    /// Where a camera's zoom stands and how far it goes.
    ZoomFocus { channel_id: u8, zoom: Option<(u32, u32, u32)>, focus: Option<(u32, u32, u32)> },
    /// The camera's saved presets.
    Presets { channel_id: u8, presets: Vec<PtzPreset> },
    /// The camera's Monitor Point (PTZ Guard) state.
    MonitorPoint { channel_id: u8, state: MonitorPoint },
    /// Monitor Point's saved thumbnail (JPEG bytes), fully reassembled.
    MonitorPointImage { channel_id: u8, jpeg: Vec<u8> },
    /// A preset's saved thumbnail (JPEG bytes), fully reassembled.
    PresetImage { channel_id: u8, preset_id: u8, jpeg: Vec<u8> },
    /// The camera answered an image request with "no such file" (`preset_id`
    /// is `None` for Monitor Point's) — nothing to show, but the request is
    /// over, so a queue of further ones can move on.
    ImageNotFound { channel_id: u8, preset_id: Option<u8> },
    /// A fresh live picture (full size) taken for `preset_id`.
    Snapshot { channel_id: u8, preset_id: u8, jpeg: Vec<u8> },
    /// The device's answer to a control command (siren, spotlight).
    ControlReply { msg_id: u32, code: u16 },
    /// A channel's encoder settings (`None`: the device refused to say).
    Encoding { channel_id: u8, encoding: Option<crate::encoding::Encoding> },
    /// The encoder values a channel allows, asked for when the login reply's
    /// list didn't cover it (empty: the device has none to give).
    EncodeProfiles { channel_id: u8, profiles: Vec<crate::encoding::EncodeProfile> },
    /// What camera sits on a channel: its own name, model and firmware.
    ChannelVersion { channel_id: u8, name: Option<String>, model: Option<String>, firmware: Option<String> },
    /// One read-only settings page, complete (see `query_settings`).
    Settings(SettingsPage),
}

/// What the device says it is, as reported after login.
#[derive(Debug, Clone, Default)]
pub struct DeviceIdentity {
    /// The name the owner gave the device.
    pub name: Option<String>,
    /// The model, e.g. a camera, NVR or Home Hub type string.
    pub model: Option<String>,
}

/// Whether a device of this name/model is an NVR or Home Hub (several
/// cameras behind one device) rather than a single camera.
pub fn looks_multi_channel(name: &str, model: &str) -> bool {
    let text = format!("{name} {model}").to_lowercase();
    // "RLN…" are Reolink's NVR model numbers.
    text.contains("nvr") || text.contains("hub") || model.to_lowercase().starts_with("rln")
}

/// How long to wait for the device to describe itself before carrying on
/// without: the name is a nicety, the video must not wait for it.
const IDENTITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Which of the device's streams to request in `start_video`. `Main` is
/// full resolution/bitrate; `Sub` is the lowest-bitrate one; `Extern` sits
/// between them on many cameras (on some it is another lens). Which of them
/// a channel really has is up to the device — see `ChannelInfo::streams`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamProfile {
    #[default]
    Main,
    Extern,
    Sub,
}

/// One channel as the device describes it (NVR / Home Hub push after login).
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub channel_id: u8,
    pub name: String,
    pub online: bool,
    /// The streams the channel offers, highest quality first.
    pub streams: Vec<StreamProfile>,
    /// The camera's UID as the channel list gives it, which the recorded
    /// video requests name the camera by. `None` when the list has none
    /// (a stand-alone camera sends no list at all). Never log it: its
    /// `Debug` is masked and it has no `Display`.
    pub uid: Option<CameraUid>,
}

/// Mirrors `transport::discovery`'s established pattern of bounding every
/// network wait explicitly (see `OVERALL_TIMEOUT`/`DNS_TIMEOUT` there) — a
/// silently-unreachable IP (the most common user mistake: right subnet,
/// wrong last octet) would otherwise hang on the OS's own SYN timeout,
/// commonly over a minute, with the connect dialog frozen on "Connecting...".
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct ReolinkClient {
    connection: Arc<Mutex<BcConnection>>,
    encryption: EncryptionProtocol,
    next_msg_num: u16,
    video_task: Option<tokio::task::JoinHandle<()>>,
    /// The message number all audio blocks of the current talk share.
    talk_msg_num: u16,
    /// The stream `start_video` opened, so `stop_video` can name it.
    playing: Option<PlayingStream>,
    /// Keeps a direct (non-relay) connection alive — see
    /// `BcConnection::spawn_direct_keepalive`. `None` on a relay connection
    /// or one built via `from_connection`. Held so the handle isn't
    /// dropped (which would just detach the task, not stop it — it keeps
    /// running for the process's lifetime regardless); never polled
    /// directly.
    direct_keepalive_task: Option<tokio::task::JoinHandle<()>>,
    /// Channel lists the device pushes (NVR / Home Hub) whenever it likes —
    /// during `identity`, while starting video, or mid-stream.
    channel_updates_tx: tokio::sync::mpsc::UnboundedSender<DeviceUpdate>,
    channel_updates_rx: Option<tokio::sync::mpsc::UnboundedReceiver<DeviceUpdate>>,
    /// AAC (ADTS) frames of the running stream's audio.
    audio_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    audio_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    /// What each in-flight request (keyed by its `msg_num`) is for, so its
    /// reply — read by whichever task happens to be reading the connection
    /// when it arrives, not necessarily the one that sent the request —
    /// knows which `DeviceUpdate` to emit: the `MSG_ID_IMAGE_FILE` requests
    /// it was made for, and the settings reads (`ImageKind::SettingsPage`).
    /// `Arc` so the video task's own read loop (a `'static` spawned task
    /// with no `&mut self`) can share it too. (The name predates the
    /// settings reads; renaming it touches the playback readers, so that
    /// waits for a quiet moment there.)
    pending_images: Arc<std::sync::Mutex<HashMap<u16, ImageKind>>>,
    /// The settings page last asked for per target, for replacing a read
    /// that is still pending and for the probe.
    settings_pages: HashMap<SettingsTarget, Arc<std::sync::Mutex<settings::PageCollector>>>,
    /// The encoder values the device allows, from its login reply.
    encode_profiles: Vec<crate::encoding::EncodeProfile>,
    /// Each channel's camera UID, from the pushed channel list (shared with
    /// the video tasks, which see that push too). Never logged.
    channel_uids: Arc<std::sync::Mutex<HashMap<u8, CameraUid>>>,
    /// The running recorded-video playback, if any (see `replay`).
    replay: Option<replay::ActiveReplay>,
    replay_task: Option<tokio::task::JoinHandle<()>>,
    /// Playbacks opened on this connection, for the sequence numbers.
    replay_opens: u32,
    last_replay_seq: Option<u32>,
    /// The device left the start of an event-window search unanswered: it
    /// is not asked again on this connection (see `search_events`).
    event_search_unanswered: bool,
    /// The running download's reader and cancel handle, if any (see
    /// `download_range`).
    download_task: Option<tokio::task::JoinHandle<replay::DownloadSummary>>,
    download_cancel: Option<replay::DownloadCancel>,
}

/// What an in-flight request was asking for: a `MSG_ID_IMAGE_FILE` request
/// (see its own doc comment), or one read of a settings page.
#[derive(Debug, Clone)]
enum ImageKind {
    MonitorPoint,
    Preset(u8),
    /// A live snapshot taken for this preset (Add / Edit's picture).
    Snapshot(u8),
    /// One read of a settings page; the collector gathers the page's
    /// replies (see `client::settings`).
    SettingsPage(Arc<std::sync::Mutex<settings::PageCollector>>),
}

#[derive(Debug, Clone)]
struct PlayingStream {
    channel_id: u8,
    handle: u32,
    stream_name: &'static str,
    bc_stream_type: u8,
}

fn control_xml(inner: &str) -> Vec<u8> {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n{inner}</body>\n").into_bytes()
}

/// The siren command exactly as the official app sends it (same length, as
/// captured: 223 bytes): one play, not a switch.
fn siren_xml(channel_id: u8) -> Vec<u8> {
    control_xml(&format!(
        "<audioPlayInfo version=\"1.1\">\n<channelId>{channel_id}</channelId>\n<playMode>0</playMode>\n<playDuration>0</playDuration>\n<playTimes>1</playTimes>\n<onOff>0</onOff>\n</audioPlayInfo>\n"
    ))
}

/// A pan/tilt command as the official app sends it (speed 32 to move, 0 to
/// stop; lengths 163/164 and 162 as captured).
fn ptz_xml(channel_id: u8, command: &str, speed: u8) -> Vec<u8> {
    control_xml(&format!(
        "<PtzControl version=\"1.1\">\n<channelId>{channel_id}</channelId>\n<speed>{speed}</speed>\n<command>{command}</command>\n</PtzControl>\n"
    ))
}

/// The talk configuration as the official app sends it (394 bytes, as
/// captured): follow the video stream's audio mode, ADPCM 16 kHz mono.
fn talk_config_xml(channel_id: u8) -> Vec<u8> {
    control_xml(&format!(
        "<TalkConfig version=\"1.1\">\n<channelId>{channel_id}</channelId>\n<duplex>FDX</duplex>\n<audioStreamMode>followVideoStream</audioStreamMode>\n<audioConfig>\n<audioType>adpcm</audioType>\n<sampleRate>16000</sampleRate>\n<samplePrecision>16</samplePrecision>\n<lengthPerEncoder>1024</lengthPerEncoder>\n<soundTrack>mono</soundTrack>\n</audioConfig>\n</TalkConfig>\n"
    ))
}

/// Sets the zoom position, as the official app does (`zoomPos`; lengths 177
/// to 179 bytes as captured for positions of one to three digits).
fn zoom_xml(channel_id: u8, position: u32) -> Vec<u8> {
    control_xml(&format!(
        "<StartZoomFocus version=\"1.1\">\n<channelId>{channel_id}</channelId>\n<command>zoomPos</command>\n<movePos>{position}</movePos>\n</StartZoomFocus>\n"
    ))
}

fn focus_xml(channel_id: u8, position: u32) -> Vec<u8> {
    control_xml(&format!(
        "<StartZoomFocus version=\"1.1\">\n<channelId>{channel_id}</channelId>\n<command>focusPos</command>\n<movePos>{position}</movePos>\n</StartZoomFocus>\n"
    ))
}

/// A PTZ preset command (`setPos`/`toPos`/`delPos`) in the short shape
/// (`PtzPreset` > `presetList` > `preset`, no position fields).
fn preset_xml(channel_id: u8, id: u8, name: Option<&str>, command: &str) -> Vec<u8> {
    let name_tag = name.map(|n| format!("<name>{}</name>", crate::protocol::bc::xml::xml_escape(n))).unwrap_or_default();
    control_xml(&format!(
        "<PtzPreset version=\"1.1\">\n<channelId>{channel_id}</channelId>\n<presetList>\n<preset><id>{id}</id>{name_tag}<command>{command}</command></preset>\n</presetList>\n</PtzPreset>\n"
    ))
}

/// A PTZ preset message in the full shape the official app sends
/// (position fields and all, zeroed) — `command` `None` is a plain edit.
fn preset_full_xml(channel_id: u8, id: u8, command: Option<&str>, name: &str, with_image: bool) -> Vec<u8> {
    let command = command.map(|c| format!("<command>{c}</command>")).unwrap_or_default();
    let name = crate::protocol::bc::xml::xml_escape(name);
    let image = if with_image { format!("<imageName>preset_{id:02}</imageName>") } else { String::new() };
    control_xml(&format!(
        "<PtzPreset version=\"1.1\">\n<channelId>{channel_id}</channelId>\n<presetList>\n<preset><id>{id}</id>{command}<name>{name}</name>{image}<xpos>0.000000e+00</xpos><ypos>0.000000e+00</ypos><height>0.000000e+00</height><width>0.000000e+00</width><mode>global</mode></preset>\n</presetList>\n</PtzPreset>\n"
    ))
}

/// Extension for reading Monitor Point (332): unlike every other control
/// message here, the READ extension carries `chnType` — the WRITE one
/// (331) doesn't. Confirmed against a real capture (see
/// `.plans/reolink-baichuan-calibration-monitor-point.md` section 7); this
/// asymmetry is why this isn't built from the shared `Extension` struct.
fn monitor_point_read_extension(channel_id: u8) -> Vec<u8> {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<Extension version=\"1.1\">\n<channelId>{channel_id}</channelId>\n<chnType>0</chnType>\n</Extension>\n"
    )
    .into_bytes()
}

/// Requests the named image file, exactly as captured for Monitor Point's
/// own thumbnail (`name = "guard"`).
fn image_file_read_xml(channel_id: u8, name: &str) -> Vec<u8> {
    control_xml(&format!(
        "<imageFileInfo version=\"1.1\">\n<channelId>{channel_id}</channelId>\n<imageName>{name}</imageName>\n</imageFileInfo>\n"
    ))
}

/// A Monitor Point (`PtzGuard`) write: `setGrd` (configure — add
/// `needSetPos` to also save the camera's current position as the point)
/// or `toGrd` (move to it now). `xpos`/`ypos`/`height`/`width` are written
/// as `0.000000e+00`, not `0` — some firmware's parser is strict about it
/// (same source as above, section 13).
fn monitor_point_xml(channel_id: u8, enabled: bool, timeout: u32, command: &str, set_position: bool) -> Vec<u8> {
    let need_set_pos = if set_position { "<needSetPos>1</needSetPos>\n" } else { "" };
    control_xml(&format!(
        "<PtzGuard version=\"1.1\">\n<channelId>{channel_id}</channelId>\n<benable>{}</benable>\n<timeout>{timeout}</timeout>\n{need_set_pos}<command>{command}</command>\n<imageName></imageName>\n<xpos>0.000000e+00</xpos>\n<ypos>0.000000e+00</ypos>\n<height>0.000000e+00</height>\n<width>0.000000e+00</width>\n<mode>global</mode>\n</PtzGuard>\n",
        u8::from(enabled)
    ))
}

/// The spotlight command: on or off, with the 180 s duration both of the
/// official app's messages carried (177 bytes each, as captured).
fn spotlight_xml(channel_id: u8, on: bool) -> Vec<u8> {
    control_xml(&format!(
        "<FloodlightManual version=\"1.1\">\n<channelId>{channel_id}</channelId>\n<status>{}</status>\n<duration>180</duration>\n</FloodlightManual>\n",
        u8::from(on)
    ))
}

/// What `bc` tells us, if it is a pushed channel list or the answer to a
/// channel-name request. `image_chunks` reassembles a multi-message image
/// download (see `MSG_ID_IMAGE_FILE`'s doc comment) — callers keep one map
/// alive for as long as they keep reading from the same connection, the
/// same way `read_bc`'s own `bin_mode` is kept alive across calls.
/// `pending_images` says which request (Monitor Point, or which preset)
/// each in-flight download answers — shared (not loop-local, unlike
/// `image_chunks`) because the request that populates it can be sent from a
/// completely different task than the one that ends up reading the reply.
/// Replies to the encoder and channel-version requests (56, 730, 318).
fn encoding_update(bc: &Bc) -> Option<DeviceUpdate> {
    let id = bc.meta.msg_id;
    if id != MSG_ID_GET_COMPRESSION && id != MSG_ID_CHANNEL_ENCODE_TABLE && id != MSG_ID_CHANNEL_VERSION {
        return None;
    }
    let channel_id = bc.meta.channel_id;
    // 300 is how these devices answer a complete list; 200 otherwise.
    let ok = bc.meta.response_code == 200 || bc.meta.response_code == 300;
    let body = match &bc.body {
        BcBody::Modern(ModernMsg { payload: Some(p), .. }) if ok => String::from_utf8_lossy(p).into_owned(),
        _ => String::new(),
    };
    if std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
        eprintln!("DEBUG encoding reply: msg={id} channel={channel_id} code={} bytes={}", bc.meta.response_code, body.len());
    }
    Some(match id {
        MSG_ID_GET_COMPRESSION => {
            let encoding = crate::encoding::Encoding::parse(&body);
            DeviceUpdate::Encoding { channel_id: encoding.as_ref().map_or(channel_id, |e| e.channel_id), encoding }
        }
        MSG_ID_CHANNEL_ENCODE_TABLE => {
            DeviceUpdate::EncodeProfiles { channel_id, profiles: crate::encoding::EncodeProfile::parse_list(&body) }
        }
        _ => {
            let info = BcXml::from_bytes(body.as_bytes()).ok().and_then(|x| x.version_info);
            DeviceUpdate::ChannelVersion {
                channel_id: info.as_ref().and_then(|i| i.channel_id).unwrap_or(channel_id),
                name: info.as_ref().and_then(|i| i.name.clone()),
                model: info.as_ref().and_then(|i| i.model.clone()),
                firmware: info.and_then(|i| i.firmware_version),
            }
        }
    })
}

/// `channel_uids` keeps each channel's camera UID from the pushed channel
/// list, for the recorded-video requests that name the camera by it.
fn pushed_update(
    bc: &Bc,
    image_chunks: &mut HashMap<u16, Vec<u8>>,
    pending_images: &std::sync::Mutex<HashMap<u16, ImageKind>>,
    channel_uids: &std::sync::Mutex<HashMap<u8, CameraUid>>,
) -> Option<DeviceUpdate> {
    // A reply to one of a settings page's reads goes to its page and
    // nowhere else (its id would mean something else below); nothing else
    // is routed by message number.
    match settings::settings_reply(bc, pending_images) {
        settings::Routed::Consumed(update) => return update,
        settings::Routed::NotOurs => {}
    }
    if bc.meta.msg_id == MSG_ID_IMAGE_FILE || bc.meta.msg_id == MSG_ID_SNAP {
        if bc.meta.response_code == 0xffff {
            // The device's answer for "no file by that name" (e.g. a
            // preset Reoling itself saved, which never got a thumbnail
            // from the official app) — confirmed 2026-09-23 against a real
            // capture; `read_bc` already strips the 4 stray bytes this
            // reply carries beyond its own declared body length. Nothing
            // to reassemble; just free the slot for the next request.
            let kind = pending_images.lock().expect("pending_images mutex poisoned").remove(&bc.meta.msg_num);
            let preset_id = match kind {
                Some(ImageKind::Preset(id)) => Some(id),
                _ => None,
            };
            return Some(DeviceUpdate::ImageNotFound { channel_id: bc.meta.channel_id, preset_id });
        }
        let BcBody::Modern(ModernMsg { extension_xml, payload }) = &bc.body else {
            return None;
        };
        let is_chunk = extension_xml
            .as_deref()
            .and_then(|e| Extension::from_bytes(e).ok())
            .and_then(|e| e.binary_data)
            == Some(1);
        if !is_chunk {
            // The small metadata reply that precedes the chunks; nothing
            // this project reads from it yet.
            return None;
        }
        if bc.meta.response_code == 200 {
            if let Some(payload) = payload {
                image_chunks.entry(bc.meta.msg_num).or_default().extend_from_slice(payload);
            }
            return None; // more to come
        }
        // Any other code (201, seen in a capture) ends the transfer.
        let jpeg = image_chunks.remove(&bc.meta.msg_num).unwrap_or_default();
        if jpeg.is_empty() {
            return None;
        }
        let kind = pending_images.lock().expect("pending_images mutex poisoned").remove(&bc.meta.msg_num);
        return match kind {
            Some(ImageKind::Snapshot(preset_id)) => {
                Some(DeviceUpdate::Snapshot { channel_id: bc.meta.channel_id, preset_id, jpeg })
            }
            _ if bc.meta.msg_id == MSG_ID_SNAP => None,
            Some(ImageKind::Preset(preset_id)) => {
                Some(DeviceUpdate::PresetImage { channel_id: bc.meta.channel_id, preset_id, jpeg })
            }
            // Monitor Point either by request, or as a fallback for a
            // reply whose own request this client did not see (unexpected,
            // but showing Monitor Point's picture is a safer default than
            // silently dropping a downloaded image).
            _ => Some(DeviceUpdate::MonitorPointImage { channel_id: bc.meta.channel_id, jpeg }),
        };
    }
    if let Some(update) = encoding_update(bc) {
        return Some(update);
    }
    if bc.meta.msg_id == MSG_ID_SET_COMPRESSION && std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
        eprintln!("DEBUG encoding write reply: channel={} code={}", bc.meta.channel_id, bc.meta.response_code);
    }
    if bc.meta.msg_id == MSG_ID_PLAY_SIREN
        || bc.meta.msg_id == MSG_ID_SPOTLIGHT
        || bc.meta.msg_id == MSG_ID_SET_COMPRESSION
        || bc.meta.msg_id == MSG_ID_PTZ
        || bc.meta.msg_id == MSG_ID_PTZ_PRESET
        || bc.meta.msg_id == MSG_ID_SET_ZOOM_FOCUS
        || bc.meta.msg_id == MSG_ID_TALK_CONFIG
        || bc.meta.msg_id == MSG_ID_PTZ_GUARD
        || bc.meta.msg_id == MSG_ID_PTZ_CALIBRATE
    {
        return Some(DeviceUpdate::ControlReply {
            msg_id: bc.meta.msg_id,
            code: bc.meta.response_code,
        });
    }
    if bc.meta.msg_id != MSG_ID_CHANNEL_INFO
        && bc.meta.msg_id != MSG_ID_OSD
        && bc.meta.msg_id != MSG_ID_SUPPORT
        && bc.meta.msg_id != MSG_ID_GET_ZOOM_FOCUS
        && bc.meta.msg_id != MSG_ID_GET_PTZ_PRESET
        && bc.meta.msg_id != MSG_ID_GET_PTZ_GUARD
    {
        return None;
    }
    if bc.meta.msg_id == MSG_ID_OSD && std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
        let body = match &bc.body {
            BcBody::Modern(ModernMsg { payload: Some(p), .. }) => String::from_utf8_lossy(p)
                .chars()
                .take(1500)
                .map(|c| if c == '\n' { ' ' } else { c })
                .collect::<String>(),
            other => format!("{other:?}"),
        };
        eprintln!(
            "DEBUG osd reply: channel={} code={} body={body}",
            bc.meta.channel_id, bc.meta.response_code
        );
    }
    let BcBody::Modern(ModernMsg { extension_xml, payload: Some(payload) }) = &bc.body else {
        return None;
    };
    let xml = BcXml::from_bytes(payload).ok()?;
    if let Some(preset) = xml.ptz_preset {
        let channel_id = preset.channel_id.unwrap_or(bc.meta.channel_id);
        return Some(DeviceUpdate::Presets { channel_id, presets: preset.into_presets() });
    }
    if let Some(guard) = xml.ptz_guard {
        let channel_id = guard.channel_id.unwrap_or(bc.meta.channel_id);
        return Some(DeviceUpdate::MonitorPoint { channel_id, state: guard.into_monitor_point() });
    }
    if let Some(zf) = xml.ptz_zoom_focus {
        // (min, max, current)
        let range = |r: Option<crate::protocol::bc::xml::PositionRange>| {
            r.map(|r| (r.min_pos.unwrap_or(0), r.max_pos.unwrap_or(0), r.cur_pos.unwrap_or(0)))
        };
        return Some(DeviceUpdate::ZoomFocus {
            channel_id: zf.channel_id.unwrap_or(bc.meta.channel_id),
            zoom: range(zf.zoom),
            focus: range(zf.focus),
        });
    }
    if let Some(support) = xml.support {
        let abilities = support.into_abilities();
        if std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
            eprintln!("DEBUG abilities: {abilities:?}");
        }
        return Some(DeviceUpdate::Abilities(abilities));
    }
    if let Some(list) = xml.channel_info_list {
        let channels = list.into_channels();
        remember_channel_uids(channel_uids, &channels);
        if std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
            eprintln!("DEBUG channel list pushed: {channels:?}");
        }
        return Some(DeviceUpdate::Channels(channels));
    }
    let name = xml.osd_channel_name?.name?.trim().to_string();
    let channel_id = extension_xml
        .as_deref()
        .and_then(|e| Extension::from_bytes(e).ok())
        .and_then(|e| e.channel_id)
        .unwrap_or(bc.meta.channel_id);
    Some(DeviceUpdate::ChannelName { channel_id, name })
}

/// Stores the camera UID of every listed channel, and forgets the one of a
/// listed channel that no longer has any.
fn remember_channel_uids(channel_uids: &std::sync::Mutex<HashMap<u8, CameraUid>>, channels: &[ChannelInfo]) {
    let mut uids = channel_uids.lock().expect("channel_uids mutex poisoned");
    for channel in channels {
        match &channel.uid {
            Some(uid) => {
                uids.insert(channel.channel_id, uid.clone());
            }
            None => {
                uids.remove(&channel.channel_id);
            }
        }
    }
}

/// `text` with the contents of every element that can hold a camera or
/// device UID replaced, for diagnostic output: `<uid>`, `<fakeUid>`, a
/// recording's `<Id>` and `<fileId>` (a Home Hub's ends with the camera
/// UID).
fn redact_identifiers(text: &str) -> String {
    let mut out = text.to_string();
    for tag in ["uid", "fakeUid", "UID", "Id", "fileId"] {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let mut from = 0;
        while let Some(start) = out[from..].find(&open).map(|i| from + i + open.len()) {
            let Some(len) = out[start..].find(&close) else { break };
            out.replace_range(start..start + len, "<hidden>");
            from = start + "<hidden>".len() + close.len();
        }
    }
    out
}

/// The camera truncates the hex MD5 digest of `input` to 31 characters
/// (uppercase) before comparing it — a quirk inherited from a fixed-size C
/// buffer in the original firmware. Both username and password must be
/// hashed this way for the modern login step.
fn md5_hex_truncated(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let hex = digest.iter().map(|b| format!("{b:02X}")).collect::<String>();
    hex[..31].to_string()
}

impl ReolinkClient {
    /// Resolves `uid` over Reolink's P2P infrastructure and opens the data
    /// connection. Does not log in yet — call `login` next.
    ///
    /// Races every connect candidate the register server offers (LAN-local
    /// direct, NAT-external "map", and relay) simultaneously and uses
    /// whichever answers first — see `transport::discovery::connect_race`
    /// for the real-capture evidence this mirrors exactly, and
    /// `transport::discovery::connect_by_uid`'s doc comment for how this
    /// replaced an earlier, disproven "skip direct whenever relay is
    /// available" strategy. Still strictly UDP/P2P — no TCP.
    pub async fn connect_by_uid(uid: &str) -> crate::Result<Self> {
        let socket = Arc::new(crate::transport::bind_udp_socket_with_large_rcvbuf().await?);
        let peer: PeerHandle = connect_by_uid(&socket, uid).await?;
        let connection = BcConnection::new(socket, peer);
        let keepalive = connection.spawn_direct_keepalive();
        let mut client = Self::from_connection(connection, EncryptionProtocol::Unencrypted);
        client.direct_keepalive_task = keepalive;
        Ok(client)
    }

    /// Diagnostic-only, not used by the app: identical to `connect_by_uid`,
    /// but prefers the direct P2P path over relay whenever the register
    /// server gave us any device address at all. See
    /// `transport::discovery::connect_by_uid_prefer_direct`.
    pub async fn connect_by_uid_prefer_direct(uid: &str) -> crate::Result<Self> {
        let socket = Arc::new(crate::transport::bind_udp_socket_with_large_rcvbuf().await?);
        let peer: PeerHandle =
            crate::transport::discovery::connect_by_uid_prefer_direct(&socket, uid).await?;
        let connection = BcConnection::new(socket, peer);
        let keepalive = connection.spawn_direct_keepalive();
        let mut client = Self::from_connection(connection, EncryptionProtocol::Unencrypted);
        client.direct_keepalive_task = keepalive;
        Ok(client)
    }

    /// Diagnostic-only, never the app's default — see
    /// `project-udp-only-scope`: the official Reolink app never uses TCP
    /// for Baichuan except as an extreme last resort, and this project
    /// only targets UDP/P2P. Resolves `uid` the same way
    /// `connect_by_uid_prefer_direct` does (bounded to a short timeout for
    /// the direct attempt specifically — see
    /// `transport::discovery::connect_by_uid_prefer_direct_bounded`), but
    /// when that resolution reaches the device directly (not through
    /// relay), continues the session over TCP:9000 (`connect_by_ip`)
    /// instead of the UDP/P2P transport, purely to isolate whether a given
    /// symptom is transport-layer or not. `.plans/reolink-protocols.md`'s
    /// claim that Baichuan TCP is the NVR/powered-camera family's
    /// "primary" protocol is wrong (unverified, contradicted by real capture
    /// evidence) — don't cite it. Falls back
    /// to the UDP session if the device only resolved via relay, or if the
    /// direct TCP connect itself fails.
    pub async fn connect_by_uid_prefer_tcp(uid: &str) -> crate::Result<Self> {
        let socket = Arc::new(crate::transport::bind_udp_socket_with_large_rcvbuf().await?);
        let peer: PeerHandle =
            crate::transport::discovery::connect_by_uid_prefer_direct_bounded(&socket, uid)
                .await?;
        if peer.is_direct {
            const BASIC_SERVICE_PORT: u16 = 9000;
            if let Ok(client) = Self::connect_by_ip(peer.addr.ip(), BASIC_SERVICE_PORT).await {
                return Ok(client);
            }
        }
        let connection = BcConnection::new(socket, peer);
        let keepalive = connection.spawn_direct_keepalive();
        let mut client = Self::from_connection(connection, EncryptionProtocol::Unencrypted);
        client.direct_keepalive_task = keepalive;
        Ok(client)
    }

    /// Connects directly to a device's Baichuan "Basic Service" TCP port
    /// (typically 9000) by IP — the explicit alternative to
    /// `connect_by_uid`'s P2P path, chosen by the user, never a fallback.
    /// No P2P handshake, no relay, no direct-connect keepalive (`C2D_HB`
    /// has no meaning on a stable TCP connection — see
    /// `BcConnection::spawn_direct_keepalive`).
    pub async fn connect_by_ip(ip: std::net::IpAddr, port: u16) -> crate::Result<Self> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((ip, port)))
            .await
            .map_err(|_| Error::ProtocolError(format!("connecting to {ip}:{port} timed out")))?
            .map_err(|e| Error::ProtocolError(format!("could not connect to {ip}:{port}: {e}")))?;
        let connection = BcConnection::from_tcp(stream);
        Ok(Self::from_connection(connection, EncryptionProtocol::Unencrypted))
    }

    /// Test/advanced entry point: wraps an already-established
    /// `BcConnection` (used directly in tests to skip P2P discovery, which
    /// is covered separately in `transport::discovery`'s own tests).
    pub fn from_connection(connection: BcConnection, encryption: EncryptionProtocol) -> Self {
        let (channel_updates_tx, channel_updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let (audio_tx, audio_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            connection: Arc::new(Mutex::new(connection)),
            encryption,
            next_msg_num: 1,
            video_task: None,
            playing: None,
            talk_msg_num: 0,
            direct_keepalive_task: None,
            channel_updates_tx,
            channel_updates_rx: Some(channel_updates_rx),
            audio_tx,
            audio_rx: Some(audio_rx),
            pending_images: Arc::new(std::sync::Mutex::new(HashMap::new())),
            settings_pages: HashMap::new(),
            encode_profiles: Vec::new(),
            channel_uids: Arc::new(std::sync::Mutex::new(HashMap::new())),
            replay: None,
            replay_task: None,
            replay_opens: 0,
            last_replay_seq: None,
            event_search_unanswered: false,
            download_task: None,
            download_cancel: None,
        }
    }

    /// The camera UID the channel list gave for `channel_id`, if any.
    pub fn channel_uid(&self, channel_id: u8) -> Option<CameraUid> {
        self.channel_uids.lock().expect("channel_uids mutex poisoned").get(&channel_id).cloned()
    }

    /// The live stream's audio, as AAC (ADTS) frames (a playback's audio
    /// comes with its video, see `ReplayStream::units`). Can be taken once.
    pub fn take_audio(&mut self) -> Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>> {
        self.audio_rx.take()
    }

    /// The channel lists this device pushes, as they arrive. Can be taken
    /// once.
    pub fn take_channel_updates(
        &mut self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<DeviceUpdate>> {
        self.channel_updates_rx.take()
    }

    /// Whether the P2P handshake delivered a relay login nonce (see
    /// `PeerHandle::nonce`) — exposed for diagnostics only, to tell apart
    /// "no nonce was available, `login` used the legacy fallback" from
    /// "a nonce was available and something else went wrong".
    pub async fn has_relay_nonce(&self) -> bool {
        self.connection.lock().await.peer_nonce().is_some()
    }

    /// Diagnostic-only: which address this connection is actually talking
    /// to — lets a caller tell a direct connection apart from a relay one.
    pub async fn peer_addr(&self) -> std::net::SocketAddr {
        self.connection.lock().await.peer_addr()
    }

    fn next_msg_num(&mut self) -> u16 {
        let n = self.next_msg_num;
        self.next_msg_num = self.next_msg_num.wrapping_add(1);
        // The number comes round again after 65536 requests: an unanswered
        // settings read left under it must not take this request's reply.
        settings::number_reused(&self.pending_images, n);
        n
    }

    /// Login is a host-level operation, not a per-channel one — confirmed
    /// 2026-09-13 against a real TCP:9000 capture of the official Windows
    /// client logging into a Home Hub Pro NVR
    /// (`.plans/docu/Wireshark/Reolink-ALL-Login_only_with_Home_Hub_Pro.pcapng`):
    /// both the legacy `LoginUpgrade` and the modern `LoginUser` request
    /// carry `channel_id: 0` in their `BcMeta`, even though the session
    /// goes on to stream a non-zero camera channel afterward. An earlier
    /// version of this code threaded the target camera's channel into
    /// login's `BcMeta` too (see bug #13/#14 in `.plans/reoling-project.md`)
    /// — real capture evidence for the exact device class this project
    /// targets overrides that assumption. `channel_id` is passed only to
    /// `start_video`/`stop_video`, which is where the real client's own
    /// per-channel routing (`Extension`/`channelId` XML, wire `channel+1`)
    /// actually happens.
    pub async fn login(
        &mut self,
        username: &str,
        password: &str,
    ) -> crate::Result<DeviceInfoSummary> {
        let channel_id = 0;
        let msg_num = self.next_msg_num();

        let relay_nonce = self.connection.lock().await.peer_nonce().map(str::to_string);
        let nonce = if let Some(nonce) = relay_nonce {
            // Confirmed 2026-09-13 against a real capture: over the relay
            // path, the real client never performs the legacy
            // login/nonce-reply BC exchange below at all — the nonce
            // already arrived in the relay handshake's D2C_CFM (see
            // `PeerHandle::nonce`). Sending the legacy step anyway is what
            // was causing the device to tear the session down with
            // D2C_DISC right after connecting.
            nonce
        } else {
            let legacy_login = Bc {
                meta: BcMeta {
                    msg_id: MSG_ID_LOGIN,
                    channel_id,
                    stream_type: 0,
                    msg_num,
                    // 0xdc02 was our own invention (bug #12): we assumed
                    // it meant "control-channel-only AES", to keep the
                    // video feed unencrypted for our GStreamer pipeline
                    // (which expects raw H.264). That premise was wrong on
                    // two counts, 2026-09-13: (1) 0xdc02 isn't a real,
                    // recognized value — a real NVR (Hub Pro) was observed
                    // transport-ACKing this exact legacy login message and
                    // then immediately `D2C_DISC`-ing with no
                    // application-level reply at all, on both the direct
                    // and relay paths, across every other field we varied
                    // — consistent with the device rejecting an
                    // unrecognized request outright rather than degrading
                    // it; (2) asking for less than full AES was never
                    // needed: under AES the camera encrypts only the leading
                    // `encryptLen` bytes of each media unit, which `read_bc`
                    // decrypts before GStreamer sees them (2026-09-14), so
                    // live video plays after a 0xdc12 login. Now requesting
                    // 0xdc12 (Aes), the value in our captured login (see
                    // codec.rs's `legacy_login_upgrade_matches_a_real_captured_request_byte_for_byte`).
                    response_code: 0xdc12,
                    class: 0x6514,
                },
                body: BcBody::Legacy(LegacyMsg::LoginUpgrade),
            };
            self.connection.lock().await.send_bc(&legacy_login, &self.encryption).await?;
            let reply = self.connection.lock().await.recv_bc(&self.encryption).await?;
            let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = reply.body else {
                return Err(Error::ProtocolError("expected an Encryption reply".to_string()));
            };
            BcXml::from_bytes(&payload)?
                .encryption
                .ok_or_else(|| Error::ProtocolError("missing nonce in Encryption reply".to_string()))?
                .nonce
        };

        self.encryption = EncryptionProtocol::Aes {
            key: aes_key_from_password(password, &nonce),
        };

        let modern_login = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id,
                stream_type: 0,
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(
                    BcXml {
                        login_user: Some(LoginUser {
                            version: XML_VERSION.to_string(),
                            user_name: md5_hex_truncated(&format!("{username}{nonce}")),
                            password: md5_hex_truncated(&format!("{password}{nonce}")),
                            user_ver: 1,
                        }),
                        login_net: Some(LoginNet::default()),
                        ..Default::default()
                    }
                    .to_bytes(),
                ),
            }),
        };
        self.connection.lock().await.send_bc(&modern_login, &self.encryption).await?;
        let reply = self.connection.lock().await.recv_bc(&self.encryption).await?;
        if reply.meta.response_code != 200 {
            return Err(Error::LoginFailed { code: reply.meta.response_code });
        }
        let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = reply.body else {
            return Err(Error::ProtocolError("expected a DeviceInfo reply".to_string()));
        };
        if std::env::var("REOLING_PROBE_PUSH").is_ok() {
            let text: String = redact_identifiers(&String::from_utf8_lossy(&payload)).chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
            eprintln!("PROBE login reply: {text}");
        }
        self.encode_profiles = crate::encoding::EncodeProfile::parse_list(&String::from_utf8_lossy(&payload));
        let device_info = BcXml::from_bytes(&payload)?.device_info.unwrap_or_default();
        Ok(DeviceInfoSummary {
            resolution_name: device_info.resolution.and_then(|r| r.name),
        })
    }

    /// A command about one channel: the channel goes in the extension, the
    /// command itself in the payload. The answer arrives as a
    /// `DeviceUpdate::ControlReply`.
    async fn send_control(&mut self, msg_id: u32, channel_id: u8, xml: Vec<u8>) -> crate::Result<()> {
        let msg_num = self.next_msg_num();
        let extension = Extension {
            version: XML_VERSION.to_string(),
            channel_id: Some(channel_id),
            ..Default::default()
        };
        let request = Bc {
            meta: BcMeta {
                msg_id,
                channel_id,
                stream_type: 0,
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: Some(extension.to_bytes()),
                payload: Some(xml),
            }),
        };
        self.connection.lock().await.send_bc(&request, &self.encryption).await
    }

    /// Like `send_control`, but with no payload at all (e.g. calibration,
    /// which the official app sends with only the channel extension).
    async fn send_control_no_payload(&mut self, msg_id: u32, channel_id: u8) -> crate::Result<()> {
        let msg_num = self.next_msg_num();
        let extension = Extension {
            version: XML_VERSION.to_string(),
            channel_id: Some(channel_id),
            ..Default::default()
        };
        let request = Bc {
            meta: BcMeta {
                msg_id,
                channel_id,
                stream_type: 0,
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg { extension_xml: Some(extension.to_bytes()), payload: None }),
        };
        self.connection.lock().await.send_bc(&request, &self.encryption).await
    }

    /// Plays the camera's siren once.
    pub async fn play_siren(&mut self, channel_id: u8) -> crate::Result<()> {
        self.send_control(MSG_ID_PLAY_SIREN, channel_id, siren_xml(channel_id)).await
    }

    /// Starts moving the camera (`left`, `right`, `up`, `down`, `leftUp`,
    /// `leftDown`, `rightUp`, `rightDown`) or, with `stop`, stops it. It
    /// moves until told to stop.
    pub async fn ptz(&mut self, channel_id: u8, command: &str, speed: u8) -> crate::Result<()> {
        let asked = std::time::Instant::now();
        let result = self.send_control(MSG_ID_PTZ, channel_id, ptz_xml(channel_id, command, speed)).await;
        if std::env::var("REOLING_DEBUG_PTZ").is_ok() {
            let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            eprintln!("PTZ {command} sent at {at:?}, waited {:?} for the connection", asked.elapsed());
        }
        result
    }

    /// Re-calibrates the pan/tilt mechanism. A successful reply means the
    /// camera accepted the request, not that the (mechanical, several-second)
    /// calibration has finished — there is no separate "done" message.
    pub async fn calibrate_ptz(&mut self, channel_id: u8) -> crate::Result<()> {
        self.send_control_no_payload(MSG_ID_PTZ_CALIBRATE, channel_id).await
    }

    /// Asks for the camera's Monitor Point (the answer arrives as
    /// `DeviceUpdate::MonitorPoint`). The read extension differs from every
    /// other control message's (see `monitor_point_read_extension`), so
    /// this bypasses `request_for_channel`.
    pub async fn query_monitor_point(&mut self, channel_id: u8) {
        let msg_num = self.next_msg_num();
        let request = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_GET_PTZ_GUARD,
                channel_id,
                stream_type: 0,
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: Some(monitor_point_read_extension(channel_id)),
                payload: None,
            }),
        };
        let _ = self.connection.lock().await.send_bc(&request, &self.encryption).await;
    }

    /// Asks for Monitor Point's saved thumbnail. The device answers with
    /// several messages; the reassembled JPEG arrives as one
    /// `DeviceUpdate::MonitorPointImage` once the transfer ends.
    pub async fn query_monitor_point_image(&mut self, channel_id: u8) {
        self.query_image(channel_id, "guard".to_string(), ImageKind::MonitorPoint).await;
    }

    /// Asks for a preset's saved thumbnail. `preset_{id:02}` is the name
    /// the official app itself uploads a freshly-taken snapshot under
    /// (confirmed against a capture of it doing exactly that for a preset
    /// Reoling had saved without one) — the same read-and-reassemble
    /// mechanism as Monitor Point's, distinguished by `ImageKind`.
    pub async fn query_preset_image(&mut self, channel_id: u8, preset_id: u8) {
        self.query_image(channel_id, format!("preset_{preset_id:02}"), ImageKind::Preset(preset_id)).await;
    }

    async fn query_image(&mut self, channel_id: u8, image_name: String, kind: ImageKind) {
        let payload = image_file_read_xml(channel_id, &image_name);
        self.start_image_transfer(channel_id, MSG_ID_IMAGE_FILE, payload, kind).await;
    }

    /// Asks the camera for a fresh live picture, for `preset_id`; arrives as
    /// `DeviceUpdate::Snapshot`.
    pub async fn request_snapshot(&mut self, channel_id: u8, preset_id: u8) {
        let payload = control_xml(&format!(
            "<Snap version=\"1.1\">\n<channelId>{channel_id}</channelId>\n<logicChannel>{channel_id}</logicChannel>\n<time>0</time>\n<fullFrame>0</fullFrame>\n<streamType>sub</streamType>\n</Snap>\n"
        ));
        self.start_image_transfer(channel_id, MSG_ID_SNAP, payload, ImageKind::Snapshot(preset_id)).await;
    }

    /// Writes `jpeg` as preset `preset_id`'s picture, the way the official
    /// app does (a metadata message, then the bytes, same `msg_num`).
    pub async fn upload_preset_image(&mut self, channel_id: u8, preset_id: u8, jpeg: Vec<u8>) {
        let msg_num = self.next_msg_num();
        let info = control_xml(&format!(
            "<imageFileInfo version=\"1.1\">\n<channelId>{channel_id}</channelId>\n<fileSize>{}</fileSize>\n<imageName>preset_{preset_id:02}</imageName>\n</imageFileInfo>\n",
            jpeg.len()
        ));
        let meta = BcMeta { msg_id: MSG_ID_IMAGE_UPLOAD, channel_id, stream_type: 0, msg_num, response_code: 0, class: 0x6414 };
        let plain = Extension { version: XML_VERSION.to_string(), channel_id: Some(channel_id), ..Default::default() };
        let binary = Extension {
            version: XML_VERSION.to_string(),
            binary_data: Some(1),
            channel_id: Some(channel_id),
            ..Default::default()
        };
        let first = Bc {
            meta: meta.clone(),
            body: BcBody::Modern(ModernMsg { extension_xml: Some(plain.to_bytes()), payload: Some(info) }),
        };
        let second = Bc {
            meta,
            body: BcBody::Modern(ModernMsg { extension_xml: Some(binary.to_bytes()), payload: Some(jpeg) }),
        };
        let mut connection = self.connection.lock().await;
        if connection.send_bc(&first, &self.encryption).await.is_ok() {
            let _ = connection.send_bc(&second, &self.encryption).await;
        }
    }

    /// Saves the current position as preset `id` (`setPos`), with the picture
    /// just uploaded for it, in the full shape the official app sends.
    pub async fn save_preset_with_image(&mut self, channel_id: u8, id: u8, name: &str) -> crate::Result<()> {
        self.send_control(MSG_ID_PTZ_PRESET, channel_id, preset_full_xml(channel_id, id, Some("setPos"), name, true)).await
    }

    /// Renames preset `id` and/or gives it the picture just uploaded for it
    /// — a `preset` entry with no `command` at all, as captured from the
    /// official app's Edit dialog; the position is left alone.
    pub async fn modify_preset(&mut self, channel_id: u8, id: u8, name: &str, with_image: bool) -> crate::Result<()> {
        self.send_control(MSG_ID_PTZ_PRESET, channel_id, preset_full_xml(channel_id, id, None, name, with_image)).await
    }

    async fn start_image_transfer(&mut self, channel_id: u8, msg_id: u32, payload: Vec<u8>, kind: ImageKind) {
        let msg_num = self.next_msg_num();
        self.pending_images.lock().expect("pending_images mutex poisoned").insert(msg_num, kind);
        let extension = Extension {
            version: XML_VERSION.to_string(),
            channel_id: Some(channel_id),
            ..Default::default()
        };
        let request = Bc {
            meta: BcMeta {
                msg_id,
                channel_id,
                stream_type: 0,
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: Some(extension.to_bytes()),
                payload: Some(payload),
            }),
        };
        if self.connection.lock().await.send_bc(&request, &self.encryption).await.is_err() {
            self.pending_images.lock().expect("pending_images mutex poisoned").remove(&msg_num);
            return;
        }
        // Safety net: if the device never replies at all (drops the request
        // outright, seen under load), the pending entry would otherwise
        // block every later image query for the rest of the connection.
        let pending_images = Arc::clone(&self.pending_images);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            pending_images.lock().expect("pending_images mutex poisoned").remove(&msg_num);
        });
    }

    /// Configures Monitor Point (Auto Return on/off, its timeout) without
    /// moving it — moving it would need `set_current_position_as_monitor_point`
    /// instead, so a plain settings change never relocates it by accident.
    pub async fn set_monitor_point_config(
        &mut self,
        channel_id: u8,
        enabled: bool,
        timeout_seconds: u32,
    ) -> crate::Result<()> {
        let timeout = timeout_seconds.clamp(10, 300);
        let xml = monitor_point_xml(channel_id, enabled, timeout, "setGrd", false);
        self.send_control(MSG_ID_PTZ_GUARD, channel_id, xml).await
    }

    /// Saves the camera's current position as Monitor Point (`needSetPos`)
    /// — "Reset Monitor Point" in the official app. Point the camera where
    /// you want it (e.g. via `ptz`) before calling this.
    pub async fn set_current_position_as_monitor_point(
        &mut self,
        channel_id: u8,
        enabled: bool,
        timeout_seconds: u32,
    ) -> crate::Result<()> {
        let timeout = timeout_seconds.clamp(10, 300);
        let xml = monitor_point_xml(channel_id, enabled, timeout, "setGrd", true);
        self.send_control(MSG_ID_PTZ_GUARD, channel_id, xml).await
    }

    /// Moves the camera to Monitor Point now ("Return to Monitor Point").
    pub async fn go_to_monitor_point(&mut self, channel_id: u8, timeout_seconds: u32) -> crate::Result<()> {
        let timeout = timeout_seconds.clamp(10, 300);
        let xml = monitor_point_xml(channel_id, false, timeout, "toGrd", false);
        self.send_control(MSG_ID_PTZ_GUARD, channel_id, xml).await
    }

    /// Opens a talk session with the camera: the audio format it should
    /// expect (the one every camera here announced: ADPCM, 16 kHz, mono).
    /// The answer arrives as a `ControlReply` for message 201 (422 means
    /// another talk is still open: send `talk_stop` and try again).
    pub async fn talk_start(&mut self, channel_id: u8) -> crate::Result<()> {
        self.talk_msg_num = self.next_msg_num();
        self.send_control(MSG_ID_TALK_CONFIG, channel_id, talk_config_xml(channel_id)).await
    }

    /// One block of audio (see `talk`), sent unencrypted under the number the
    /// talk started with.
    pub async fn talk_block(&mut self, channel_id: u8, block: &[u8]) -> crate::Result<()> {
        let extension = Extension {
            version: XML_VERSION.to_string(),
            binary_data: Some(1),
            channel_id: Some(channel_id),
            ..Default::default()
        };
        let request = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_TALK_DATA,
                channel_id,
                stream_type: 0,
                msg_num: self.talk_msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: Some(extension.to_bytes()),
                payload: Some(crate::talk::adpcm_unit(block)),
            }),
        };
        self.connection.lock().await.send_bc(&request, &self.encryption).await
    }

    /// Ends the talk session.
    pub async fn talk_stop(&mut self, channel_id: u8) -> crate::Result<()> {
        let msg_num = self.next_msg_num();
        let extension = Extension {
            version: XML_VERSION.to_string(),
            channel_id: Some(channel_id),
            ..Default::default()
        };
        let request = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_TALK_STOP,
                channel_id,
                stream_type: 0,
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg { extension_xml: Some(extension.to_bytes()), payload: None }),
        };
        self.connection.lock().await.send_bc(&request, &self.encryption).await
    }

    /// Asks where the camera's zoom and focus are and how far they go; the
    /// answer arrives as `DeviceUpdate::ZoomFocus`.
    pub async fn query_zoom_focus(&mut self, channel_id: u8) {
        self.request_for_channel(MSG_ID_GET_ZOOM_FOCUS, channel_id).await;
    }

    /// Moves the zoom to a position (within what `query_zoom_focus` reported).
    pub async fn set_zoom(&mut self, channel_id: u8, position: u32) -> crate::Result<()> {
        self.send_control(MSG_ID_SET_ZOOM_FOCUS, channel_id, zoom_xml(channel_id, position)).await
    }

    /// Moves the focus to a position. The command name is `focusPos`, by
    /// analogy with the captured `zoomPos`; not itself seen in a capture.
    pub async fn set_focus(&mut self, channel_id: u8, position: u32) -> crate::Result<()> {
        self.send_control(MSG_ID_SET_ZOOM_FOCUS, channel_id, focus_xml(channel_id, position)).await
    }

    /// Asks for the camera's saved presets; the answer arrives as
    /// `DeviceUpdate::Presets`.
    pub async fn query_presets(&mut self, channel_id: u8) {
        self.request_for_channel(MSG_ID_GET_PTZ_PRESET, channel_id).await;
    }

    async fn send_preset(&mut self, channel_id: u8, id: u8, name: Option<&str>, command: &str) -> crate::Result<()> {
        self.send_control(MSG_ID_PTZ_PRESET, channel_id, preset_xml(channel_id, id, name, command)).await
    }

    /// Saves the current position as preset `id`, named `name`.
    pub async fn set_preset(&mut self, channel_id: u8, id: u8, name: &str) -> crate::Result<()> {
        self.send_preset(channel_id, id, Some(name), "setPos").await
    }

    /// Moves the camera to preset `id`.
    pub async fn goto_preset(&mut self, channel_id: u8, id: u8) -> crate::Result<()> {
        self.send_preset(channel_id, id, None, "toPos").await
    }

    /// Removes preset `id`. Unverified against a real capture (see
    /// `PtzPresetXml`'s doc comment) — the camera may accept the request
    /// (code 200) without actually clearing the slot on some firmwares.
    pub async fn delete_preset(&mut self, channel_id: u8, id: u8) -> crate::Result<()> {
        self.send_preset(channel_id, id, None, "delPos").await
    }

    /// Switches the camera's spotlight on or off.
    pub async fn set_spotlight(&mut self, channel_id: u8, on: bool) -> crate::Result<()> {
        self.send_control(MSG_ID_SPOTLIGHT, channel_id, spotlight_xml(channel_id, on)).await
    }

    async fn send_empty_request(&mut self, msg_id: u32) -> crate::Result<()> {
        let msg_num = self.next_msg_num();
        let request = Bc {
            meta: BcMeta {
                msg_id,
                channel_id: 0,
                stream_type: 0,
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: None }),
        };
        self.connection.lock().await.send_bc(&request, &self.encryption).await
    }

    /// Asks the logged-in device for its name and model. Best effort: any
    /// failure or silence yields an empty identity rather than an error.
    pub async fn identity(&mut self) -> DeviceIdentity {
        // What the official app sends first after login (seen in a capture
        // against a Home Hub, and confirmed by a probe: the Hub pushes its
        // channel list, with the channels' names, only after these two).
        // Answers are picked up as they come.
        let _ = self.send_empty_request(MSG_ID_STREAM_INFO).await;
        let _ = self.send_empty_request(MSG_ID_SUBSCRIBE).await;
        // Which camera can do what: the support table, one row per channel.
        self.request_for_channel(MSG_ID_SUPPORT, 0).await;
        let msg_num = self.next_msg_num();
        let request = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_VERSION,
                channel_id: 0,
                stream_type: 0,
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: None }),
        };
        let mut info: Option<VersionInfo> = None;
        let exchange = async {
            if self.connection.lock().await.send_bc(&request, &self.encryption).await.is_err() {
                return;
            }
            let mut image_chunks = HashMap::new();
            for _ in 0..16 {
                let Ok(reply) = self.connection.lock().await.recv_bc(&self.encryption).await
                else {
                    return;
                };
                if let Some(update) = pushed_update(&reply, &mut image_chunks, &self.pending_images, &self.channel_uids) {
                    let _ = self.channel_updates_tx.send(update);
                    continue;
                }
                if reply.meta.msg_id != MSG_ID_VERSION {
                    continue;
                }
                if let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = reply.body {
                    info = BcXml::from_bytes(&payload).ok().and_then(|x| x.version_info);
                }
                return;
            }
        };
        let _ = tokio::time::timeout(IDENTITY_TIMEOUT, exchange).await;
        let (name, model) = info.map(|i| (i.name, i.model)).unwrap_or_default();
        if std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
            eprintln!("DEBUG identity reply: name={name:?} model={model:?}");
        }
        // Diagnostic-only: a scripted recorded-video session on one channel
        // (see `probe_replay`), or an event search (see `probe_events`).
        // Each runs only when its variable is set. Lines are tagged with
        // the model (never the UID): every device the app connects to runs
        // the probe, side by side.
        let tag = model.as_deref().map(str::trim).filter(|m| !m.is_empty()).unwrap_or("device").to_string();
        if let Some(channel_id) = replay::probe_replay_channel() {
            self.probe_replay(channel_id, &tag).await;
        }
        if let Some(channel_id) = replay::probe_events_channel() {
            self.probe_events(channel_id, &tag).await;
        }
        DeviceIdentity { name, model }
    }

    /// Diagnostic-only (`REOLING_PROBE_PUSH`): sends the empty requests the
    /// official app sends right after login (192 and 146, seen in a capture
    /// against a Home Hub, which then pushed its channel list) and prints
    /// every reply, decrypted, for a few seconds.
    pub async fn probe_pushes(&mut self, username: &str) {
        // The ability query (151) names the sections it wants in its extension.
        let ability = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<Extension version=\"1.1\">\n<userName>{username}</userName>\n<token>system, streaming, PTZ, IO, security, replay, disk, network, alarm, record, video, image</token>\n</Extension>\n"
        );
        {
            let msg_num = self.next_msg_num();
            let request = Bc {
                meta: BcMeta {
                    msg_id: 151,
                    channel_id: 0,
                    stream_type: 0,
                    msg_num,
                    response_code: 0,
                    class: 0x6414,
                },
                body: BcBody::Modern(ModernMsg {
                    extension_xml: Some(ability.into_bytes()),
                    payload: None,
                }),
            };
            let sent = self.connection.lock().await.send_bc(&request, &self.encryption).await;
            eprintln!("PROBE sent 151: {sent:?}");
        }
        // What each channel can do (58, per channel) is what decides which
        // camera controls to offer; shown here to see how the devices word it.
        // `REOLING_PROBE_PUSH=0,11` probes those channels; empty probes 0-2.
        let channels: Vec<u8> = std::env::var("REOLING_PROBE_PUSH")
            .ok()
            .map(|v| v.split(',').filter_map(|c| c.trim().parse().ok()).collect::<Vec<u8>>())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| vec![0, 1, 2]);
        for channel_id in channels {
            self.request_for_channel(MSG_ID_ABILITY_SUPPORT, channel_id).await;
            // The per-channel requests the official app makes at start (talk
            // ability, channel type, and others): which of the answers says
            // what each camera can do is what this looks for. 56 is the
            // encoder configuration, 730 its per-channel allowed values.
            for msg_id in [10u32, 299, 56, 730, 199] {
                self.request_for_channel(msg_id, channel_id).await;
            }
            // The per-channel version info (the camera model on an NVR or
            // Hub channel) is asked for with the channel type too.
            let msg_num = self.next_msg_num();
            let request = Bc {
                meta: BcMeta { msg_id: 318, channel_id, stream_type: 0, msg_num, response_code: 0, class: 0x6414 },
                body: BcBody::Modern(ModernMsg {
                    extension_xml: Some(monitor_point_read_extension(channel_id)),
                    payload: None,
                }),
            };
            let _ = self.connection.lock().await.send_bc(&request, &self.encryption).await;
        }
        for msg_id in [192u32, 146] {
            let msg_num = self.next_msg_num();
            let request = Bc {
                meta: BcMeta {
                    msg_id,
                    channel_id: 0,
                    stream_type: 0,
                    msg_num,
                    response_code: 0,
                    class: 0x6414,
                },
                body: BcBody::Modern(ModernMsg { extension_xml: None, payload: None }),
            };
            let sent = self.connection.lock().await.send_bc(&request, &self.encryption).await;
            eprintln!("PROBE sent {msg_id}: {sent:?}");
        }
        let listen = async {
            let mut image_chunks = HashMap::new();
            loop {
                let Ok(bc) = self.connection.lock().await.recv_bc(&self.encryption).await else {
                    return;
                };
                let text = match &bc.body {
                    BcBody::Modern(ModernMsg { payload: Some(p), .. }) => {
                        let t = redact_identifiers(&String::from_utf8_lossy(p));
                        t.chars().take(30000).map(|c| if c == '\n' { ' ' } else { c }).collect::<String>()
                    }
                    other => format!("{other:?}").chars().take(200).collect(),
                };
                eprintln!(
                    "PROBE msg_id={} code={} num={} body={}",
                    bc.meta.msg_id, bc.meta.response_code, bc.meta.msg_num, text
                );
                // Keep the app working while probing: the channel list is
                // among what arrives here.
                if let Some(update) = pushed_update(&bc, &mut image_chunks, &self.pending_images, &self.channel_uids) {
                    let _ = self.channel_updates_tx.send(update);
                }
            }
        };
        let _ = tokio::time::timeout(std::time::Duration::from_secs(8), listen).await;
    }

    /// Diagnostic-only, not used by `login`: sends only the modern
    /// `LoginUser` with an empty nonce, skipping the legacy step
    /// unconditionally (regardless of `peer_nonce`) and matching the real
    /// captured client's exact message shape (`class=0x0000`, `msg_num=0`)
    /// byte-for-byte. Exists to test, against real hardware, whether a
    /// device that disconnects during `login`'s legacy step will accept a
    /// login with no nonce at all — isolating "device dislikes our login
    /// message" from "device dislikes our session" before investigating
    /// further. Delete once that question is answered.
    pub async fn login_probe_empty_nonce(
        &mut self,
        username: &str,
        password: &str,
    ) -> crate::Result<DeviceInfoSummary> {
        let nonce = String::new();
        self.encryption = EncryptionProtocol::Aes {
            key: aes_key_from_password(password, &nonce),
        };
        let modern_login = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: 0,
                response_code: 0,
                class: 0x0000,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(
                    BcXml {
                        login_user: Some(LoginUser {
                            version: XML_VERSION.to_string(),
                            user_name: md5_hex_truncated(&format!("{username}{nonce}")),
                            password: md5_hex_truncated(&format!("{password}{nonce}")),
                            user_ver: 1,
                        }),
                        login_net: Some(LoginNet::default()),
                        ..Default::default()
                    }
                    .to_bytes(),
                ),
            }),
        };
        self.connection.lock().await.send_bc(&modern_login, &self.encryption).await?;
        let reply = self.connection.lock().await.recv_bc(&self.encryption).await?;
        if reply.meta.response_code != 200 {
            return Err(Error::LoginFailed { code: reply.meta.response_code });
        }
        let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = reply.body else {
            return Err(Error::ProtocolError("expected a DeviceInfo reply".to_string()));
        };
        if std::env::var("REOLING_PROBE_PUSH").is_ok() {
            let text: String = redact_identifiers(&String::from_utf8_lossy(&payload)).chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
            eprintln!("PROBE login reply: {text}");
        }
        self.encode_profiles = crate::encoding::EncodeProfile::parse_list(&String::from_utf8_lossy(&payload));
        let device_info = BcXml::from_bytes(&payload)?.device_info.unwrap_or_default();
        Ok(DeviceInfoSummary {
            resolution_name: device_info.resolution.and_then(|r| r.name),
        })
    }

    pub async fn start_video(
        &mut self,
        channel_id: u8,
        quality: StreamProfile,
    ) -> crate::Result<ReceiverStream<crate::Result<VideoFrame>>> {
        self.start_video_with_trace(channel_id, quality, None).await
    }

    /// Diagnostic variant: records every decrypted video `Bc` message
    /// (with its arrival timestamp) before `BcMedia` parsing, via
    /// `media_trace::MediaTrace`. The normal application (and `start_video`
    /// above) passes `None` and pays no extra cost.
    pub async fn start_video_with_trace(
        &mut self,
        channel_id: u8,
        quality: StreamProfile,
        trace: Option<Arc<crate::media_trace::MediaTrace>>,
    ) -> crate::Result<ReceiverStream<crate::Result<VideoFrame>>> {
        // Only one reader may consume the connection's data: a playback
        // still running would swallow the live stream's messages. Its close
        // is not waited for long (see `LIVE_SWITCH_CLOSE_WAIT`).
        if self.replay.is_some() || self.replay_task.is_some() {
            let _ = self.stop_replay_within(replay::LIVE_SWITCH_CLOSE_WAIT).await;
        }
        // The same for a download: its reader is stopped (the device may go
        // on sending for a moment; live video's reader drops that).
        if self.download_task.is_some() {
            let _ = self.stop_download_within(replay::LIVE_SWITCH_CLOSE_WAIT).await;
        }
        let msg_num = self.next_msg_num();
        // handle/stream_type per quality. `Main`'s values (handle 0,
        // BcMeta.stream_type 0, XML streamType "mainStream") come straight
        // from our own real capture and are not in question.
        //
        // `Sub`'s values (handle 256, BcMeta.stream_type 1, XML streamType
        // "subStream") are NOT independently confirmed against our own
        // Wireshark capture — that capture only covers Main-stream Preview
        // usage. They come from the 2026-09-16 audit (section 21), not
        // from a capture. Treat as unverified until confirmed against real
        // hardware requesting a Sub stream.
        let (handle, stream_type_str, bc_meta_stream_type) = match quality {
            StreamProfile::Main => (0, "mainStream", 0),
            StreamProfile::Sub => (256, "subStream", 1),
            StreamProfile::Extern => (1024, "externStream", 0),
        };
        let request = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_VIDEO,
                channel_id,
                stream_type: bc_meta_stream_type,
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(
                    BcXml {
                        preview: Some(Preview {
                            version: XML_VERSION.to_string(),
                            channel_id,
                            handle,
                            stream_type: Some(stream_type_str.to_string()),
                        }),
                        ..Default::default()
                    }
                    .to_bytes(),
                ),
            }),
        };
        // DEBUG-ONLY, temporary (`REOLING_DEBUG_NEGOTIATION`): our own
        // plaintext Preview request and the camera's reply, to compare
        // against the official client's for a negotiated-parameter mismatch
        // that could explain a lower camera bitrate. No credentials here.
        let debug_negotiation = std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok();
        if debug_negotiation {
            if let BcBody::Modern(ModernMsg { payload: Some(p), .. }) = &request.body {
                eprintln!(
                    "NEGOTIATION preview request: channel_id={} stream_type={} class={:#x} xml={}",
                    request.meta.channel_id,
                    request.meta.stream_type,
                    request.meta.class,
                    String::from_utf8_lossy(p)
                );
            }
        }
        self.playing = Some(PlayingStream {
            channel_id,
            handle,
            stream_name: stream_type_str,
            bc_stream_type: bc_meta_stream_type,
        });
        {
            let mut connection = self.connection.lock().await;
            // Live video's own bound on unread data from here on, even right
            // after a playback (closed above, or earlier by the caller): this
            // reader never pauses and drops what still trails in from it. A
            // no-op when no playback ever ran.
            connection.end_replay_buffering_now();
            connection.send_bc(&request, &self.encryption).await?;
        }
        // Pushes (channel lists, alarm events, ...) can come between our
        // request and the answer; they are not the answer.
        let mut image_chunks = HashMap::new();
        let ack = loop {
            let bc = self.connection.lock().await.recv_bc(&self.encryption).await?;
            if let Some(update) = pushed_update(&bc, &mut image_chunks, &self.pending_images, &self.channel_uids) {
                let _ = self.channel_updates_tx.send(update);
            } else if bc.meta.msg_id == MSG_ID_VIDEO && bc.meta.msg_num == msg_num {
                // Same message number as our request: an earlier stream's
                // leftover frames carry theirs.
                break bc;
            }
        };
        if debug_negotiation {
            eprintln!(
                "NEGOTIATION preview reply: response_code={} class={:#x} body={:?}",
                ack.meta.response_code, ack.meta.class, ack.body
            );
        }
        if ack.meta.response_code != 200 {
            return Err(Error::ProtocolError(format!(
                "camera rejected the video stream request with code {}",
                ack.meta.response_code
            )));
        }

        let (tx, rx) = channel(32);
        let connection = Arc::clone(&self.connection);
        let encryption = self.encryption.clone();
        let channel_updates = self.channel_updates_tx.clone();
        let audio = self.audio_tx.clone();
        let pending_images = Arc::clone(&self.pending_images);
        let channel_uids = Arc::clone(&self.channel_uids);
        self.video_task = Some(tokio::spawn(async move {
            let mut buffer: Vec<u8> = Vec::new();
            let mut guard = MediaGuard::default();
            let mut image_chunks = HashMap::new();
            loop {
                let bc = {
                    let mut conn = connection.lock().await;
                    match conn.recv_bc(&encryption).await {
                        Ok(bc) => bc,
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    }
                };
                if let Some(update) = pushed_update(&bc, &mut image_chunks, &pending_images, &channel_uids) {
                    let _ = channel_updates.send(update);
                    continue;
                }
                if bc.meta.msg_id != MSG_ID_VIDEO {
                    continue;
                }
                if let Some(trace) = &trace {
                    trace.record(&bc);
                }
                let BcBody::Modern(ModernMsg { extension_xml, payload: Some(payload) }) = bc.body
                else {
                    continue;
                };
                let extension =
                    extension_xml.as_deref().and_then(|xml| Extension::from_bytes(xml).ok());
                match guard.judge(extension.as_ref(), &payload) {
                    Verdict::Accept => {}
                    Verdict::Skip => continue,
                    Verdict::Damaged => {
                        // The partial unit being assembled is damaged too.
                        buffer.clear();
                        continue;
                    }
                }
                buffer.extend_from_slice(&payload);
                loop {
                    match parse_one(&buffer) {
                        Ok(Some((msg, used))) => {
                            buffer.drain(..used);
                            if let BcMediaMessage::Info { width, height } = &msg {
                                if std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
                                    eprintln!("NEGOTIATION stream info unit: {width}x{height}");
                                }
                            }
                            if let BcMediaMessage::Audio(frame) = msg {
                                let _ = audio.send(frame);
                            } else if let BcMediaMessage::Video(frame) = msg {
                                if tx.send(Ok(frame)).await.is_err() {
                                    return; // receiver dropped
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            // Not the start of a unit — typically the tail of
                            // the stream we just switched away from, still in
                            // flight when the new one began. Skip to the next
                            // unit instead of ending the stream.
                            let skip = crate::protocol::bcmedia::model::resync_offset(&buffer);
                            if std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
                                eprintln!("NEGOTIATION media resync: {e}; skipped {skip} bytes");
                            }
                            if skip == 0 {
                                break;
                            }
                            buffer.drain(..skip);
                        }
                    }
                }
            }
        }));
        Ok(ReceiverStream::new(rx))
    }

    /// Stops the stream `start_video` opened (or, if none is known, whatever
    /// `channel_id` is sending), naming it the way the official app does.
    pub async fn stop_video(&mut self, channel_id: u8) -> crate::Result<()> {
        if let Some(task) = self.video_task.take() {
            task.abort();
        }
        let playing = self.playing.take();
        let msg_num = self.next_msg_num();
        let payload = playing.as_ref().map(|p| {
            BcXml {
                preview: Some(Preview {
                    version: XML_VERSION.to_string(),
                    channel_id: p.channel_id,
                    handle: p.handle,
                    stream_type: Some(p.stream_name.to_string()),
                }),
                ..Default::default()
            }
            .to_bytes()
        });
        let request = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_VIDEO_STOP,
                channel_id: playing.as_ref().map_or(channel_id, |p| p.channel_id),
                stream_type: playing.as_ref().map_or(0, |p| p.bc_stream_type),
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload }),
        };
        self.connection.lock().await.send_bc(&request, &self.encryption).await
    }

    /// Asks for the names of these channels. The answers come back through
    /// `take_channel_updates` as `DeviceUpdate::ChannelName`.
    pub async fn request_channel_names(&mut self, channel_ids: &[u8]) {
        for &channel_id in channel_ids {
            self.request_for_channel(MSG_ID_OSD, channel_id).await;
        }
    }

    /// The encoder values the device allows, from its login reply.
    pub fn encode_profiles(&self) -> Vec<crate::encoding::EncodeProfile> {
        self.encode_profiles.clone()
    }

    /// Asks for a channel's encoder settings (`DeviceUpdate::Encoding`).
    pub async fn query_encoding(&mut self, channel_id: u8) {
        self.request_for_channel(MSG_ID_GET_COMPRESSION, channel_id).await;
    }

    /// Asks for the encoder values one channel allows
    /// (`DeviceUpdate::EncodeProfiles`), for when the login list has none.
    pub async fn query_encode_profiles(&mut self, channel_id: u8) {
        self.request_for_channel(MSG_ID_CHANNEL_ENCODE_TABLE, channel_id).await;
    }

    /// Asks which camera sits on a channel (`DeviceUpdate::ChannelVersion`).
    /// Like the Monitor Point read, this request names the channel type too.
    pub async fn query_channel_version(&mut self, channel_id: u8) {
        let msg_num = self.next_msg_num();
        let request = Bc {
            meta: BcMeta { msg_id: MSG_ID_CHANNEL_VERSION, channel_id, stream_type: 0, msg_num, response_code: 0, class: 0x6414 },
            body: BcBody::Modern(ModernMsg {
                extension_xml: Some(monitor_point_read_extension(channel_id)),
                payload: None,
            }),
        };
        let _ = self.connection.lock().await.send_bc(&request, &self.encryption).await;
    }

    /// Writes a channel's encoder settings: `body` is a 56 reply with the
    /// changes applied (`Encoding::with_changes`). The answer arrives as a
    /// `DeviceUpdate::ControlReply` for 57.
    pub async fn set_encoding(&mut self, channel_id: u8, body: String) -> crate::Result<()> {
        if std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
            eprintln!("DEBUG encoding write: channel={channel_id} body={}", body.replace('\n', " "));
        }
        self.send_control(MSG_ID_SET_COMPRESSION, channel_id, body.into_bytes()).await
    }

    /// An empty request about one channel (the channel travels in the
    /// extension, as in the official app's requests for OSD settings).
    async fn request_for_channel(&mut self, msg_id: u32, channel_id: u8) {
        {
            let msg_num = self.next_msg_num();
            let extension = Extension {
                version: XML_VERSION.to_string(),
                channel_id: Some(channel_id),
                ..Default::default()
            };
            let request = Bc {
                meta: BcMeta {
                    msg_id,
                    channel_id,
                    stream_type: 0,
                    msg_num,
                    response_code: 0,
                    class: 0x6414,
                },
                body: BcBody::Modern(ModernMsg {
                    extension_xml: Some(extension.to_bytes()),
                    payload: None,
                }),
            };
            let _ = self.connection.lock().await.send_bc(&request, &self.encryption).await;
        }
    }

    /// Reads whatever the device sends while no video is running (pushed
    /// channel lists, alarm events), forwarding channel lists to
    /// `take_channel_updates`. Returns only when the connection fails.
    /// Cancel-safe: partial messages are kept by the connection.
    pub async fn wait_for_pushes(&mut self) -> crate::Error {
        // While a playback runs, its reader is the one reading (and passes
        // the pushes on); reading here as well would take its data.
        while self.replay_task.as_ref().is_some_and(|t| !t.is_finished()) || self.download_running() {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let mut image_chunks = HashMap::new();
        loop {
            let received = self.connection.lock().await.recv_bc(&self.encryption).await;
            match received {
                Ok(bc) => {
                    if let Some(update) = pushed_update(&bc, &mut image_chunks, &self.pending_images, &self.channel_uids) {
                        let _ = self.channel_updates_tx.send(update);
                    }
                }
                Err(Error::ReplyTimeout) => {}
                Err(e) => return e,
            }
        }
    }

    pub async fn logout(&mut self) -> crate::Result<()> {
        let msg_num = self.next_msg_num();
        let request = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGOUT,
                channel_id: 0,
                stream_type: 0,
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: None }),
        };
        self.connection.lock().await.send_bc(&request, &self.encryption).await
    }

    /// Tells the device this session is ending, so it stops streaming
    /// immediately instead of continuing until its own idle timeout — see
    /// `BcConnection::disconnect`'s doc comment. Confirmed real
    /// 2026-09-16: closing the official Windows app makes the camera stop
    /// sending right away; closing this app previously left it sending
    /// until its own TTL expired, because nothing was ever called here.
    /// Callers should invoke this before dropping the client (or the
    /// window that owns it) rather than just letting it fall out of
    /// scope.
    ///
    /// Order matters here: `video_task` (still running, looping on
    /// `connection.lock()` for every frame) is stopped *first* so it
    /// can't keep re-acquiring the lock ahead of this call; the P2P-level
    /// `C2D_DISC` — the one that actually matters for this symptom — goes
    /// out next; `logout`'s BC-level message is best-effort and last,
    /// since it can fail (e.g. the connection is already half-gone)
    /// without that being a reason to skip or delay the disconnect that
    /// does matter.
    pub async fn disconnect(&mut self) {
        if let Some(task) = self.video_task.take() {
            task.abort();
        }
        if let Some(task) = self.replay_task.take() {
            task.abort();
        }
        self.replay = None;
        if let Some(task) = self.download_task.take() {
            task.abort();
        }
        self.download_cancel = None;
        self.connection.lock().await.disconnect().await;
        let _ = self.logout().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UdpSocket;
    use tokio_stream::StreamExt;

    /// Drives a `BcConnection` as if it were the camera: legacy login,
    /// modern login, then a Preview ack followed by one video Iframe.
    async fn run_fake_camera(mut conn: BcConnection) {
        // 1. Legacy login upgrade -> reply with nonce, unencrypted for simplicity.
        let legacy = conn.recv_bc(&EncryptionProtocol::Unencrypted).await.unwrap();
        assert_eq!(legacy.meta.msg_id, MSG_ID_LOGIN);
        let nonce_reply = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: legacy.meta.msg_num,
                response_code: 0,
                class: 0x6614,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(
                    BcXml {
                        encryption: Some(Encryption {
                            version: XML_VERSION.to_string(),
                            type_: "md5".to_string(),
                            nonce: "TESTNONCE".to_string(),
                        }),
                        ..Default::default()
                    }
                    .to_bytes(),
                ),
            }),
        };
        conn.send_bc(&nonce_reply, &EncryptionProtocol::Unencrypted).await.unwrap();

        // From here on the real camera (and our client) switches to AES,
        // keyed from the password and the nonce just exchanged.
        let enc = EncryptionProtocol::Aes {
            key: aes_key_from_password("swordfish", "TESTNONCE"),
        };

        // 2. Modern login -> reply with DeviceInfo, response_code 200.
        let modern = conn.recv_bc(&enc).await.unwrap();
        assert_eq!(modern.meta.msg_id, MSG_ID_LOGIN);
        let device_info_reply = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: modern.meta.msg_num,
                response_code: 200,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(
                    BcXml {
                        device_info: Some(DeviceInfo { version: None, resolution: None }),
                        ..Default::default()
                    }
                    .to_bytes(),
                ),
            }),
        };
        conn.send_bc(&device_info_reply, &enc).await.unwrap();

        // 3. Preview request -> ack (response_code 200, empty body).
        let preview = conn.recv_bc(&enc).await.unwrap();
        assert_eq!(preview.meta.msg_id, MSG_ID_VIDEO);
        let ack = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_VIDEO,
                channel_id: 0,
                stream_type: 0,
                msg_num: preview.meta.msg_num,
                response_code: 200,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: None }),
        };
        conn.send_bc(&ack, &enc).await.unwrap();

        // 4. One video data message: Extension says binary, payload is a raw
        // bcmedia Iframe unit.
        let mut media_bytes = Vec::new();
        media_bytes.extend_from_slice(&0x63643030u32.to_le_bytes()); // Iframe magic
        media_bytes.extend_from_slice(b"H264");
        let frame_data = vec![0, 0, 0, 1, 0x67];
        media_bytes.extend_from_slice(&(frame_data.len() as u32).to_le_bytes());
        media_bytes.extend_from_slice(&0u32.to_le_bytes());
        media_bytes.extend_from_slice(&999u32.to_le_bytes());
        media_bytes.extend_from_slice(&0u32.to_le_bytes());
        media_bytes.extend_from_slice(&frame_data);
        let pad = (8 - frame_data.len() % 8) % 8;
        media_bytes.extend(std::iter::repeat(0u8).take(pad));

        let video_data = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_VIDEO,
                channel_id: 0,
                stream_type: 0,
                msg_num: preview.meta.msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: Some(
                    Extension {
                        version: XML_VERSION.to_string(),
                        binary_data: Some(1),
                        channel_id: Some(0),
                        encrypt_len: Some(media_bytes.len() as u32),
                        ..Default::default()
                    }
                    .to_bytes(),
                ),
                payload: Some(media_bytes),
            }),
        };
        conn.send_bc(&video_data, &enc).await.unwrap();

        // 5. A second Iframe on the same connection, to prove start_video
        // keeps yielding frames without a second start_video call.
        let mut media_bytes_2 = Vec::new();
        media_bytes_2.extend_from_slice(&0x63643030u32.to_le_bytes()); // Iframe magic
        media_bytes_2.extend_from_slice(b"H264");
        let frame_data_2 = vec![9, 9, 9];
        media_bytes_2.extend_from_slice(&(frame_data_2.len() as u32).to_le_bytes());
        media_bytes_2.extend_from_slice(&0u32.to_le_bytes());
        media_bytes_2.extend_from_slice(&1000u32.to_le_bytes());
        media_bytes_2.extend_from_slice(&0u32.to_le_bytes());
        media_bytes_2.extend_from_slice(&frame_data_2);
        let pad_2 = (8 - frame_data_2.len() % 8) % 8;
        media_bytes_2.extend(std::iter::repeat(0u8).take(pad_2));
        let video_data_2 = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_VIDEO,
                channel_id: 0,
                stream_type: 0,
                msg_num: preview.meta.msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: Some(
                    Extension {
                        version: XML_VERSION.to_string(),
                        binary_data: Some(1),
                        channel_id: Some(0),
                        encrypt_len: Some(media_bytes_2.len() as u32),
                        ..Default::default()
                    }
                    .to_bytes(),
                ),
                payload: Some(media_bytes_2),
            }),
        };
        conn.send_bc(&video_data_2, &enc).await.unwrap();
    }

    #[tokio::test]
    async fn login_and_start_video_against_a_fake_camera() {
        let client_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let camera_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let client_addr = client_socket.local_addr().unwrap();
        let camera_addr = camera_socket.local_addr().unwrap();

        let camera_conn = BcConnection::new(
            camera_socket,
            PeerHandle { addr: client_addr, local_connection_id: 2, remote_connection_id: 1, nonce: None, is_direct: false },
        );
        let fake_camera = tokio::spawn(run_fake_camera(camera_conn));

        let client_conn = BcConnection::new(
            client_socket,
            PeerHandle { addr: camera_addr, local_connection_id: 1, remote_connection_id: 2, nonce: None, is_direct: false },
        );
        let mut client = ReolinkClient::from_connection(client_conn, EncryptionProtocol::Unencrypted);

        let _device_info = client.login("admin", "swordfish").await.unwrap();

        let mut frames = client.start_video(0, StreamProfile::Main).await.unwrap();
        let frame = frames.next().await.unwrap().unwrap();
        assert_eq!(frame.data, vec![0, 0, 0, 1, 0x67]);
        assert_eq!(frame.microseconds, 999);

        // The background reader task keeps yielding frames without a
        // second start_video call — this is Task 18's whole point.
        let second = frames.next().await.unwrap().unwrap();
        assert_eq!(second.data, vec![9, 9, 9]);
        assert_eq!(second.microseconds, 1000);

        fake_camera.await.unwrap();
    }

    #[tokio::test]
    async fn login_and_start_video_against_a_fake_tcp_camera() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let fake_camera = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            run_fake_camera(BcConnection::from_tcp(stream)).await;
        });

        let mut client = ReolinkClient::connect_by_ip(addr.ip(), addr.port()).await.unwrap();

        let _device_info = client.login("admin", "swordfish").await.unwrap();

        let mut frames = client.start_video(0, StreamProfile::Main).await.unwrap();
        let frame = frames.next().await.unwrap().unwrap();
        assert_eq!(frame.data, vec![0, 0, 0, 1, 0x67]);
        assert_eq!(frame.microseconds, 999);

        fake_camera.await.unwrap();
    }
}

#[cfg(test)]
mod control_tests {
    use super::*;

    #[test]
    fn preset_commands_round_trip_through_bcxml() {
        for xml in [
            preset_xml(0, 5, Some("test1"), "setPos"),
            preset_xml(0, 5, None, "toPos"),
            preset_xml(0, 5, None, "delPos"),
        ] {
            let parsed = BcXml::from_bytes(&xml).unwrap().ptz_preset.unwrap();
            let preset = &parsed.preset_list.unwrap().presets[0];
            assert_eq!(preset.id, 5);
        }
        let named = BcXml::from_bytes(&preset_xml(0, 5, Some("a & b"), "setPos")).unwrap();
        let preset = &named.ptz_preset.unwrap().preset_list.unwrap().presets[0];
        assert_eq!(preset.name.as_deref(), Some("a & b"));
    }


    // Lengths measured on the official app's messages (the payloads are
    // encrypted, but encryption keeps the length).
    #[test]
    fn siren_command_has_the_length_of_the_official_one() {
        assert_eq!(siren_xml(0).len(), 223);
    }

    #[test]
    fn zoom_commands_have_the_lengths_of_the_official_ones() {
        assert_eq!(zoom_xml(0, 0).len(), 177);
        assert_eq!(zoom_xml(0, 100).len(), 179);
    }

    #[test]
    fn talk_config_has_the_length_of_the_official_one() {
        assert_eq!(talk_config_xml(0).len(), 394);
    }

    #[test]
    fn ptz_commands_have_the_length_of_the_official_ones() {
        assert_eq!(ptz_xml(0, "left", 32).len(), 163);
        assert_eq!(ptz_xml(0, "right", 32).len(), 164);
        assert_eq!(ptz_xml(0, "stop", 0).len(), 162);
    }

    #[test]
    fn spotlight_commands_have_the_length_of_the_official_ones() {
        assert_eq!(spotlight_xml(0, true).len(), 177);
        assert_eq!(spotlight_xml(0, false).len(), 177);
    }

    #[test]
    fn diagnostic_text_hides_uids_and_recording_ids() {
        let text = "<ChannelInfo><uid>TESTCAMUID000001</uid><fakeUid>NVRUIDPLACEHOLDR_00</fakeUid></ChannelInfo>\
                    <FileInfo><name>0120261017092107</name><Id>1-0-01260917062107-00000-TESTCAMUID000001</Id></FileInfo>\
                    <uid>SECOND0000000002</uid><uid></uid>";
        let shown = redact_identifiers(text);
        assert!(!shown.contains("TESTCAMUID") && !shown.contains("NVRUID") && !shown.contains("SECOND"), "{shown}");
        assert!(shown.contains("<name>0120261017092107</name>"));
        assert_eq!(shown.matches("<hidden>").count(), 5);
        // An unterminated element is left alone rather than looping.
        assert_eq!(redact_identifiers("<uid>abc"), "<uid>abc");
    }

    #[test]
    fn channel_uids_follow_the_latest_channel_list() {
        let uids = std::sync::Mutex::new(HashMap::new());
        let channel = |channel_id: u8, uid: Option<&str>| ChannelInfo {
            channel_id,
            name: String::new(),
            online: true,
            streams: vec![StreamProfile::Main],
            uid: uid.map(CameraUid::new),
        };
        remember_channel_uids(&uids, &[channel(0, Some("TESTCAMUID000001")), channel(1, Some("TESTCAMUID000002"))]);
        remember_channel_uids(&uids, &[channel(1, None)]);
        let uids = uids.into_inner().unwrap();
        assert_eq!(uids.get(&0).map(CameraUid::as_str), Some("TESTCAMUID000001"));
        assert!(!uids.contains_key(&1));
    }

    #[test]
    fn full_preset_xml_matches_the_captured_edit_and_save_shapes() {
        let edit = String::from_utf8(preset_full_xml(0, 3, None, "Test3-Rename", true)).unwrap();
        assert!(edit.contains("<preset><id>3</id><name>Test3-Rename</name><imageName>preset_03</imageName><xpos>"));
        assert!(!edit.contains("<command>"));
        let save = String::from_utf8(preset_full_xml(0, 0, Some("setPos"), "Test5", true)).unwrap();
        assert!(save.contains("<id>0</id><command>setPos</command><name>Test5</name><imageName>preset_00</imageName>"));
    }
}
