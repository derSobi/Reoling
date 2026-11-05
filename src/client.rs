use crate::protocol::bc::model::*;
use crate::protocol::bc::xml::*;
use crate::protocol::bcmedia::model::{parse_one, BcMediaMessage};
use crate::protocol::crypto::{aes_key_from_password, EncryptionProtocol};
use crate::media_guard::{MediaGuard, Verdict};
use crate::transport::connection::BcConnection;
use crate::transport::discovery::{connect_by_uid_with_path, PeerHandle};
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
    DeviceUsersPage, NetworkRows, RawReplies, Read, ReadFailure, SettingsGates, SettingsPage, SettingsTarget,
    WriteOutcome, WriteReply, SETTINGS_REPLY_TIMEOUT, SETTINGS_WRITE_TIMEOUT,
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
    /// One settings page, complete, with the caller's number for the read
    /// it answers (see `query_settings`).
    Settings { page: SettingsPage, read: u64 },
    /// The device's answers to one group of settings writes, in the order
    /// they were sent, with the caller's number for the group (see
    /// `write_settings`).
    SettingsWritten { target: SettingsTarget, group: u64, results: Vec<settings::WriteOutcome> },
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

/// How long a login step waits for its reply in all, other messages read
/// past included (see `ReolinkClient::recv_login_reply`): the connection's
/// own reply timeout.
const LOGIN_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How many messages a login step reads in all, the reply counted: up to 63
/// others can come ahead of it. The limit is generous: `LOGIN_REPLY_TIMEOUT` is
/// the bound that matters.
const LOGIN_READ_LIMIT: usize = 64;

/// How `identity`'s version request ended, for the negotiation debug output.
#[derive(Debug)]
enum VersionAnswer {
    /// The request could not be sent.
    NotSent(Error),
    /// The reply came, with this code; `body` says what its body gave.
    Reply { code: u16, body: VersionBody },
    /// Reading the connection failed.
    RecvError(Error),
    /// Only other messages came, as many as `identity` reads.
    OnlyOthers,
    /// Nothing within `IDENTITY_TIMEOUT`.
    TimedOut,
}

/// What the body of the version reply gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VersionBody {
    Empty,
    Unparsed,
    NoVersionInfo,
    VersionInfo,
}

impl VersionAnswer {
    /// The debug line's text: how the request ended, `elapsed` after it was
    /// sent, with `others` other messages read meanwhile.
    fn describe(&self, elapsed: std::time::Duration, others: usize) -> String {
        let ms = elapsed.as_millis();
        let others = if others == 1 { "1 other message".to_string() } else { format!("{others} other messages") };
        match self {
            Self::Reply { code, body } => {
                let body = match body {
                    VersionBody::Empty => "no body",
                    VersionBody::Unparsed => "the body did not parse",
                    VersionBody::NoVersionInfo => "no VersionInfo in the body",
                    VersionBody::VersionInfo => "VersionInfo read",
                };
                format!("version reply code {code} after {ms} ms ({others}), {body}")
            }
            Self::NotSent(e) => format!("version request not sent: {e}"),
            Self::RecvError(e) => format!("receive error after {ms} ms ({others}): {e}"),
            Self::OnlyOthers => format!("no version reply among {others} ({ms} ms)"),
            Self::TimedOut => format!("no version reply within {} s ({others})", IDENTITY_TIMEOUT.as_secs()),
        }
    }
}

/// How long the session may carry no BC message before `wait_for_pushes`
/// sends `MSG_ID_LINK_TYPE`, and then how often while none comes: about the
/// official app's own period while it streams nothing. A Home Hub Pro ends a
/// session that carried no BC message for about 62 s (BCUDP acks do not
/// count, device pushes do).
const IDLE_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(6);

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
    /// When `wait_for_pushes` last sent `MSG_ID_LINK_TYPE`, so that a device
    /// leaving it unanswered gets the next one `IDLE_KEEPALIVE` later, not
    /// at once.
    keepalive_sent: Option<tokio::time::Instant>,
    /// The number the subscription (`MSG_ID_SUBSCRIBE`, sent by `identity`)
    /// went out under: the device sends its alarm events (33) and a few
    /// other pushes under it for as long as the session lasts, so
    /// `next_msg_num` does not give it out again.
    subscribe_msg_num: Option<u16>,
    /// The number the last `start_video` asked under, which its stream's
    /// data carry: some may still come in after `stop_video`, so
    /// `next_msg_num` does not give it out until the next stream starts.
    live_msg_num: Option<u16>,
    /// When the device accepted the login, for the time the negotiation
    /// debug lines start with (see `debug_prefix`).
    logged_in_at: Option<std::time::Instant>,
    /// This connection's number in the negotiation debug lines (see
    /// `debug_prefix`).
    debug_id: u32,
    /// Which P2P path won the connect race ("local", "NAT-mapped" or
    /// "relay"), for the negotiation debug output; `None` for a connection
    /// made another way.
    p2p_path: Option<&'static str>,
}

/// What an in-flight request was asking for: a `MSG_ID_IMAGE_FILE` request
/// (see its own doc comment), one read of a settings page, or one settings
/// write.
#[derive(Debug, Clone)]
enum ImageKind {
    MonitorPoint,
    Preset(u8),
    /// A live snapshot taken for this preset (Add / Edit's picture).
    Snapshot(u8),
    /// One read of a settings page; the collector gathers the page's
    /// replies (see `client::settings`).
    SettingsPage(Arc<std::sync::Mutex<settings::PageCollector>>),
    /// One write of a group; the collector gathers the device's codes.
    SettingsWrite(Arc<std::sync::Mutex<settings::WriteCollector>>),
}

#[derive(Debug, Clone)]
struct PlayingStream {
    channel_id: u8,
    handle: u32,
    stream_name: &'static str,
    bc_stream_type: u8,
}

/// `from`, or else the first number after it (65535 is followed by 0) that
/// is neither 0 nor one of `reserved`; see `ReolinkClient::next_msg_num`.
fn free_msg_num(from: u16, reserved: [Option<u16>; 2]) -> u16 {
    let mut n = from;
    while n == 0 || reserved.contains(&Some(n)) {
        n = n.wrapping_add(1);
    }
    n
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
            // this project reads from it yet. It opens a transfer: what an
            // unfinished one left under its number goes, once the number has
            // been given out again (see `ReolinkClient::next_msg_num`). Until
            // then nothing is kept under it here: in every capture of ours
            // this reply is the first answer under its number.
            image_chunks.remove(&bc.meta.msg_num);
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
/// UID). The same for serial numbers, MAC addresses, user names, passwords
/// and tokens, and for the `<secretCode>` and `<bootSecret>` of a login
/// reply.
fn redact_identifiers(text: &str) -> String {
    let mut out = text.to_string();
    let secrets = ["serialNumber", "userName", "validpassword", "password", "mac", "token", "secretCode", "bootSecret"];
    for tag in ["uid", "fakeUid", "UID", "Id", "fileId"].into_iter().chain(secrets) {
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

/// Numbers this process's connections for the negotiation debug lines (see
/// `debug_prefix`): every device connects at start, side by side, and the
/// number tells their lines apart without naming the device.
static NEXT_DEBUG_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// What the negotiation and identity debug lines start with: the
/// connection's number and the time since the device accepted the login
/// ("[conn 2 +812 ms] "), or the number alone before that.
fn debug_prefix(debug_id: u32, logged_in_at: Option<std::time::Instant>) -> String {
    match logged_in_at {
        Some(at) => format!("[conn {debug_id} +{} ms] ", at.elapsed().as_millis()),
        None => format!("[conn {debug_id}] "),
    }
}

/// What live video's reader left out of an earlier stream before its own
/// stream's first message, for the negotiation debug line: the messages,
/// and their bytes as their headers count them (Extension and payload).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SkippedTail {
    messages: u32,
    bytes: usize,
}

impl SkippedTail {
    fn add(&mut self, bc: &Bc) {
        let len = match &bc.body {
            BcBody::Modern(ModernMsg { extension_xml, payload }) => {
                extension_xml.as_ref().map_or(0, Vec::len) + payload.as_ref().map_or(0, Vec::len)
            }
            BcBody::Legacy(_) => 0,
        };
        self.messages = self.messages.saturating_add(1);
        self.bytes = self.bytes.saturating_add(len);
    }

    /// The line, or `None` when nothing was left out.
    fn describe(&self) -> Option<String> {
        let s = if self.messages == 1 { "" } else { "s" };
        (self.messages > 0)
            .then(|| format!("live: skipped {} message{s} ({} bytes) of an earlier stream", self.messages, self.bytes))
    }
}

/// The login reply's stream list for the negotiation debug output: per
/// `StreamInfo`, the channels it is for (a bit mask) and each of its tables'
/// stream type, size and frame rate; nothing else of the reply.
fn describe_streams(profiles: &[crate::encoding::EncodeProfile]) -> String {
    if profiles.is_empty() {
        return "no stream list".to_string();
    }
    let info = |profile: &crate::encoding::EncodeProfile| {
        let tables: Vec<String> = profile
            .tables
            .iter()
            .map(|t| format!("{} {}x{} {} fps", t.stream_type, t.width, t.height, t.default_frame_rate))
            .collect();
        let tables = if tables.is_empty() { "no tables".to_string() } else { tables.join(", ") };
        format!("channels {:#x}: {tables}", profile.channel_bits)
    };
    profiles.iter().map(info).collect::<Vec<_>>().join("; ")
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
        let (peer, path): (PeerHandle, _) = connect_by_uid_with_path(&socket, uid).await?;
        let connection = BcConnection::new(socket, peer);
        let keepalive = connection.spawn_direct_keepalive();
        let mut client = Self::from_connection(connection, EncryptionProtocol::Unencrypted);
        client.direct_keepalive_task = keepalive;
        client.p2p_path = Some(path);
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
            keepalive_sent: None,
            subscribe_msg_num: None,
            live_msg_num: None,
            logged_in_at: None,
            debug_id: NEXT_DEBUG_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            p2p_path: None,
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

    /// The number for the next request: 1, 2, 3, ... and, after 65535, round
    /// again from 1. Once round, it skips the numbers the device sends under
    /// without being asked: 0, the number of its pushes, and the
    /// subscription's (both in every capture of ours), and also the last
    /// live stream's, until the next one starts. Until then none of these
    /// can come up (0 is never reached, the others were given out earlier),
    /// so the numbers are the same as without the skip.
    fn next_msg_num(&mut self) -> u16 {
        let reserved = [self.subscribe_msg_num, self.live_msg_num];
        let n = free_msg_num(self.next_msg_num, reserved);
        self.next_msg_num = n.wrapping_add(1);
        // The number comes round again after about 65535 requests: an
        // unanswered settings read left under it must not take this
        // request's reply.
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
            let reply = self.recv_login_reply(msg_num).await?;
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
        let reply = self.recv_login_reply(msg_num).await?;
        if reply.meta.response_code != 200 {
            return Err(Error::LoginFailed { code: reply.meta.response_code });
        }
        self.logged_in_at = Some(std::time::Instant::now());
        let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = reply.body else {
            return Err(Error::ProtocolError("expected a DeviceInfo reply".to_string()));
        };
        if std::env::var("REOLING_PROBE_PUSH").is_ok() {
            let text: String = redact_identifiers(&String::from_utf8_lossy(&payload)).chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
            eprintln!("PROBE login reply: {text}");
        }
        self.encode_profiles = crate::encoding::EncodeProfile::parse_list(&String::from_utf8_lossy(&payload));
        if std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
            let at = debug_prefix(self.debug_id, self.logged_in_at);
            if let Some(path) = self.p2p_path {
                eprintln!("{at}NEGOTIATION p2p path: {path}");
            }
            eprintln!("{at}DEBUG login streams: {}", describe_streams(&self.encode_profiles));
        }
        let device_info = BcXml::from_bytes(&payload)?.device_info.unwrap_or_default();
        Ok(DeviceInfoSummary {
            resolution_name: device_info.resolution.and_then(|r| r.name),
        })
    }

    /// Reads until the reply to a login step: `MSG_ID_LOGIN` under the
    /// request's number, as every login reply in our captures is. A device
    /// can push before it answers: our RLN36 on firmware v3.5 sent an alarm
    /// push (33 under 0) ahead of its nonce reply in one run of 14, and the
    /// login took it for the reply. Other messages are passed on like any
    /// push and read past, for at most `LOGIN_READ_LIMIT` messages, the reply
    /// counted, and `LOGIN_REPLY_TIMEOUT` for the whole step.
    async fn recv_login_reply(&self, msg_num: u16) -> crate::Result<Bc> {
        let mut image_chunks = HashMap::new();
        let read = async {
            for _ in 0..LOGIN_READ_LIMIT {
                let reply = self.connection.lock().await.recv_bc(&self.encryption).await?;
                if reply.meta.msg_id == MSG_ID_LOGIN && reply.meta.msg_num == msg_num {
                    return Ok(reply);
                }
                if std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
                    let at = debug_prefix(self.debug_id, None);
                    eprintln!("{at}DEBUG login: read past message {} under {}", reply.meta.msg_id, reply.meta.msg_num);
                }
                let update = pushed_update(&reply, &mut image_chunks, &self.pending_images, &self.channel_uids);
                if let Some(update) = update {
                    let _ = self.channel_updates_tx.send(update);
                }
            }
            Err(Error::ProtocolError(format!("no login reply among {LOGIN_READ_LIMIT} messages")))
        };
        tokio::time::timeout(LOGIN_REPLY_TIMEOUT, read).await.map_err(|_| Error::ReplyTimeout)?
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

    /// Sends a bodiless request; returns the number it went out under.
    async fn send_empty_request(&mut self, msg_id: u32) -> crate::Result<u16> {
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
        self.connection.lock().await.send_bc(&request, &self.encryption).await.map(|()| msg_num)
    }

    /// Asks the logged-in device for its name and model. Best effort: any
    /// failure or silence yields an empty identity rather than an error.
    pub async fn identity(&mut self) -> DeviceIdentity {
        // What the official app sends first after login (seen in a capture
        // against a Home Hub, and confirmed by a probe: the Hub pushes its
        // channel list, with the channels' names, only after these two).
        // Answers are picked up as they come.
        let _ = self.send_empty_request(MSG_ID_STREAM_INFO).await;
        if let Ok(msg_num) = self.send_empty_request(MSG_ID_SUBSCRIBE).await {
            self.subscribe_msg_num = Some(msg_num);
        }
        // Which camera can do what: the support table, one row per channel.
        self.request_for_channel(MSG_ID_SUPPORT, 0).await;
        let asked = std::time::Instant::now();
        let (info, answer, others) = self.ask_version().await;
        let (name, model) = info.map(|i| (i.name, i.model)).unwrap_or_default();
        if std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
            let at = debug_prefix(self.debug_id, self.logged_in_at);
            eprintln!("{at}DEBUG identity version: {}", answer.describe(asked.elapsed(), others));
            eprintln!("{at}DEBUG identity reply: name={name:?} model={model:?}");
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

    /// Sends the version request (80) and reads until its reply, for at most
    /// `IDENTITY_TIMEOUT` and 16 other messages, passing pushes on. Returns
    /// the `VersionInfo` of the reply, how the request ended and how many
    /// other messages came meanwhile. A reply's body is read whatever its
    /// code: our Hub and NVR answer some requests with 300 and a full body.
    async fn ask_version(&mut self) -> (Option<VersionInfo>, VersionAnswer, usize) {
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
        let mut answer = VersionAnswer::TimedOut;
        let mut others = 0;
        let exchange = async {
            if let Err(e) = self.connection.lock().await.send_bc(&request, &self.encryption).await {
                answer = VersionAnswer::NotSent(e);
                return;
            }
            let mut image_chunks = HashMap::new();
            for _ in 0..16 {
                let reply = match self.connection.lock().await.recv_bc(&self.encryption).await {
                    Ok(reply) => reply,
                    Err(e) => {
                        answer = VersionAnswer::RecvError(e);
                        return;
                    }
                };
                if let Some(update) = pushed_update(&reply, &mut image_chunks, &self.pending_images, &self.channel_uids) {
                    let _ = self.channel_updates_tx.send(update);
                    others += 1;
                    continue;
                }
                if reply.meta.msg_id != MSG_ID_VERSION {
                    others += 1;
                    continue;
                }
                let body = match reply.body {
                    BcBody::Modern(ModernMsg { payload: Some(payload), .. }) => match BcXml::from_bytes(&payload) {
                        Ok(xml) => {
                            info = xml.version_info;
                            if info.is_some() {
                                VersionBody::VersionInfo
                            } else {
                                VersionBody::NoVersionInfo
                            }
                        }
                        Err(_) => VersionBody::Unparsed,
                    },
                    _ => VersionBody::Empty,
                };
                answer = VersionAnswer::Reply { code: reply.meta.response_code, body };
                return;
            }
            answer = VersionAnswer::OnlyOthers;
        };
        let _ = tokio::time::timeout(IDENTITY_TIMEOUT, exchange).await;
        (info, answer, others)
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
        // Picked while the last stream's number is still kept out.
        let msg_num = self.next_msg_num();
        self.live_msg_num = Some(msg_num);
        // handle/stream_type per quality. What the captures we decoded show
        // (the official app's against our Home Hub and NVR, and Reoling's
        // own): the official app asks for the sub stream with XML handle
        // 256 + channel, streamType "subStream" and a
        // <policy>newestIFrame</policy> that Reoling does not send. It
        // leaves msg_num at 0 and counts its requests in the header bytes
        // Reoling uses for the channel and the stream type, so it says
        // nothing about BcMeta.stream_type. Reoling's own `Sub` request
        // below (handle 256 on every channel, BcMeta.stream_type 1,
        // "subStream", no policy) is answered 200 by the Hub on channels 0
        // to 2 and by the NVR on channel 0. Every Preview in them is for the
        // sub stream: `Main`'s and `Extern`'s values are not confirmed by
        // these captures.
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
                    "{}NEGOTIATION preview request: channel_id={} stream_type={} class={:#x} xml={}",
                    debug_prefix(self.debug_id, self.logged_in_at),
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
                "{}NEGOTIATION preview reply: response_code={} class={:#x} body={:?}",
                debug_prefix(self.debug_id, self.logged_in_at),
                ack.meta.response_code,
                ack.meta.class,
                ack.body
            );
        }
        if ack.meta.response_code != 200 {
            return Err(Error::StreamRefused { stream: quality, code: ack.meta.response_code });
        }

        let (tx, rx) = channel(32);
        let connection = Arc::clone(&self.connection);
        let encryption = self.encryption.clone();
        let channel_updates = self.channel_updates_tx.clone();
        let audio = self.audio_tx.clone();
        let pending_images = Arc::clone(&self.pending_images);
        let channel_uids = Arc::clone(&self.channel_uids);
        let (debug_id, logged_in_at) = (self.debug_id, self.logged_in_at);
        self.video_task = Some(tokio::spawn(async move {
            let mut buffer: Vec<u8> = Vec::new();
            let mut guard = MediaGuard::default();
            let mut image_chunks = HashMap::new();
            // Counted for the debug line until this stream's first message.
            let mut skipped = debug_negotiation.then(SkippedTail::default);
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
                if bc.meta.msg_num != msg_num {
                    // An earlier stream's tail. The device sends each stream
                    // under its own Preview's number, and what the last one
                    // still had in flight comes after this one's reply (in
                    // our captures, up to 0.7 s of it): another codec, or
                    // another camera. Taken for this stream's, its first
                    // frame would pick the decoder. Pushes and replies were
                    // passed on above.
                    if let Some(skipped) = &mut skipped {
                        skipped.add(&bc);
                    }
                    continue;
                }
                if let Some(line) = skipped.take().and_then(|skipped| skipped.describe()) {
                    eprintln!("{}NEGOTIATION {line}", debug_prefix(debug_id, logged_in_at));
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
                                    let at = debug_prefix(debug_id, logged_in_at);
                                    eprintln!("{at}NEGOTIATION stream info unit: {width}x{height}");
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
                            // Not the start of a unit. Skip to the next unit
                            // instead of ending the stream. (The tail of the
                            // stream we just switched away from, once the
                            // usual cause, is left out above by its number.)
                            let skip = crate::protocol::bcmedia::model::resync_offset(&buffer);
                            if std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
                                let at = debug_prefix(debug_id, logged_in_at);
                                eprintln!("{at}NEGOTIATION media resync: {e}; skipped {skip} bytes");
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
    /// Cancel-safe: partial messages are kept by the connection. Dropped
    /// while sending the keep-alive, it skips that one or uses up a message
    /// number; nothing else is lost.
    ///
    /// Once the connection has carried no BC message for `IDLE_KEEPALIVE`,
    /// sends a bodiless `MSG_ID_LINK_TYPE`, and another each
    /// `IDLE_KEEPALIVE` for as long as nothing comes, as the official app
    /// does while it streams nothing: a Home Hub ends a session that stays
    /// silent for about a minute. Any message (the answer, a push, data of
    /// another reader) starts the wait over; the answer itself is dropped.
    pub async fn wait_for_pushes(&mut self) -> crate::Error {
        // While a playback runs, its reader is the one reading (and passes
        // the pushes on); reading here as well would take its data.
        while self.replay_task.as_ref().is_some_and(|t| !t.is_finished()) || self.download_running() {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let mut image_chunks = HashMap::new();
        loop {
            // The quiet time is kept by the connection and the client, not
            // by this call: the caller drops it and calls again whenever
            // something else needs the client.
            let received = {
                let mut connection = self.connection.lock().await;
                let quiet_since = match self.keepalive_sent {
                    Some(sent) => sent.max(connection.last_received()),
                    None => connection.last_received(),
                };
                tokio::time::timeout_at(quiet_since + IDLE_KEEPALIVE, connection.recv_bc(&self.encryption)).await
            };
            match received {
                Ok(Ok(bc)) => {
                    if let Some(update) = pushed_update(&bc, &mut image_chunks, &self.pending_images, &self.channel_uids) {
                        let _ = self.channel_updates_tx.send(update);
                    }
                }
                Ok(Err(Error::ReplyTimeout)) => {}
                Ok(Err(e)) => return e,
                Err(_) => {
                    // Under a number of its own, like every request (a
                    // settings answer is routed by its number). A send that
                    // fails is not retried before the next period.
                    let _ = self.send_empty_request(MSG_ID_LINK_TYPE).await;
                    self.keepalive_sent = Some(tokio::time::Instant::now());
                }
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
    fn diagnostic_text_hides_serial_numbers_addresses_and_secrets() {
        let text = "<DeviceInfo><serialNumber>TESTSERIAL</serialNumber><mac>TESTMAC</mac>\
                    <secretCode>TESTSECRETCODE</secretCode><bootSecret>TESTBOOTSECRET</bootSecret>\
                    <type>TESTMODEL</type></DeviceInfo><User><userName>TESTUSER</userName>\
                    <password>TESTPASSWORD</password><validpassword>TESTVALID</validpassword></User>\
                    <token>TESTTOKEN</token>";
        let shown = redact_identifiers(text);
        for placeholder in ["SERIAL", "MAC", "SECRETCODE", "BOOTSECRET", "USER", "PASSWORD", "VALID", "TOKEN"] {
            assert!(!shown.contains(&format!("TEST{placeholder}")), "{placeholder}: {shown}");
        }
        assert_eq!(shown.matches("<hidden>").count(), 8);
        // Not an identifier: the model stays.
        assert!(shown.contains("<type>TESTMODEL</type>"));
    }

    #[test]
    fn the_identity_debug_line_says_how_the_version_request_ended() {
        let ms = std::time::Duration::from_millis;
        let reply = |code, body| VersionAnswer::Reply { code, body };
        assert_eq!(
            reply(200, VersionBody::VersionInfo).describe(ms(143), 2),
            "version reply code 200 after 143 ms (2 other messages), VersionInfo read"
        );
        assert_eq!(
            reply(400, VersionBody::Empty).describe(ms(90), 0),
            "version reply code 400 after 90 ms (0 other messages), no body"
        );
        assert_eq!(
            reply(300, VersionBody::Unparsed).describe(ms(71), 1),
            "version reply code 300 after 71 ms (1 other message), the body did not parse"
        );
        assert_eq!(
            reply(200, VersionBody::NoVersionInfo).describe(ms(5), 0),
            "version reply code 200 after 5 ms (0 other messages), no VersionInfo in the body"
        );
        assert_eq!(
            VersionAnswer::TimedOut.describe(ms(2001), 3),
            "no version reply within 2 s (3 other messages)"
        );
        assert_eq!(
            VersionAnswer::OnlyOthers.describe(ms(480), 16),
            "no version reply among 16 other messages (480 ms)"
        );
        assert_eq!(
            VersionAnswer::RecvError(Error::ConnectionLost).describe(ms(12), 1),
            "receive error after 12 ms (1 other message): connection lost"
        );
        assert_eq!(
            VersionAnswer::NotSent(Error::ConnectionLost).describe(ms(0), 0),
            "version request not sent: connection lost"
        );
    }

    #[test]
    fn the_debug_prefix_gives_the_connection_and_the_time_since_login() {
        assert_eq!(debug_prefix(3, None), "[conn 3] ");
        let at = std::time::Instant::now().checked_sub(std::time::Duration::from_millis(812)).unwrap();
        let shown = debug_prefix(3, Some(at));
        let ms = shown.strip_prefix("[conn 3 +").and_then(|rest| rest.strip_suffix(" ms] "));
        assert!(ms.and_then(|ms| ms.parse::<u128>().ok()).is_some_and(|ms| ms >= 812), "{shown}");
    }

    #[test]
    fn the_live_debug_line_counts_what_was_left_out_of_an_earlier_stream() {
        let mut skipped = SkippedTail::default();
        assert_eq!(skipped.describe(), None);
        let message = |extension_xml: Option<Vec<u8>>, payload: Vec<u8>| Bc {
            meta: BcMeta {
                msg_id: MSG_ID_VIDEO,
                channel_id: 2,
                stream_type: 0,
                msg_num: 16,
                response_code: 200,
                class: 0x0000,
            },
            body: BcBody::Modern(ModernMsg { extension_xml, payload: Some(payload) }),
        };
        let binary = Extension { version: XML_VERSION.to_string(), binary_data: Some(1), ..Default::default() };
        let first = message(Some(binary.to_bytes()), vec![0; 600]);
        skipped.add(&first);
        // The bytes as the message's header counts them.
        let wire = crate::protocol::bc::codec::write_bc(&first, &EncryptionProtocol::Unencrypted);
        let (header, _) = crate::protocol::bc::header_codec::read_header(&wire).unwrap().unwrap();
        assert_eq!(skipped.bytes, header.body_len as usize);
        let one = format!("live: skipped 1 message ({} bytes) of an earlier stream", header.body_len);
        assert_eq!(skipped.describe(), Some(one));
        skipped.add(&message(None, vec![0; 63]));
        let two = format!("live: skipped 2 messages ({} bytes) of an earlier stream", header.body_len + 63);
        assert_eq!(skipped.describe(), Some(two));
    }

    #[test]
    fn the_login_streams_line_gives_type_size_and_frame_rate_per_table() {
        use crate::encoding::{EncodeProfile, EncodeTable};
        let table = |stream_type: &str, width, height, default_frame_rate| EncodeTable {
            stream_type: stream_type.to_string(),
            width,
            height,
            default_frame_rate,
            default_bitrate: 1024,
            frame_rates: vec![25, 20, 15],
            bitrates: vec![1024, 2048],
            ..Default::default()
        };
        let profiles = [
            EncodeProfile {
                channel_bits: 1,
                tables: vec![table("mainStream", 1920, 1080, 15), table("subStream", 640, 360, 10)],
            },
            EncodeProfile { channel_bits: 6, tables: vec![table("externStream", 1280, 720, 25)] },
        ];
        assert_eq!(
            describe_streams(&profiles),
            "channels 0x1: mainStream 1920x1080 15 fps, subStream 640x360 10 fps; \
             channels 0x6: externStream 1280x720 25 fps"
        );
        assert_eq!(describe_streams(&[]), "no stream list");
        let bare = EncodeProfile { channel_bits: 1, tables: Vec::new() };
        assert_eq!(describe_streams(&[bare]), "channels 0x1: no tables");
        // From a login reply's text, as `login` reads it.
        let reply = "<body><DeviceInfo><serialNumber>TESTSERIAL</serialNumber></DeviceInfo><StreamInfoList>\
                     <StreamInfo><channelBits>1</channelBits><encodeTable><type>subStream</type><resolution>\
                     <width>640</width><height>360</height></resolution><defaultFramerate>15</defaultFramerate>\
                     </encodeTable></StreamInfo></StreamInfoList></body>";
        let shown = describe_streams(&EncodeProfile::parse_list(reply));
        assert_eq!(shown, "channels 0x1: subStream 640x360 15 fps");
        let without = "<body><DeviceInfo></DeviceInfo></body>";
        assert_eq!(describe_streams(&EncodeProfile::parse_list(without)), "no stream list");
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

#[cfg(test)]
mod keepalive_tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::UdpSocket;
    use tokio::time::Instant;
    use tokio_stream::StreamExt;

    const PLAIN: EncryptionProtocol = EncryptionProtocol::Unencrypted;

    /// A client and the device end of a loopback BCUDP connection, both
    /// unencrypted (no login), relay-style (no `C2D_HB`).
    async fn client_and_device() -> (ReolinkClient, BcConnection) {
        client_and_device_with(EncryptionProtocol::Unencrypted).await
    }

    /// The same, with the client encrypting as `encryption` says.
    async fn client_and_device_with(encryption: EncryptionProtocol) -> (ReolinkClient, BcConnection) {
        client_and_device_with_nonce(encryption, None).await
    }

    /// The same, with the login nonce the P2P handshake gave the client, if
    /// it gave one.
    async fn client_and_device_with_nonce(
        encryption: EncryptionProtocol,
        nonce: Option<&str>,
    ) -> (ReolinkClient, BcConnection) {
        let client_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let device_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let client_addr = client_socket.local_addr().unwrap();
        let device_addr = device_socket.local_addr().unwrap();
        let peer = |addr, local_connection_id, remote_connection_id, nonce: Option<&str>| PeerHandle {
            addr,
            local_connection_id,
            remote_connection_id,
            nonce: nonce.map(str::to_string),
            is_direct: false,
        };
        let device = BcConnection::new(device_socket, peer(client_addr, 2, 1, None));
        let connection = BcConnection::new(client_socket, peer(device_addr, 1, 2, nonce));
        let client = ReolinkClient::from_connection(connection, encryption);
        (client, device)
    }

    fn device_message(
        msg_id: u32,
        msg_num: u16,
        code: u16,
        extension: Option<Vec<u8>>,
        payload: Option<Vec<u8>>,
    ) -> Bc {
        Bc {
            meta: BcMeta { msg_id, channel_id: 0, stream_type: 0, msg_num, response_code: code, class: 0x0000 },
            body: BcBody::Modern(ModernMsg { extension_xml: extension, payload }),
        }
    }

    /// The answer as captured: 200 and a `LinkType` body naming the link,
    /// 109 bytes long like the captured one.
    fn link_type_reply(msg_num: u16) -> Bc {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<LinkType version=\"1.1\">\n\
                   <type>LAN</type>\n</LinkType>\n</body>\n";
        assert_eq!(xml.len(), 109);
        device_message(MSG_ID_LINK_TYPE, msg_num, 200, None, Some(xml.as_bytes().to_vec()))
    }

    /// An alarm-event push (33), which nothing in the client reads.
    fn alarm_push() -> Bc {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<AlarmEventList version=\"1.1\">\n\
                   </AlarmEventList>\n</body>\n";
        device_message(33, 0, 200, None, Some(xml.as_bytes().to_vec()))
    }

    /// Reoling's own bodiless request: channel 0, no Extension, no body.
    fn assert_link_type_request(bc: &Bc) {
        assert_eq!(bc.meta.msg_id, MSG_ID_LINK_TYPE);
        assert_eq!((bc.meta.channel_id, bc.meta.stream_type, bc.meta.class), (0, 0, 0x6414));
        assert_ne!(bc.meta.msg_num, 0);
        assert!(matches!(&bc.body, BcBody::Modern(ModernMsg { extension_xml: None, payload: None })), "{:?}", bc.body);
    }

    #[track_caller]
    fn assert_about_6_s(from: Instant, to: Instant) {
        let waited = to - from;
        assert!(waited >= IDLE_KEEPALIVE && waited < IDLE_KEEPALIVE + Duration::from_millis(500), "{waited:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_session_asks_for_the_link_type_every_6_s() {
        let (mut client, mut device) = client_and_device().await;
        let mut updates = client.take_channel_updates().unwrap();
        let start = Instant::now();
        let fake = async move {
            let first = device.recv_bc(&PLAIN).await.unwrap();
            assert_about_6_s(start, Instant::now());
            assert_link_type_request(&first);
            device.send_bc(&link_type_reply(first.meta.msg_num), &PLAIN).await.unwrap();
            let replied = Instant::now();

            let second = device.recv_bc(&PLAIN).await.unwrap();
            let asked = Instant::now();
            assert_about_6_s(replied, asked);
            assert_link_type_request(&second);
            assert_eq!(second.meta.msg_num, first.meta.msg_num + 1);

            // Left unanswered, the next one still waits its 6 s, counted
            // from when the client sent the last one (which reached the
            // device a little later).
            let third = device.recv_bc(&PLAIN).await.unwrap();
            assert_about_6_s(asked - Duration::from_millis(100), Instant::now());
            assert_link_type_request(&third);
        };
        tokio::select! {
            error = client.wait_for_pushes() => panic!("wait_for_pushes ended: {error}"),
            () = fake => {}
        }
        assert!(updates.try_recv().is_err(), "the answer is dropped");
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_sent_while_messages_come_more_often_than_every_6_s() {
        let (mut client, mut device) = client_and_device().await;
        let fake = async move {
            for _ in 0..8 {
                let quiet = tokio::time::timeout(Duration::from_secs(5), device.recv_bc(&PLAIN)).await;
                assert!(quiet.is_err(), "sent while pushes came: {:?}", quiet.map(|r| r.map(|bc| bc.meta)));
                device.send_bc(&alarm_push(), &PLAIN).await.unwrap();
            }
            let pushed = Instant::now();
            let asked = device.recv_bc(&PLAIN).await.unwrap();
            assert_about_6_s(pushed, Instant::now());
            assert_link_type_request(&asked);
        };
        tokio::select! {
            error = client.wait_for_pushes() => panic!("wait_for_pushes ended: {error}"),
            () = fake => {}
        }
    }

    /// The caller (the device's task in the UI) drops `wait_for_pushes` and
    /// calls it again whenever another of its events comes; that must not
    /// put the request off.
    #[tokio::test(start_paused = true)]
    async fn calling_again_does_not_start_the_wait_over() {
        let (mut client, mut device) = client_and_device().await;
        let start = Instant::now();
        let fake = async move {
            let asked = device.recv_bc(&PLAIN).await.unwrap();
            assert_about_6_s(start, Instant::now());
            assert_link_type_request(&asked);
        };
        let restarted = async {
            loop {
                let _ = tokio::time::timeout(Duration::from_millis(700), client.wait_for_pushes()).await;
            }
        };
        tokio::select! {
            () = restarted => unreachable!(),
            () = fake => {}
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_message_another_reader_took_counts_too() {
        let (mut client, mut device) = client_and_device().await;
        tokio::time::sleep(Duration::from_secs(4)).await;
        device.send_bc(&alarm_push(), &PLAIN).await.unwrap();
        // As live video's reader, or a search waiting for its answer, would.
        client.connection.lock().await.recv_bc(&PLAIN).await.unwrap();
        let read = Instant::now();
        let fake = async move {
            let asked = device.recv_bc(&PLAIN).await.unwrap();
            assert_about_6_s(read, Instant::now());
            assert_link_type_request(&asked);
        };
        tokio::select! {
            error = client.wait_for_pushes() => panic!("wait_for_pushes ended: {error}"),
            () = fake => {}
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_image_whose_chunks_are_more_than_6_s_apart_is_still_put_together() {
        let (mut client, mut device) = client_and_device().await;
        let mut updates = client.take_channel_updates().unwrap();
        client.query_monitor_point_image(0).await;
        let chunk = |msg_num: u16, code: u16, bytes: Option<&[u8]>| {
            let binary = Extension { version: XML_VERSION.to_string(), binary_data: Some(1), ..Default::default() };
            device_message(MSG_ID_IMAGE_FILE, msg_num, code, Some(binary.to_bytes()), bytes.map(<[u8]>::to_vec))
        };
        let fake = async move {
            let request = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(request.meta.msg_id, MSG_ID_IMAGE_FILE);
            let image = request.meta.msg_num;
            device.send_bc(&chunk(image, 200, Some(&[0xff, 0xd8, 1, 2])), &PLAIN).await.unwrap();
            let first = Instant::now();
            let asked = device.recv_bc(&PLAIN).await.unwrap();
            assert_about_6_s(first, Instant::now());
            assert_link_type_request(&asked);
            device.send_bc(&link_type_reply(asked.meta.msg_num), &PLAIN).await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
            device.send_bc(&chunk(image, 200, Some(&[3, 4, 0xff, 0xd9])), &PLAIN).await.unwrap();
            // Any code other than 200 ends the transfer.
            device.send_bc(&chunk(image, 201, None), &PLAIN).await.unwrap();
            match updates.recv().await {
                Some(DeviceUpdate::MonitorPointImage { channel_id: 0, jpeg }) => {
                    assert_eq!(jpeg, vec![0xff, 0xd8, 1, 2, 3, 4, 0xff, 0xd9]);
                }
                other => panic!("expected the image, got {other:?}"),
            }
        };
        tokio::select! {
            error = client.wait_for_pushes() => panic!("wait_for_pushes ended: {error}"),
            () = fake => {}
        }
    }

    /// 30 s without a request while the task behind `end` runs, then one at
    /// once when it ends: nothing has come for far more than 6 s by then (in
    /// a real session the task's own data count).
    async fn quiet_until_ended(mut client: ReolinkClient, mut device: BcConnection, end: tokio::task::AbortHandle) {
        let fake = async move {
            // `recv_bc` gives up after 15 s with nothing at all.
            for _ in 0..2 {
                assert!(matches!(device.recv_bc(&PLAIN).await, Err(Error::ReplyTimeout)), "sent while it ran");
            }
            end.abort();
            let asked = tokio::time::timeout(Duration::from_secs(1), device.recv_bc(&PLAIN)).await;
            assert_link_type_request(&asked.expect("asked once it had ended").unwrap());
        };
        tokio::select! {
            error = client.wait_for_pushes() => panic!("wait_for_pushes ended: {error}"),
            () = fake => {}
        }
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_sent_while_a_playback_runs() {
        let (mut client, device) = client_and_device().await;
        let playback = tokio::spawn(std::future::pending::<()>());
        let end = playback.abort_handle();
        client.replay_task = Some(playback);
        quiet_until_ended(client, device, end).await;
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_sent_while_a_download_runs() {
        let (mut client, device) = client_and_device().await;
        let download = tokio::spawn(std::future::pending::<replay::DownloadSummary>());
        let end = download.abort_handle();
        client.download_task = Some(download);
        quiet_until_ended(client, device, end).await;
    }

    #[test]
    fn a_free_number_is_never_0_nor_a_reserved_one() {
        assert_eq!(free_msg_num(1, [None, None]), 1);
        assert_eq!(free_msg_num(65534, [Some(3), Some(6)]), 65534);
        assert_eq!(free_msg_num(65535, [Some(3), Some(6)]), 65535);
        assert_eq!(free_msg_num(0, [None, None]), 1);
        assert_eq!(free_msg_num(0, [Some(1), None]), 2);
        assert_eq!(free_msg_num(0, [None, Some(1)]), 2);
        assert_eq!(free_msg_num(0, [Some(1), Some(2)]), 3);
        assert_eq!(free_msg_num(3, [Some(3), None]), 4);
        assert_eq!(free_msg_num(3, [Some(3), Some(4)]), 5);
        assert_eq!(free_msg_num(65535, [Some(65535), None]), 1);
        assert_eq!(free_msg_num(65535, [Some(65535), Some(1)]), 2);
    }

    /// Until the count comes round, the numbers are 1 to 65535 in order
    /// whatever is reserved; then 0, the subscription's number and the live
    /// stream's are left out.
    #[tokio::test]
    async fn the_count_is_1_to_65535_and_then_goes_round_past_the_reserved_numbers() {
        let (mut client, _device) = client_and_device().await;
        for expected in 1..=u16::MAX {
            let n = client.next_msg_num();
            assert_eq!(n, expected);
            // As `identity` and `start_video` keep them.
            match n {
                3 => client.subscribe_msg_num = Some(n),
                6 => client.live_msg_num = Some(n),
                _ => {}
            }
        }
        let round: Vec<u16> = (0..5).map(|_| client.next_msg_num()).collect();
        assert_eq!(round, [1, 2, 4, 5, 7]);
        // The last stream's number stays out after a stop (which keeps it),
        // until the next stream starts under another one.
        client.next_msg_num = 6;
        assert_eq!(client.next_msg_num(), 7);
        client.live_msg_num = Some(9);
        client.next_msg_num = 6;
        assert_eq!(client.next_msg_num(), 6);
    }

    #[tokio::test]
    async fn a_count_at_65535_goes_on_with_1_or_the_next_free_number() {
        let (mut client, _device) = client_and_device().await;
        client.next_msg_num = u16::MAX;
        let round: Vec<u16> = (0..3).map(|_| client.next_msg_num()).collect();
        assert_eq!(round, [65535, 1, 2]);
        client.next_msg_num = u16::MAX;
        client.subscribe_msg_num = Some(1);
        let round: Vec<u16> = (0..3).map(|_| client.next_msg_num()).collect();
        assert_eq!(round, [65535, 2, 3]);
        client.next_msg_num = u16::MAX;
        client.subscribe_msg_num = Some(u16::MAX);
        client.live_msg_num = Some(1);
        let round: Vec<u16> = (0..2).map(|_| client.next_msg_num()).collect();
        assert_eq!(round, [2, 3]);
    }

    /// A settings read left unanswered under a number is forgotten when the
    /// number comes round again, so the new request's answer is not taken
    /// for it.
    #[tokio::test]
    async fn a_number_given_out_again_drops_what_was_pending_under_it() {
        let (mut client, _device) = client_and_device().await;
        client.pending_images.lock().unwrap().insert(1, ImageKind::Preset(4));
        client.next_msg_num = u16::MAX;
        assert_eq!(client.next_msg_num(), 65535);
        assert!(client.pending_images.lock().unwrap().contains_key(&1));
        assert_eq!(client.next_msg_num(), 1);
        assert!(client.pending_images.lock().unwrap().is_empty());
    }

    /// About 4.5 days of idle keep-alives: the 65535th is followed by 1, not
    /// 0 (the device's own number for its pushes), and the subscription's
    /// number (3, as after a login) is left out.
    #[tokio::test(start_paused = true)]
    async fn an_idle_session_s_keep_alive_goes_round_past_0_and_the_subscription() {
        let (mut client, mut device) = client_and_device().await;
        client.subscribe_msg_num = Some(3);
        client.next_msg_num = u16::MAX;
        let fake = async move {
            let mut asked = Vec::new();
            for _ in 0..4 {
                let request = device.recv_bc(&PLAIN).await.unwrap();
                assert_link_type_request(&request);
                device.send_bc(&link_type_reply(request.meta.msg_num), &PLAIN).await.unwrap();
                asked.push(request.meta.msg_num);
            }
            assert_eq!(asked, [65535, 1, 2, 4]);
        };
        tokio::select! {
            error = client.wait_for_pushes() => panic!("wait_for_pushes ended: {error}"),
            () = fake => {}
        }
    }

    /// `identity` keeps the number its subscription went out under (the
    /// second request, as no login came first here).
    #[tokio::test]
    async fn identity_keeps_the_number_of_the_subscription() {
        let (mut client, mut device) = client_and_device().await;
        let fake = tokio::spawn(async move {
            let mut asked = Vec::new();
            for _ in 0..4 {
                let request = device.recv_bc(&PLAIN).await.unwrap();
                asked.push((request.meta.msg_id, request.meta.msg_num));
            }
            let version = device_message(MSG_ID_VERSION, asked[3].1, 200, None, None);
            device.send_bc(&version, &PLAIN).await.unwrap();
            asked
        });
        client.identity().await;
        let asked = fake.await.unwrap();
        assert_eq!(asked[1].0, MSG_ID_SUBSCRIBE);
        assert_eq!(client.subscribe_msg_num, Some(asked[1].1));
        assert_eq!(client.subscribe_msg_num, Some(2));
    }

    /// Once the count has come round, `identity` asks under 65533 to 65535
    /// and then 1, not 0. An image under 1 left that number marked binary
    /// long ago; the answer to the version request under it is still read
    /// as the version, decrypted, and not passed on as image bytes.
    #[tokio::test]
    async fn identity_s_answer_is_read_after_the_count_has_come_round() {
        let enc = EncryptionProtocol::BcEncrypt;
        let (mut client, mut device) = client_and_device_with(enc.clone()).await;
        let binary = Extension { version: XML_VERSION.to_string(), binary_data: Some(1), ..Default::default() };
        let image_end = device_message(MSG_ID_IMAGE_FILE, 1, 201, Some(binary.to_bytes()), None);
        device.send_bc(&image_end, &enc).await.unwrap();
        client.connection.lock().await.recv_bc(&enc).await.unwrap();
        client.next_msg_num = 65533;
        let fake = tokio::spawn(async move {
            let mut asked = Vec::new();
            for _ in 0..4 {
                let request = device.recv_bc(&enc).await.unwrap();
                asked.push((request.meta.msg_id, request.meta.msg_num));
            }
            let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<VersionInfo version=\"1.1\">\n\
                       <name>Test hub</name>\n<type>TESTMODEL</type>\n</VersionInfo>\n</body>\n";
            let version = device_message(MSG_ID_VERSION, asked[3].1, 200, None, Some(xml.as_bytes().to_vec()));
            device.send_bc(&version, &enc).await.unwrap();
            asked
        });
        let identity = client.identity().await;
        let asked = fake.await.unwrap();
        assert_eq!(
            asked,
            [(MSG_ID_STREAM_INFO, 65533), (MSG_ID_SUBSCRIBE, 65534), (MSG_ID_SUPPORT, 65535), (MSG_ID_VERSION, 1)]
        );
        assert_eq!(client.subscribe_msg_num, Some(65534));
        assert_eq!((identity.name.as_deref(), identity.model.as_deref()), (Some("Test hub"), Some("TESTMODEL")));
    }

    /// `start_video` keeps the number it asked under, and a stop keeps it
    /// too: once the count has come round, the next start does not take it,
    /// so a frame the old stream still sends under it is not taken for the
    /// new start's answer (here a refusal).
    #[tokio::test]
    async fn a_live_stream_s_number_is_kept_until_the_next_stream_starts() {
        let (mut client, mut device) = client_and_device().await;
        let (asked_tx, mut asked) = tokio::sync::mpsc::unbounded_channel();
        let fake = tokio::spawn(async move {
            let mut live = None;
            while let Ok(request) = device.recv_bc(&PLAIN).await {
                let (msg_id, msg_num) = (request.meta.msg_id, request.meta.msg_num);
                let _ = asked_tx.send((msg_id, msg_num));
                if msg_id != MSG_ID_VIDEO {
                    continue;
                }
                let Some(old) = live else {
                    live = Some(msg_num);
                    device.send_bc(&device_message(MSG_ID_VIDEO, msg_num, 200, None, None), &PLAIN).await.unwrap();
                    continue;
                };
                let binary = Extension { version: XML_VERSION.to_string(), binary_data: Some(1), ..Default::default() };
                let frame = device_message(MSG_ID_VIDEO, old, 200, Some(binary.to_bytes()), Some(vec![0; 8]));
                device.send_bc(&frame, &PLAIN).await.unwrap();
                device.send_bc(&device_message(MSG_ID_VIDEO, msg_num, 400, None, None), &PLAIN).await.unwrap();
            }
        });
        let _frames = client.start_video(0, StreamProfile::Sub).await.unwrap();
        let (msg_id, first) = asked.recv().await.unwrap();
        assert_eq!((msg_id, client.live_msg_num), (MSG_ID_VIDEO, Some(first)));
        // As if the count had come round.
        client.next_msg_num = u16::MAX;
        client.stop_video(0).await.unwrap();
        assert_eq!(asked.recv().await.unwrap(), (MSG_ID_VIDEO_STOP, u16::MAX));
        assert_eq!(client.live_msg_num, Some(first));
        assert!(client.start_video(0, StreamProfile::Sub).await.is_err(), "the old stream's frame taken as the answer");
        let (msg_id, second) = asked.recv().await.unwrap();
        assert_eq!((msg_id, second), (MSG_ID_VIDEO, 2));
        assert_eq!(client.live_msg_num, Some(second));
        fake.abort();
    }

    /// The version request's reply is read whatever its code, and how it
    /// ended is told apart: a refusal with no body, a body under 300 that
    /// gives the name and model, a body that does not parse; a push before
    /// the reply is counted.
    #[tokio::test]
    async fn the_version_reply_is_read_whatever_its_code() {
        let (mut client, mut device) = client_and_device().await;
        let version = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<VersionInfo version=\"1.1\">\n\
                       <name>Test hub</name>\n<type>TESTMODEL</type>\n</VersionInfo>\n</body>\n";
        let fake = tokio::spawn(async move {
            let replies: [(u16, Option<&str>); 3] = [(400, None), (300, Some(version)), (200, Some("<body"))];
            for (index, (code, body)) in replies.into_iter().enumerate() {
                let request = device.recv_bc(&PLAIN).await.unwrap();
                assert_eq!(request.meta.msg_id, MSG_ID_VERSION);
                if index == 1 {
                    device.send_bc(&alarm_push(), &PLAIN).await.unwrap();
                }
                let reply = device_message(MSG_ID_VERSION, request.meta.msg_num, code, None, body.map(|b| b.into()));
                device.send_bc(&reply, &PLAIN).await.unwrap();
            }
        });
        let (info, answer, others) = client.ask_version().await;
        assert!(info.is_none());
        assert!(matches!(answer, VersionAnswer::Reply { code: 400, body: VersionBody::Empty }), "{answer:?}");
        assert_eq!(others, 0);
        let (info, answer, others) = client.ask_version().await;
        let info = info.expect("the body under 300 is read");
        assert_eq!((info.name.as_deref(), info.model.as_deref()), (Some("Test hub"), Some("TESTMODEL")));
        assert!(matches!(answer, VersionAnswer::Reply { code: 300, body: VersionBody::VersionInfo }), "{answer:?}");
        assert_eq!(others, 1);
        let (info, answer, _) = client.ask_version().await;
        assert!(info.is_none());
        assert!(matches!(answer, VersionAnswer::Reply { code: 200, body: VersionBody::Unparsed }), "{answer:?}");
        fake.await.unwrap();
    }

    /// A Preview answered with 400 and no body, the shape of every 400 in our
    /// captures, comes back as the stream the device refused, with its code,
    /// for the sub and the main stream alike.
    #[tokio::test]
    async fn a_refused_preview_names_the_stream_and_the_code() {
        let (mut client, mut device) = client_and_device().await;
        let fake = tokio::spawn(async move {
            let mut asked = Vec::new();
            for _ in 0..2 {
                let request = device.recv_bc(&PLAIN).await.unwrap();
                let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = request.body else {
                    panic!("a Preview without a body");
                };
                let preview = BcXml::from_bytes(&payload).unwrap().preview.expect("a Preview");
                asked.push((request.meta.msg_id, preview.stream_type.unwrap_or_default()));
                let refusal = device_message(MSG_ID_VIDEO, request.meta.msg_num, 400, None, None);
                device.send_bc(&refusal, &PLAIN).await.unwrap();
            }
            asked
        });
        for stream in [StreamProfile::Sub, StreamProfile::Main] {
            match client.start_video(0, stream).await.err() {
                Some(Error::StreamRefused { stream: refused, code: 400 }) => assert_eq!(refused, stream),
                other => panic!("expected the {stream:?} stream refused with 400, got {other:?}"),
            }
        }
        let asked = fake.await.unwrap();
        assert_eq!(asked, [(MSG_ID_VIDEO, "subStream".to_string()), (MSG_ID_VIDEO, "mainStream".to_string())]);
    }

    /// Live video's reader puts images together for as long as the stream
    /// runs. An image under 2 that never ended left its first bytes there;
    /// once the count has come round, the picture asked for under 2 again
    /// (1 is the stream's) is made of its own bytes only, and is the one
    /// asked for.
    #[tokio::test]
    async fn an_image_under_a_number_given_out_again_is_only_its_own() {
        let (mut client, mut device) = client_and_device().await;
        let mut updates = client.take_channel_updates().unwrap();
        let fake = tokio::spawn(async move {
            let binary = Extension { version: XML_VERSION.to_string(), binary_data: Some(1), ..Default::default() };
            let opening = |msg_num: u16| {
                let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n</body>\n";
                device_message(MSG_ID_IMAGE_FILE, msg_num, 200, None, Some(xml.as_bytes().to_vec()))
            };
            let chunk = |msg_num: u16, code: u16, bytes: Option<&[u8]>| {
                device_message(MSG_ID_IMAGE_FILE, msg_num, code, Some(binary.to_bytes()), bytes.map(<[u8]>::to_vec))
            };
            let mut asked = Vec::new();
            while let Ok(request) = device.recv_bc(&PLAIN).await {
                let (msg_id, msg_num) = (request.meta.msg_id, request.meta.msg_num);
                asked.push((msg_id, msg_num));
                let answer = match asked.len() {
                    1 => vec![device_message(MSG_ID_VIDEO, msg_num, 200, None, None)],
                    // The first image's opening reply and first bytes; its end never comes.
                    2 => vec![opening(msg_num), chunk(msg_num, 200, Some(&[0xff, 0xd8, 9, 9]))],
                    _ => vec![
                        opening(msg_num),
                        chunk(msg_num, 200, Some(&[0xff, 0xd8, 1, 2, 0xff, 0xd9])),
                        chunk(msg_num, 201, None),
                    ],
                };
                for bc in answer {
                    device.send_bc(&bc, &PLAIN).await.unwrap();
                }
                if asked.len() == 3 {
                    break;
                }
            }
            asked
        });
        let _frames = client.start_video(0, StreamProfile::Sub).await.unwrap();
        client.query_preset_image(0, 3).await;
        // As if 65535 had just been given out.
        client.next_msg_num = 0;
        client.query_preset_image(0, 4).await;
        match updates.recv().await {
            Some(DeviceUpdate::PresetImage { channel_id: 0, preset_id: 4, jpeg }) => {
                assert_eq!(jpeg, [0xff, 0xd8, 1, 2, 0xff, 0xd9]);
            }
            other => panic!("expected preset 4's image, got {other:?}"),
        }
        let asked = fake.await.unwrap();
        assert_eq!(asked, [(MSG_ID_VIDEO, 1), (MSG_ID_IMAGE_FILE, 2), (MSG_ID_IMAGE_FILE, 2)]);
    }

    /// The nonce reply as our RLN36 on firmware v3.5 sends it in every
    /// capture: login under the request's number, 0xdd12 (a BCEncrypt body),
    /// class 0x6614.
    fn nonce_reply(msg_num: u16, nonce: &str) -> Bc {
        let xml = BcXml {
            encryption: Some(Encryption {
                version: XML_VERSION.to_string(),
                type_: "md5".to_string(),
                nonce: nonce.to_string(),
            }),
            ..Default::default()
        };
        Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num,
                response_code: 0xdd12,
                class: 0x6614,
            },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: Some(xml.to_bytes()) }),
        }
    }

    /// A message shaped as the login reply: 200 and a stream list of one
    /// table, of `stream` at 640x360 and 10 fps.
    fn stream_list_message(msg_id: u32, msg_num: u16, stream: &str) -> Bc {
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<DeviceInfo version=\"1.1\">\n</DeviceInfo>\n\
             <StreamInfoList version=\"1.1\">\n<StreamInfo>\n<channelBits>1</channelBits>\n<encodeTable>\n\
             <type>{stream}</type>\n<resolution>\n<width>640</width>\n<height>360</height>\n</resolution>\n\
             <defaultFramerate>10</defaultFramerate>\n</encodeTable>\n</StreamInfo>\n</StreamInfoList>\n\
             </body>\n"
        );
        device_message(msg_id, msg_num, 200, None, Some(xml.into_bytes()))
    }

    /// The login reply: 200 under the request's number, with a stream list of
    /// one table.
    fn login_reply(msg_num: u16) -> Bc {
        stream_list_message(MSG_ID_LOGIN, msg_num, "subStream")
    }

    /// What `login_reply`'s stream list reads as.
    const LOGIN_REPLY_STREAMS: &str = "channels 0x1: subStream 640x360 10 fps";

    /// A channel list push (145 under 0) of one channel, with a placeholder
    /// camera UID.
    fn channel_list_push() -> Bc {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<ChannelInfoList version=\"1.1\">\n\
                   <ChannelInfo>\n<channelId>0</channelId>\n<devName>Test cam</devName>\n<state>connect</state>\n\
                   <uid>TESTCAMUID000001</uid>\n</ChannelInfo>\n</ChannelInfoList>\n</body>\n";
        device_message(MSG_ID_CHANNEL_INFO, 0, 200, None, Some(xml.as_bytes().to_vec()))
    }

    /// The header `login` always sends its two requests with: login, channel
    /// 0, under 1 (its first request).
    fn login_request_meta(response_code: u16, class: u16) -> BcMeta {
        BcMeta { msg_id: MSG_ID_LOGIN, channel_id: 0, stream_type: 0, msg_num: 1, response_code, class }
    }

    /// The device end of a login with the nonce step (no nonce from the P2P
    /// handshake): checks that the upgrade request is the one `login` always
    /// sends, sends `before_nonce` ahead of the nonce reply, then goes on as
    /// `fake_login_step`.
    async fn fake_login_with_nonce_step(
        mut device: BcConnection,
        before_nonce: Vec<Bc>,
        before_login: Vec<Bc>,
        reply: fn(u16) -> Bc,
    ) {
        let upgrade = device.recv_bc(&PLAIN).await.unwrap();
        assert_eq!(upgrade.meta, login_request_meta(0xdc12, 0x6514));
        assert!(matches!(upgrade.body, BcBody::Legacy(LegacyMsg::LoginUpgrade)), "{:?}", upgrade.body);
        // Before the login the device has no key: these go out as it would
        // send them, which the client cannot read.
        for message in &before_nonce {
            device.send_bc(message, &EncryptionProtocol::BcEncrypt).await.unwrap();
        }
        device.send_bc(&nonce_reply(upgrade.meta.msg_num, "TESTNONCE"), &EncryptionProtocol::BcEncrypt).await.unwrap();
        fake_login_step(device, before_login, reply).await;
    }

    /// The device end of the login step, with the nonce "TESTNONCE": checks
    /// that the request is the one `login` always sends, hashed with that
    /// nonce, and sends `before_reply` ahead of `reply` under the request's
    /// number.
    async fn fake_login_step(mut device: BcConnection, before_reply: Vec<Bc>, reply: fn(u16) -> Bc) {
        let aes = EncryptionProtocol::Aes { key: aes_key_from_password("swordfish", "TESTNONCE") };
        let login = device.recv_bc(&aes).await.unwrap();
        assert_eq!(login.meta, login_request_meta(0, 0x6414));
        let BcBody::Modern(ModernMsg { extension_xml: None, payload: Some(payload) }) = &login.body else {
            panic!("{:?}", login.body);
        };
        let user = BcXml::from_bytes(payload).unwrap().login_user.unwrap();
        assert_eq!(user.user_name, md5_hex_truncated("adminTESTNONCE"), "hashed with the nonce of the reply");
        assert_eq!(user.password, md5_hex_truncated("swordfishTESTNONCE"));
        for message in &before_reply {
            device.send_bc(message, &aes).await.unwrap();
        }
        device.send_bc(&reply(login.meta.msg_num), &aes).await.unwrap();
    }

    /// An alarm push (33 under 0) ahead of the nonce reply, as our RLN36 on
    /// firmware v3.5 sent one in a run, is read past: the login goes on
    /// with the nonce of the reply.
    #[tokio::test]
    async fn a_push_before_the_nonce_reply_is_read_past() {
        let (mut client, device) = client_and_device().await;
        let fake = tokio::spawn(fake_login_with_nonce_step(device, vec![alarm_push()], Vec::new(), login_reply));
        client.login("admin", "swordfish").await.unwrap();
        assert_eq!(describe_streams(&client.encode_profiles()), LOGIN_REPLY_STREAMS);
        fake.await.unwrap();
    }

    /// Ahead of the login reply, neither an alarm push nor a channel list is
    /// taken for it (the reply's stream list is the one kept), and the
    /// channel list is passed on, its camera UID kept for recorded video.
    #[tokio::test]
    async fn a_push_before_the_login_reply_is_read_past_and_passed_on() {
        let (mut client, device) = client_and_device().await;
        let mut updates = client.take_channel_updates().unwrap();
        let pushes = vec![alarm_push(), channel_list_push()];
        let fake = tokio::spawn(fake_login_with_nonce_step(device, Vec::new(), pushes, login_reply));
        client.login("admin", "swordfish").await.unwrap();
        assert_eq!(describe_streams(&client.encode_profiles()), LOGIN_REPLY_STREAMS);
        match updates.try_recv() {
            Ok(DeviceUpdate::Channels(channels)) => assert_eq!(channels.len(), 1),
            other => panic!("expected the channel list, got {other:?}"),
        }
        assert_eq!(client.channel_uid(0).as_ref().map(CameraUid::as_str), Some("TESTCAMUID000001"));
        fake.await.unwrap();
    }

    /// The same when the nonce came from the P2P handshake, as on our Hub and
    /// our RLN36 on firmware v3.6: only the login step reads.
    #[tokio::test]
    async fn a_push_before_the_login_reply_is_read_past_with_the_nonce_from_the_handshake() {
        let (mut client, device) = client_and_device_with_nonce(PLAIN, Some("TESTNONCE")).await;
        let mut updates = client.take_channel_updates().unwrap();
        let pushes = vec![alarm_push(), channel_list_push()];
        let fake = tokio::spawn(fake_login_step(device, pushes, login_reply));
        client.login("admin", "swordfish").await.unwrap();
        assert_eq!(describe_streams(&client.encode_profiles()), LOGIN_REPLY_STREAMS);
        match updates.try_recv() {
            Ok(DeviceUpdate::Channels(channels)) => assert_eq!(channels.len(), 1),
            other => panic!("expected the channel list, got {other:?}"),
        }
        assert_eq!(client.channel_uid(0).as_ref().map(CameraUid::as_str), Some("TESTCAMUID000001"));
        fake.await.unwrap();
    }

    /// A message of another id under the request's number, ahead of either
    /// reply, is not taken for it.
    #[tokio::test]
    async fn a_message_of_another_id_under_the_request_s_number_is_read_past() {
        let (mut client, device) = client_and_device().await;
        let mut other_id = nonce_reply(1, "OTHERNONCE");
        other_id.meta.msg_id = MSG_ID_VERSION;
        let before_nonce = vec![other_id];
        let before_login = vec![stream_list_message(MSG_ID_VERSION, 1, "mainStream")];
        let fake = tokio::spawn(fake_login_with_nonce_step(device, before_nonce, before_login, login_reply));
        client.login("admin", "swordfish").await.unwrap();
        assert_eq!(describe_streams(&client.encode_profiles()), LOGIN_REPLY_STREAMS);
        fake.await.unwrap();
    }

    /// A login message under another number, ahead of either reply, is not
    /// taken for it: here a nonce reply under 0 with another nonce, and a
    /// login reply under 2 with another stream list.
    #[tokio::test]
    async fn a_login_message_under_another_number_is_read_past() {
        let (mut client, device) = client_and_device().await;
        let before_nonce = vec![nonce_reply(0, "OTHERNONCE")];
        let before_login = vec![stream_list_message(MSG_ID_LOGIN, 2, "mainStream")];
        let fake = tokio::spawn(fake_login_with_nonce_step(device, before_nonce, before_login, login_reply));
        client.login("admin", "swordfish").await.unwrap();
        assert_eq!(describe_streams(&client.encode_profiles()), LOGIN_REPLY_STREAMS);
        fake.await.unwrap();
    }

    /// A push ahead of a refusal leaves the refusal's code to decide: 401
    /// (wrong password), which makes the app forget the saved password.
    #[tokio::test]
    async fn a_push_before_a_refusal_still_gives_the_refusal_s_code() {
        let (mut client, device) = client_and_device().await;
        let refusal = |msg_num| device_message(MSG_ID_LOGIN, msg_num, 401, None, None);
        let fake = tokio::spawn(fake_login_with_nonce_step(device, Vec::new(), vec![alarm_push()], refusal));
        let error = client.login("admin", "swordfish").await.unwrap_err();
        assert!(matches!(error, Error::LoginFailed { code: 401 }), "{error}");
        fake.await.unwrap();
    }

    /// 63 messages ahead of a reply are read past: with the reply, the 64 a
    /// login step reads at most.
    #[tokio::test]
    async fn sixty_three_pushes_ahead_of_the_reply_are_read_past() {
        let (mut client, device) = client_and_device().await;
        let pushes = (0..63).map(|_| alarm_push()).collect();
        let fake = tokio::spawn(fake_login_with_nonce_step(device, pushes, Vec::new(), login_reply));
        client.login("admin", "swordfish").await.unwrap();
        assert_eq!(describe_streams(&client.encode_profiles()), LOGIN_REPLY_STREAMS);
        fake.await.unwrap();
    }

    /// 64 messages ahead of the reply end the step before it.
    #[tokio::test]
    async fn a_login_step_reads_at_most_64_messages() {
        let (mut client, mut device) = client_and_device().await;
        let fake = tokio::spawn(async move {
            let upgrade = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(upgrade.meta, login_request_meta(0xdc12, 0x6514));
            for _ in 0..64 {
                device.send_bc(&alarm_push(), &EncryptionProtocol::BcEncrypt).await.unwrap();
            }
            let reply = nonce_reply(upgrade.meta.msg_num, "TESTNONCE");
            device.send_bc(&reply, &EncryptionProtocol::BcEncrypt).await.unwrap();
            device
        });
        let error = client.login("admin", "swordfish").await.unwrap_err();
        match &error {
            Error::ProtocolError(text) => assert_eq!(text, "no login reply among 64 messages"),
            other => panic!("expected a protocol error, got {other:?}"),
        }
        drop(fake.await.unwrap());
    }

    /// A device that only pushes, every 4 s so that no single read waits
    /// long, ends a login step 15 s after its request.
    #[tokio::test(start_paused = true)]
    async fn a_login_step_gives_up_15_s_after_its_request() {
        let (mut client, mut device) = client_and_device().await;
        let fake = tokio::spawn(async move {
            device.recv_bc(&PLAIN).await.unwrap();
            for _ in 0..8 {
                tokio::time::sleep(Duration::from_secs(4)).await;
                device.send_bc(&alarm_push(), &EncryptionProtocol::BcEncrypt).await.unwrap();
            }
            device
        });
        let start = Instant::now();
        let error = client.login("admin", "swordfish").await.unwrap_err();
        let waited = start.elapsed();
        assert!(matches!(error, Error::ReplyTimeout), "{error}");
        assert!(waited >= Duration::from_secs(15) && waited < Duration::from_secs(16), "{waited:?}");
        fake.abort();
    }

    /// One BcMedia video unit without an extra header: a key frame of `codec`
    /// (`H264` or `H265`), or with `key` false a delta frame.
    fn video_unit(key: bool, codec: &[u8; 4], microseconds: u32, data: &[u8]) -> Vec<u8> {
        let magic: u32 = if key { 0x6364_3030 } else { 0x6364_3130 };
        let mut unit = magic.to_le_bytes().to_vec();
        unit.extend_from_slice(codec);
        unit.extend_from_slice(&(data.len() as u32).to_le_bytes());
        unit.extend_from_slice(&0u32.to_le_bytes());
        unit.extend_from_slice(&microseconds.to_le_bytes());
        unit.extend_from_slice(&0u32.to_le_bytes());
        unit.extend_from_slice(data);
        unit.resize(unit.len() + (8 - data.len() % 8) % 8, 0);
        unit
    }

    /// One BcMedia AAC unit.
    fn audio_unit(adts: &[u8]) -> Vec<u8> {
        let mut unit = 0x6277_3530u32.to_le_bytes().to_vec();
        unit.extend_from_slice(&(adts.len() as u16).to_le_bytes());
        unit.extend_from_slice(&(adts.len() as u16).to_le_bytes());
        unit.extend_from_slice(adts);
        unit.resize(unit.len() + (8 - adts.len() % 8) % 8, 0);
        unit
    }

    /// A live stream's message that starts a unit, under its Preview's
    /// number, channel and stream type (`preview` is the request's header),
    /// with an Extension that marks it binary and names the channel. It has
    /// no self-check, so the media guard takes it.
    fn media(preview: &BcMeta, payload: Vec<u8>) -> Bc {
        let binary = Extension {
            version: XML_VERSION.to_string(),
            binary_data: Some(1),
            channel_id: Some(preview.channel_id),
            ..Default::default()
        };
        let mut bc = device_message(MSG_ID_VIDEO, preview.msg_num, 200, Some(binary.to_bytes()), Some(payload));
        (bc.meta.channel_id, bc.meta.stream_type) = (preview.channel_id, preview.stream_type);
        bc
    }

    /// The same for a later message of a unit, which carries no Extension.
    fn media_continued(preview: &BcMeta, payload: Vec<u8>) -> Bc {
        let mut bc = device_message(MSG_ID_VIDEO, preview.msg_num, 200, None, Some(payload));
        (bc.meta.channel_id, bc.meta.stream_type) = (preview.channel_id, preview.stream_type);
        bc
    }

    /// The device end of a switch from one live stream to another. The first
    /// Preview is answered and followed by `first` under its number; the stop
    /// is answered; other requests are only kept. The second Preview is
    /// answered and followed by what `tail` makes of the headers of every
    /// request so far: the messages that come after that reply. Gives back
    /// those headers.
    async fn switching_device(
        mut device: BcConnection,
        first: Vec<u8>,
        tail: impl FnOnce(&[BcMeta]) -> Vec<Bc>,
    ) -> Vec<BcMeta> {
        let mut asked: Vec<BcMeta> = Vec::new();
        while let Ok(request) = device.recv_bc(&PLAIN).await {
            let meta = request.meta;
            asked.push(meta.clone());
            if meta.msg_id != MSG_ID_VIDEO && meta.msg_id != MSG_ID_VIDEO_STOP {
                continue;
            }
            let mut reply = device_message(meta.msg_id, meta.msg_num, 200, None, None);
            (reply.meta.channel_id, reply.meta.stream_type) = (meta.channel_id, meta.stream_type);
            if meta.msg_id == MSG_ID_VIDEO && asked.iter().filter(|m| m.msg_id == MSG_ID_VIDEO).count() > 1 {
                for bc in std::iter::once(reply).chain(tail(&asked)) {
                    device.send_bc(&bc, &PLAIN).await.unwrap();
                }
                return asked;
            }
            device.send_bc(&reply, &PLAIN).await.unwrap();
            if meta.msg_id == MSG_ID_VIDEO {
                device.send_bc(&media(&meta, first.clone()), &PLAIN).await.unwrap();
            }
        }
        asked
    }

    /// The next frame of a live stream, within 5 s.
    async fn next_frame(frames: &mut ReceiverStream<crate::Result<VideoFrame>>) -> VideoFrame {
        let next = tokio::time::timeout(Duration::from_secs(5), frames.next()).await.expect("a frame within 5 s");
        next.expect("the stream goes on").expect("a frame, not an error")
    }

    /// Each request's id, number, channel and stream type.
    fn headers(asked: &[BcMeta]) -> Vec<(u32, u16, u8, u8)> {
        asked.iter().map(|m| (m.msg_id, m.msg_num, m.channel_id, m.stream_type)).collect()
    }

    /// Main to sub, as an NVR over the relay does it: after the sub stream's
    /// reply the main stream (H.265) still sends, under its own Preview's
    /// number, a whole delta frame and the start of a key frame. The sub
    /// stream (H.264) starts with its own key frame, which is what the decoder
    /// is picked by; nothing of the tail comes out, and the diagnostic trace
    /// still has it.
    #[tokio::test]
    async fn after_a_switch_the_first_frame_is_the_new_stream_s_not_the_old_stream_s_tail() {
        let (mut client, device) = client_and_device().await;
        let fake = tokio::spawn(switching_device(device, video_unit(true, b"H265", 1, &[0, 0, 0, 1, 0x40]), |asked| {
            let (main, sub) = (&asked[0], &asked[2]);
            let next_key = video_unit(true, b"H265", 3, &[0x26; 64]);
            vec![
                media(main, video_unit(false, b"H265", 2, &[0, 0, 0, 1, 0x02, 0x01])),
                media(main, next_key[..40].to_vec()),
                media(sub, video_unit(true, b"H264", 1000, &[0, 0, 0, 1, 0x67, 0x64])),
            ]
        }));
        let mut main = client.start_video(0, StreamProfile::Main).await.unwrap();
        assert_eq!(next_frame(&mut main).await.video_type, VideoType::H265);
        client.stop_video(0).await.unwrap();
        let trace = Arc::new(crate::media_trace::MediaTrace::default());
        let mut sub = client.start_video_with_trace(0, StreamProfile::Sub, Some(Arc::clone(&trace))).await.unwrap();
        let first = next_frame(&mut sub).await;
        assert_eq!(
            (first.video_type, first.is_keyframe, first.microseconds, first.data.as_slice()),
            (VideoType::H264, true, 1000, &[0, 0, 0, 1, 0x67, 0x64][..])
        );
        // The trace has the tail as well as the new stream's message.
        assert_eq!(trace.write_to(Vec::new()).unwrap(), (3, false));
        let asked = fake.await.unwrap();
        assert_eq!(headers(&asked), [(MSG_ID_VIDEO, 1, 0, 0), (MSG_ID_VIDEO_STOP, 2, 0, 0), (MSG_ID_VIDEO, 3, 0, 1)]);
    }

    /// From one camera's sub stream to another camera's, both H.264, as an
    /// NVR over the relay does it: what the first camera still sends after
    /// the reply, under its own Preview's number, is not shown as the second
    /// camera's picture.
    #[tokio::test]
    async fn after_a_switch_another_camera_s_tail_in_the_same_codec_is_not_shown() {
        let (mut client, device) = client_and_device().await;
        let fake = tokio::spawn(switching_device(device, video_unit(true, b"H264", 1, &[0, 0, 0, 1, 0x65, 1]), |asked| {
            let (one, other) = (&asked[0], &asked[2]);
            vec![
                media(one, video_unit(false, b"H264", 2, &[0, 0, 0, 1, 0x41, 1])),
                media(one, video_unit(true, b"H264", 3, &[0, 0, 0, 1, 0x65, 1])),
                media(other, video_unit(true, b"H264", 500, &[0, 0, 0, 1, 0x65, 2])),
            ]
        }));
        let mut one = client.start_video(1, StreamProfile::Sub).await.unwrap();
        assert_eq!(next_frame(&mut one).await.microseconds, 1);
        client.stop_video(1).await.unwrap();
        let mut other = client.start_video(0, StreamProfile::Sub).await.unwrap();
        let first = next_frame(&mut other).await;
        assert_eq!(
            (first.video_type, first.is_keyframe, first.microseconds, first.data.as_slice()),
            (VideoType::H264, true, 500, &[0, 0, 0, 1, 0x65, 2][..])
        );
        let asked = fake.await.unwrap();
        assert_eq!(headers(&asked), [(MSG_ID_VIDEO, 1, 1, 1), (MSG_ID_VIDEO_STOP, 2, 1, 1), (MSG_ID_VIDEO, 3, 0, 1)]);
    }

    /// The old stream's message between two messages of the new stream's
    /// first key frame: the key frame comes out whole, and the frame after it
    /// too.
    #[tokio::test]
    async fn an_old_stream_s_message_between_two_parts_of_a_frame_is_left_out() {
        let (mut client, device) = client_and_device().await;
        let data: Vec<u8> = (0..100).collect();
        let main_key = video_unit(true, b"H265", 1, &[0, 0, 0, 1, 0x40]);
        let key = video_unit(true, b"H264", 1000, &data);
        let fake = tokio::spawn(switching_device(device, main_key, move |asked| {
            let (main, sub) = (&asked[0], &asked[2]);
            vec![
                media(sub, key[..50].to_vec()),
                media(main, video_unit(false, b"H265", 2, &[0, 0, 0, 1, 0x02, 0x01])),
                media_continued(sub, key[50..].to_vec()),
                media(sub, video_unit(false, b"H264", 1040, &[0, 0, 0, 1, 0x41])),
            ]
        }));
        let mut main = client.start_video(0, StreamProfile::Main).await.unwrap();
        next_frame(&mut main).await;
        client.stop_video(0).await.unwrap();
        let mut sub = client.start_video(0, StreamProfile::Sub).await.unwrap();
        let first = next_frame(&mut sub).await;
        assert_eq!((first.video_type, first.is_keyframe, first.microseconds), (VideoType::H264, true, 1000));
        assert_eq!(first.data, data);
        let second = next_frame(&mut sub).await;
        assert_eq!(
            (second.is_keyframe, second.microseconds, second.data.as_slice()),
            (false, 1040, &[0, 0, 0, 1, 0x41][..])
        );
        fake.await.unwrap();
    }

    /// The next update, within 5 s.
    async fn next_update(updates: &mut tokio::sync::mpsc::UnboundedReceiver<DeviceUpdate>) -> DeviceUpdate {
        let next = tokio::time::timeout(Duration::from_secs(5), updates.recv()).await;
        next.expect("an update within 5 s").expect("the updates go on")
    }

    /// What else comes in the middle of the old stream's tail is still passed
    /// on: a pushed channel list, and the picture and the settings page asked
    /// for between the stop and the new Preview (an alarm push is dropped, as
    /// it always was). The new stream's sound is played, the old stream's is
    /// not.
    #[tokio::test]
    async fn pushes_pictures_settings_and_sound_still_come_through_an_old_stream_s_tail() {
        let (mut client, device) = client_and_device().await;
        let mut updates = client.take_channel_updates().unwrap();
        let mut audio = client.take_audio().unwrap();
        let fake = tokio::spawn(switching_device(device, video_unit(true, b"H265", 1, &[0, 0, 0, 1, 0x40]), |asked| {
            let (main, image, page, sub) = (&asked[0], asked[2].msg_num, asked[3].msg_num, &asked[4]);
            let channels = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<ChannelInfoList version=\"1.1\">\n\
                            <ChannelInfo>\n<channelId>2</channelId>\n<state>connect</state>\n\
                            <streamSupport>mainStream,subStream</streamSupport>\n</ChannelInfo>\n\
                            </ChannelInfoList>\n</body>\n";
            let opening = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n</body>\n";
            let reboot = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<AutoReboot version=\"1.1\">\n\
                          <enable>1</enable>\n<weekDay>Sunday</weekDay>\n<hour>11</hour>\n<minute>0</minute>\n\
                          <second>0</second>\n</AutoReboot>\n</body>\n";
            let binary = Extension { version: XML_VERSION.to_string(), binary_data: Some(1), ..Default::default() };
            let chunk = |code: u16, bytes: Option<Vec<u8>>| {
                device_message(MSG_ID_IMAGE_FILE, image, code, Some(binary.to_bytes()), bytes)
            };
            vec![
                media(main, video_unit(false, b"H265", 2, &[0, 0, 0, 1, 0x02, 0x01])),
                alarm_push(),
                device_message(MSG_ID_CHANNEL_INFO, 0, 200, None, Some(channels.as_bytes().to_vec())),
                media(main, audio_unit(&[0xff, 0xf1, 1])),
                device_message(MSG_ID_IMAGE_FILE, image, 200, None, Some(opening.as_bytes().to_vec())),
                chunk(200, Some(vec![0xff, 0xd8, 1, 2, 0xff, 0xd9])),
                media(main, video_unit(false, b"H265", 3, &[0, 0, 0, 1, 0x02, 0x02])),
                chunk(201, None),
                device_message(settings::MSG_ID_AUTO_REBOOT, page, 200, None, Some(reboot.as_bytes().to_vec())),
                media(main, audio_unit(&[0xff, 0xf1, 3])),
                media(sub, audio_unit(&[0xff, 0xf1, 2])),
                media(sub, video_unit(true, b"H264", 1000, &[0, 0, 0, 1, 0x67])),
            ]
        }));
        let mut main = client.start_video(0, StreamProfile::Main).await.unwrap();
        next_frame(&mut main).await;
        client.stop_video(0).await.unwrap();
        // Asked while no stream runs; answered in the middle of the old stream's tail.
        client.query_preset_image(0, 4).await;
        let maintenance = SettingsTarget::Device { page: DevicePage::Maintenance };
        client.query_settings(maintenance, &SettingsGates::default(), 7).await;
        let mut sub = client.start_video(0, StreamProfile::Sub).await.unwrap();
        match next_update(&mut updates).await {
            DeviceUpdate::Channels(channels) => {
                assert_eq!(channels.iter().map(|c| (c.channel_id, c.online)).collect::<Vec<_>>(), [(2, true)]);
            }
            other => panic!("expected the pushed channel list, got {other:?}"),
        }
        match next_update(&mut updates).await {
            DeviceUpdate::PresetImage { channel_id: 0, preset_id: 4, jpeg } => {
                assert_eq!(jpeg, [0xff, 0xd8, 1, 2, 0xff, 0xd9]);
            }
            other => panic!("expected preset 4's image, got {other:?}"),
        }
        match next_update(&mut updates).await {
            DeviceUpdate::Settings { page: SettingsPage::DeviceMaintenance(page), read: 7 } => {
                assert_eq!(page.auto_reboot.ok().and_then(|r| r.week_day).as_deref(), Some("Sunday"));
            }
            other => panic!("expected the Maintenance page, got {other:?}"),
        }
        let first = next_frame(&mut sub).await;
        assert_eq!((first.video_type, first.is_keyframe, first.microseconds), (VideoType::H264, true, 1000));
        // Passed on before the first frame: the new stream's sound alone.
        assert_eq!(audio.try_recv().ok(), Some(vec![0xff, 0xf1, 2]));
        assert!(audio.try_recv().is_err(), "more sound than the new stream's");
        let asked = fake.await.unwrap();
        let ids: Vec<_> = asked.iter().map(|m| (m.msg_id, m.msg_num)).collect();
        let (image, page) = ((MSG_ID_IMAGE_FILE, 3), (settings::MSG_ID_AUTO_REBOOT, 4));
        assert_eq!(ids, [(MSG_ID_VIDEO, 1), (MSG_ID_VIDEO_STOP, 2), image, page, (MSG_ID_VIDEO, 5)]);
    }

    /// Main to extern on a channel other than 0, once the request count has
    /// come round: the old stream under 65534, the stop under 65535 and the
    /// new stream under 1. Both streams have the same channel and the same
    /// header stream type, so only the number tells them apart.
    #[tokio::test]
    async fn after_a_switch_past_65535_the_new_stream_s_number_is_still_told_apart() {
        let (mut client, device) = client_and_device().await;
        client.next_msg_num = 65534;
        let fake = tokio::spawn(switching_device(device, video_unit(true, b"H265", 1, &[0, 0, 0, 1, 0x40]), |asked| {
            let (main, other) = (&asked[0], &asked[2]);
            vec![
                media(main, video_unit(false, b"H265", 2, &[0, 0, 0, 1, 0x02, 0x01])),
                media(other, video_unit(true, b"H264", 1000, &[0, 0, 0, 1, 0x67])),
            ]
        }));
        let mut main = client.start_video(2, StreamProfile::Main).await.unwrap();
        next_frame(&mut main).await;
        client.stop_video(2).await.unwrap();
        let mut other = client.start_video(2, StreamProfile::Extern).await.unwrap();
        let first = next_frame(&mut other).await;
        assert_eq!(
            (first.video_type, first.is_keyframe, first.microseconds, first.data.as_slice()),
            (VideoType::H264, true, 1000, &[0, 0, 0, 1, 0x67][..])
        );
        let asked = fake.await.unwrap();
        assert_eq!(
            headers(&asked),
            [(MSG_ID_VIDEO, 65534, 2, 0), (MSG_ID_VIDEO_STOP, 65535, 2, 0), (MSG_ID_VIDEO, 1, 2, 0)]
        );
    }
}
