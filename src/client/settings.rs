//! Read-only settings pages of a camera channel and of an NVR or Home Hub
//! itself: which queries each page sends, how the replies are gathered into
//! one `SettingsPage`, and a diagnostic probe (`REOLING_PROBE_SETTINGS`).
//!
//! Every message here is a query: an empty request, or one whose body only
//! names what to read (the smart-detection type). Nothing is written.
//!
//! A page's reads go out one after the other, each under its own message
//! number. Whichever task reads the connection when a reply comes (the video
//! task, a playback reader, the push listener) hands it to `pushed_update`,
//! which finds the page's collector through the shared pending map and
//! records the outcome. The page is reported once every read has an
//! outcome; a read the device never answers is given up after
//! `SETTINGS_REPLY_TIMEOUT`. A reply to a page's read is the page's alone:
//! it is never also routed by its message id (several of the ids here, 56,
//! 44, 58, 80, have a meaning of their own there), and that holds for a
//! reply that comes after the timeout or after the page was read again.

use super::{control_xml, monitor_point_read_extension, pushed_update, ChannelAbilities, DeviceIdentity, DeviceUpdate, ImageKind, ReolinkClient};
use crate::protocol::bc::model::*;
use crate::protocol::bc::settings_xml::*;
use crate::protocol::bc::xml::{xml_escape, Extension, XML_VERSION};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

// Message ids of the settings reads that `protocol::bc::model` does not name
// yet. Each one is answered with the root element its name says.
/// `VideoInput` + `InputAdvanceCfg`: a channel's picture settings.
pub const MSG_ID_VIDEO_INPUT: u32 = 26;
/// `DayNightThreshold`.
pub const MSG_ID_DAY_NIGHT_THRESHOLD: u32 = 296;
/// `AiCfg`: smart detection types and auto tracking.
pub const MSG_ID_AI_CFG: u32 = 299;
/// `MD`: motion detection.
pub const MSG_ID_MOTION_DETECT: u32 = 46;
/// `AiDetectCfg`: one smart-detection type, named in the request body.
pub const MSG_ID_AI_DETECT_CFG: u32 = 342;
/// `trackLimit`: the horizontal auto-tracking range.
pub const MSG_ID_TRACK_LIMIT: u32 = 434;
/// `trackSchedule`.
pub const MSG_ID_TRACK_SCHEDULE: u32 = 436;
/// `LedState`: status and infrared LEDs (per channel, or the device's own).
pub const MSG_ID_LED_STATE: u32 = 208;
/// `FloodlightTask`: the spotlight's automatic behaviour.
pub const MSG_ID_FLOODLIGHT_TASK: u32 = 289;
/// `audioCfg`: the camera's speaker volume.
pub const MSG_ID_AUDIO_CFG: u32 = 264;
/// `aiDenoise`: audio noise reduction.
pub const MSG_ID_AI_DENOISE: u32 = 439;
/// `Record`: the recording schedule.
pub const MSG_ID_RECORD_TASK: u32 = 81;
/// `RecordCfg`: recording parameters (overwrite, pre/post recording).
pub const MSG_ID_RECORD_CFG: u32 = 54;
/// `EmailTask`.
pub const MSG_ID_EMAIL_TASK: u32 = 217;
/// `PushTask`.
pub const MSG_ID_PUSH_TASK: u32 = 219;
/// `PushCfg`.
pub const MSG_ID_PUSH_CFG: u32 = 363;
/// `FtpTask`.
pub const MSG_ID_FTP_TASK: u32 = 70;
/// `AudioTask`: when the camera's siren sounds.
pub const MSG_ID_AUDIO_TASK: u32 = 232;
/// `BuzzerTask`: when the hub's own siren sounds, per channel.
pub const MSG_ID_BUZZER_TASK: u32 = 306;
/// `Dhcp` + `AutoDns` + `Ip` + `Dns`.
pub const MSG_ID_NETWORK: u32 = 76;
/// `HddInfoList`.
pub const MSG_ID_HDD_INFO: u32 = 102;
/// `RecEncCfg`: recording encryption on/off.
pub const MSG_ID_REC_ENC_CFG: u32 = 507;
/// The six service-port roots (`ServerPort`, `HttpPort`, ...).
pub const MSG_ID_SERVICE_PORTS: u32 = 37;
/// `Upnp`.
pub const MSG_ID_UPNP: u32 = 97;
/// `PTOP`: access by UID on or off.
pub const MSG_ID_PTOP: u32 = 210;
/// `Ddns`.
pub const MSG_ID_DDNS: u32 = 40;
/// `Ntp`.
pub const MSG_ID_NTP: u32 = 38;
/// `certificateInfo`: whether a custom HTTPS certificate is installed.
pub const MSG_ID_CERTIFICATE: u32 = 455;
/// `kitApCfg`: the hub's Wi-Fi for its cameras.
pub const MSG_ID_KIT_AP: u32 = 481;
/// `SystemGeneral` + `Norm`: clock, time zone, formats, login lockout.
pub const MSG_ID_SYSTEM_GENERAL: u32 = 104;
/// `Dst`.
pub const MSG_ID_DST: u32 = 106;
/// `DeiveAudioCfg`: the hub's own sounds.
pub const MSG_ID_DEVICE_AUDIO: u32 = 504;
/// `RecordEnable`: an NVR's recording switch with its per-channel mask.
pub const MSG_ID_RECORD_ENABLE: u32 = 308;
/// `EmailEnable`.
pub const MSG_ID_EMAIL_ENABLE: u32 = 310;
/// `PushEnable`.
pub const MSG_ID_PUSH_ENABLE: u32 = 314;
/// `BuzzerEnable`.
pub const MSG_ID_BUZZER_ENABLE: u32 = 316;
/// `accessUserList`: the users the device is shared with.
pub const MSG_ID_ACCESS_USERS: u32 = 511;
/// `AutoReboot`.
pub const MSG_ID_AUTO_REBOOT: u32 = 101;

/// How long a page waits for its last reply before it is reported with
/// `NoReply` gaps.
pub const SETTINGS_REPLY_TIMEOUT: Duration = Duration::from_secs(20);

/// What a device is, from its model string: which pages it has and which
/// reads only one kind answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DeviceKind {
    #[default]
    Camera,
    Hub,
    Nvr,
}

impl DeviceKind {
    /// The same words `looks_multi_channel` and the product pictures go by.
    pub fn from_model(model: &str) -> Self {
        let m = model.to_lowercase();
        if m.contains("hub") {
            DeviceKind::Hub
        } else if m.contains("nvr") || m.starts_with("rln") {
            DeviceKind::Nvr
        } else {
            DeviceKind::Camera
        }
    }
}

/// The read-only tabs of a camera channel's settings window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CameraPage {
    Info,
    Image,
    Display,
    Detection,
    LightSound,
    Schedules,
    Network,
}

impl CameraPage {
    pub const ALL: [CameraPage; 7] = [
        CameraPage::Info,
        CameraPage::Image,
        CameraPage::Display,
        CameraPage::Detection,
        CameraPage::LightSound,
        CameraPage::Schedules,
        CameraPage::Network,
    ];
}

/// The tabs of an NVR's or Home Hub's own settings window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DevicePage {
    Info,
    Storage,
    Network,
    Time,
    LightSound,
    Alerts,
    Users,
    Maintenance,
}

impl DevicePage {
    pub const ALL: [DevicePage; 8] = [
        DevicePage::Info,
        DevicePage::Storage,
        DevicePage::Network,
        DevicePage::Time,
        DevicePage::LightSound,
        DevicePage::Alerts,
        DevicePage::Users,
        DevicePage::Maintenance,
    ];
}

/// One page to read: a camera channel's tab, or the device's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingsTarget {
    Camera { channel: u8, page: CameraPage },
    Device { page: DevicePage },
}

impl SettingsTarget {
    /// The channel the page's requests name (0 for the device's own pages).
    pub fn channel(&self) -> u8 {
        match self {
            SettingsTarget::Camera { channel, .. } => *channel,
            SettingsTarget::Device { .. } => 0,
        }
    }
}

/// What decides which reads a page sends, built by the UI from what it
/// already knows about the device. With `everything` set (the probe) every
/// read of every page goes out, whatever the kind and abilities say.
#[derive(Clone, Default)]
pub struct SettingsGates {
    pub kind: DeviceKind,
    /// The channel's abilities from the device's support table; `None`
    /// (not received yet) sends every read the kind allows.
    pub abilities: Option<ChannelAbilities>,
    /// The account the session is logged in as, which the user-list reads
    /// name. Never logged (`Debug` hides it).
    pub username: String,
    pub everything: bool,
}

impl SettingsGates {
    pub fn new(kind: DeviceKind, abilities: Option<ChannelAbilities>, username: &str) -> Self {
        SettingsGates { kind, abilities, username: username.to_string(), everything: false }
    }

    /// No gating at all: the probe's gates.
    pub fn everything(kind: DeviceKind, username: &str) -> Self {
        SettingsGates { kind, abilities: None, username: username.to_string(), everything: true }
    }

    fn has(&self, ability: impl Fn(&ChannelAbilities) -> bool) -> bool {
        self.everything || self.abilities.as_ref().map_or(true, ability)
    }

    fn is(&self, kind: DeviceKind) -> bool {
        self.everything || self.kind == kind
    }
}

impl fmt::Debug for SettingsGates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsGates")
            .field("kind", &self.kind)
            .field("abilities", &self.abilities)
            .field("username", &"<hidden>")
            .field("everything", &self.everything)
            .finish()
    }
}

/// Why a read has no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFailure {
    /// The device answered with this code (anything but 200 or 300).
    Refused(u16),
    /// No answer within `SETTINGS_REPLY_TIMEOUT`, or the request could not
    /// be sent.
    NoReply,
    /// The reply had no readable body.
    Unreadable,
    /// The reply parsed but lacked this root.
    Missing,
    /// The gates said not to ask (the device has no such setting).
    NotSent,
}

/// The outcome of one read.
pub type Read<T> = Result<T, ReadFailure>;

#[derive(Debug, Clone, PartialEq)]
pub struct CameraInfoPage {
    pub version: Read<VersionInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CameraImagePage {
    pub video_input: Read<VideoInput>,
    pub advanced: Read<InputAdvanceCfg>,
    pub threshold: Read<DayNightThreshold>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CameraDisplayPage {
    pub datetime: Read<OsdDatetime>,
    pub name: Read<OsdName>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CameraDetectionPage {
    pub ai: Read<AiCfg>,
    pub motion: Read<MotionDetect>,
    pub people: Read<AiDetectCfg>,
    pub vehicle: Read<AiDetectCfg>,
    pub animal: Read<AiDetectCfg>,
    pub track_limit: Read<TrackLimit>,
    pub track_schedule: Read<TrackSchedule>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CameraLightSoundPage {
    pub led: Read<LedState>,
    pub spotlight: Read<FloodlightTask>,
    pub audio: Read<AudioCfg>,
    pub denoise: Read<AiDenoise>,
    pub record_audio: Read<CompressionAudio>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CameraSchedulesPage {
    pub record: Read<ScheduledTask>,
    pub record_cfg: Read<RecordCfg>,
    pub email: Read<ScheduledTask>,
    pub push: Read<ScheduledTask>,
    pub push_cfg: Read<PushCfg>,
    pub ftp: Read<ScheduledTask>,
    pub siren: Read<ScheduledTask>,
    pub hub_siren: Read<ScheduledTask>,
}

/// The address rows (one read, four roots), of a camera or of the device.
#[derive(Debug, Clone, PartialEq)]
pub struct NetworkRows {
    pub dhcp: Read<Enable>,
    pub auto_dns: Read<Enable>,
    pub ip: Read<IpConfig>,
    pub dns: Read<DnsConfig>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfoPage {
    pub version: Read<VersionInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceStoragePage {
    pub disks: Read<HddInfoList>,
    pub record_cfg: Read<RecordCfg>,
    pub encryption: Read<Enable>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceNetworkPage {
    pub network: NetworkRows,
    pub server: Read<ServicePort>,
    pub http: Read<ServicePort>,
    pub https: Read<ServicePort>,
    pub rtsp: Read<ServicePort>,
    pub rtmp: Read<ServicePort>,
    pub onvif: Read<ServicePort>,
    pub upnp: Read<Enable>,
    pub p2p: Read<Enable>,
    pub ddns: Read<Ddns>,
    pub ntp: Read<Ntp>,
    pub certificate: Read<Enable>,
    pub hub_ap: Read<KitApCfg>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceTimePage {
    pub general: Read<SystemGeneral>,
    pub norm: Read<Norm>,
    pub dst: Read<Dst>,
    pub ntp: Read<Ntp>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceLightSoundPage {
    pub led: Read<LedState>,
    pub audio: Read<DeviceAudioCfg>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceAlertsPage {
    pub record: Read<ChannelSwitch>,
    pub email: Read<ChannelSwitch>,
    pub push: Read<ChannelSwitch>,
    pub buzzer: Read<ChannelSwitch>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceUsersPage {
    pub users: Read<UserList>,
    pub shared: Read<AccessUserList>,
    pub general: Read<SystemGeneral>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceMaintenancePage {
    pub auto_reboot: Read<AutoReboot>,
}

/// One page's worth of settings, as read from the device.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsPage {
    CameraInfo { channel: u8, page: CameraInfoPage },
    CameraImage { channel: u8, page: CameraImagePage },
    CameraDisplay { channel: u8, page: CameraDisplayPage },
    CameraDetection { channel: u8, page: CameraDetectionPage },
    CameraLightSound { channel: u8, page: CameraLightSoundPage },
    CameraSchedules { channel: u8, page: CameraSchedulesPage },
    CameraNetwork { channel: u8, page: NetworkRows },
    DeviceInfo(DeviceInfoPage),
    DeviceStorage(DeviceStoragePage),
    DeviceNetwork(DeviceNetworkPage),
    DeviceTime(DeviceTimePage),
    DeviceLightSound(DeviceLightSoundPage),
    DeviceAlerts(DeviceAlertsPage),
    DeviceUsers(DeviceUsersPage),
    DeviceMaintenance(DeviceMaintenancePage),
}

impl SettingsPage {
    pub fn target(&self) -> SettingsTarget {
        match self {
            SettingsPage::CameraInfo { channel, .. } => SettingsTarget::Camera { channel: *channel, page: CameraPage::Info },
            SettingsPage::CameraImage { channel, .. } => SettingsTarget::Camera { channel: *channel, page: CameraPage::Image },
            SettingsPage::CameraDisplay { channel, .. } => SettingsTarget::Camera { channel: *channel, page: CameraPage::Display },
            SettingsPage::CameraDetection { channel, .. } => {
                SettingsTarget::Camera { channel: *channel, page: CameraPage::Detection }
            }
            SettingsPage::CameraLightSound { channel, .. } => {
                SettingsTarget::Camera { channel: *channel, page: CameraPage::LightSound }
            }
            SettingsPage::CameraSchedules { channel, .. } => {
                SettingsTarget::Camera { channel: *channel, page: CameraPage::Schedules }
            }
            SettingsPage::CameraNetwork { channel, .. } => SettingsTarget::Camera { channel: *channel, page: CameraPage::Network },
            SettingsPage::DeviceInfo(_) => SettingsTarget::Device { page: DevicePage::Info },
            SettingsPage::DeviceStorage(_) => SettingsTarget::Device { page: DevicePage::Storage },
            SettingsPage::DeviceNetwork(_) => SettingsTarget::Device { page: DevicePage::Network },
            SettingsPage::DeviceTime(_) => SettingsTarget::Device { page: DevicePage::Time },
            SettingsPage::DeviceLightSound(_) => SettingsTarget::Device { page: DevicePage::LightSound },
            SettingsPage::DeviceAlerts(_) => SettingsTarget::Device { page: DevicePage::Alerts },
            SettingsPage::DeviceUsers(_) => SettingsTarget::Device { page: DevicePage::Users },
            SettingsPage::DeviceMaintenance(_) => SettingsTarget::Device { page: DevicePage::Maintenance },
        }
    }
}

// ---------------------------------------------------------------------------
// The reads of a page
// ---------------------------------------------------------------------------

/// The three smart-detection types a camera is asked about, one request
/// each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiKind {
    People,
    Vehicle,
    Animal,
}

impl AiKind {
    pub const ALL: [AiKind; 3] = [AiKind::People, AiKind::Vehicle, AiKind::Animal];

    /// The type's name on the wire.
    pub fn wire(self) -> &'static str {
        match self {
            AiKind::People => "people",
            AiKind::Vehicle => "vehicle",
            AiKind::Animal => "dog_cat",
        }
    }

    pub fn from_wire(text: &str) -> Option<Self> {
        AiKind::ALL.into_iter().find(|k| k.wire() == text.trim())
    }
}

/// Which read of a page a reply fills: a message id (once per page), or one
/// of the three smart-detection requests, which share an id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Msg(u32),
    AiDetect(AiKind),
}

/// The Extension a read carries: none, the channel, the channel with its
/// type (`chnType 0`), or the login user name.
#[derive(Clone, PartialEq, Eq)]
pub enum ReadExtension {
    None,
    Channel(u8),
    ChannelTyped(u8),
    UserName(String),
}

impl fmt::Debug for ReadExtension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadExtension::None => write!(f, "None"),
            ReadExtension::Channel(c) => write!(f, "Channel({c})"),
            ReadExtension::ChannelTyped(c) => write!(f, "ChannelTyped({c})"),
            ReadExtension::UserName(_) => write!(f, "UserName(<hidden>)"),
        }
    }
}

impl ReadExtension {
    fn to_bytes(&self) -> Option<Vec<u8>> {
        match self {
            ReadExtension::None => None,
            ReadExtension::Channel(channel) => {
                Some(Extension { version: XML_VERSION.to_string(), channel_id: Some(*channel), ..Default::default() }.to_bytes())
            }
            ReadExtension::ChannelTyped(channel) => Some(monitor_point_read_extension(*channel)),
            ReadExtension::UserName(name) => Some(user_name_extension(name)),
        }
    }
}

/// One query of a page. `send` is false for a read the gates rule out; it
/// is reported as `NotSent` without going on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsRead {
    pub msg_id: u32,
    pub extension: ReadExtension,
    pub body: Option<Vec<u8>>,
    pub slot: Slot,
    pub send: bool,
}

/// The Extension of the user-list reads: the login user name alone.
fn user_name_extension(username: &str) -> Vec<u8> {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<Extension version=\"1.1\">\n<userName>{}</userName>\n</Extension>\n",
        xml_escape(username)
    )
    .into_bytes()
}

/// The body of a smart-detection read: the channel and the type to read.
fn ai_detect_query(channel: u8, kind: AiKind) -> Vec<u8> {
    control_xml(&format!(
        "<AiDetectCfg version=\"1.1\">\n<chn>{channel}</chn>\n<type>{}</type>\n</AiDetectCfg>\n",
        kind.wire()
    ))
}

struct Reads(Vec<SettingsRead>);

impl Reads {
    fn plain(&mut self, msg_id: u32, extension: ReadExtension) {
        self.gated(msg_id, extension, true);
    }

    fn gated(&mut self, msg_id: u32, extension: ReadExtension, send: bool) {
        self.0.push(SettingsRead { msg_id, extension, body: None, slot: Slot::Msg(msg_id), send });
    }
}

/// The reads of `target`, in the order they are sent.
pub fn page_reads(target: SettingsTarget, gates: &SettingsGates) -> Vec<SettingsRead> {
    use ReadExtension::{Channel, ChannelTyped, None as NoExt, UserName};
    let mut r = Reads(Vec::new());
    match target {
        SettingsTarget::Camera { channel: ch, page } => match page {
            CameraPage::Info => r.plain(MSG_ID_CHANNEL_VERSION, ChannelTyped(ch)),
            CameraPage::Image => {
                r.plain(MSG_ID_VIDEO_INPUT, Channel(ch));
                r.plain(MSG_ID_DAY_NIGHT_THRESHOLD, ChannelTyped(ch));
            }
            CameraPage::Display => r.plain(MSG_ID_OSD, Channel(ch)),
            CameraPage::Detection => {
                r.plain(MSG_ID_AI_CFG, ChannelTyped(ch));
                r.plain(MSG_ID_MOTION_DETECT, ChannelTyped(ch));
                for kind in AiKind::ALL {
                    r.0.push(SettingsRead {
                        msg_id: MSG_ID_AI_DETECT_CFG,
                        extension: Channel(ch),
                        body: Some(ai_detect_query(ch, kind)),
                        slot: Slot::AiDetect(kind),
                        send: true,
                    });
                }
                let tracking = gates.has(|a| a.tracking);
                r.gated(MSG_ID_TRACK_LIMIT, ChannelTyped(ch), tracking);
                r.gated(MSG_ID_TRACK_SCHEDULE, ChannelTyped(ch), tracking);
            }
            CameraPage::LightSound => {
                r.plain(MSG_ID_LED_STATE, ChannelTyped(ch));
                r.gated(MSG_ID_FLOODLIGHT_TASK, ChannelTyped(ch), gates.has(|a| a.spotlight));
                r.gated(MSG_ID_AUDIO_CFG, ChannelTyped(ch), gates.has(|a| a.siren));
                // No ability names audio noise reduction: every camera is
                // asked, and one without it refuses the read.
                r.plain(MSG_ID_AI_DENOISE, ChannelTyped(ch));
                r.plain(MSG_ID_GET_COMPRESSION, ChannelTyped(ch));
            }
            CameraPage::Schedules => {
                r.plain(MSG_ID_RECORD_TASK, ChannelTyped(ch));
                r.plain(MSG_ID_RECORD_CFG, ChannelTyped(ch));
                r.plain(MSG_ID_EMAIL_TASK, ChannelTyped(ch));
                r.plain(MSG_ID_PUSH_TASK, ChannelTyped(ch));
                r.plain(MSG_ID_PUSH_CFG, ChannelTyped(ch));
                // An NVR has no FTP per channel, and its buzzer is a
                // device-level switch.
                r.gated(MSG_ID_FTP_TASK, ChannelTyped(ch), gates.is(DeviceKind::Hub));
                r.gated(MSG_ID_AUDIO_TASK, ChannelTyped(ch), gates.has(|a| a.siren));
                r.gated(MSG_ID_BUZZER_TASK, ChannelTyped(ch), gates.is(DeviceKind::Hub));
            }
            // An NVR answers a channel's network read with its own address
            // block, so the camera tab has no Network page there.
            CameraPage::Network => r.gated(MSG_ID_NETWORK, ChannelTyped(ch), gates.everything || gates.kind != DeviceKind::Nvr),
        },
        SettingsTarget::Device { page } => match page {
            DevicePage::Info => r.plain(MSG_ID_VERSION, NoExt),
            DevicePage::Storage => {
                r.plain(MSG_ID_HDD_INFO, NoExt);
                r.plain(MSG_ID_RECORD_CFG, ChannelTyped(0));
                r.plain(MSG_ID_REC_ENC_CFG, NoExt);
            }
            DevicePage::Network => {
                r.plain(MSG_ID_NETWORK, NoExt);
                r.plain(MSG_ID_SERVICE_PORTS, NoExt);
                r.plain(MSG_ID_UPNP, NoExt);
                r.plain(MSG_ID_PTOP, NoExt);
                r.plain(MSG_ID_DDNS, NoExt);
                r.plain(MSG_ID_NTP, NoExt);
                r.plain(MSG_ID_CERTIFICATE, NoExt);
                r.gated(MSG_ID_KIT_AP, NoExt, gates.is(DeviceKind::Hub));
            }
            DevicePage::Time => {
                r.plain(MSG_ID_SYSTEM_GENERAL, NoExt);
                r.plain(MSG_ID_DST, NoExt);
                r.plain(MSG_ID_NTP, NoExt);
            }
            DevicePage::LightSound => {
                r.gated(MSG_ID_LED_STATE, NoExt, gates.is(DeviceKind::Hub));
                r.gated(MSG_ID_DEVICE_AUDIO, NoExt, gates.is(DeviceKind::Hub));
            }
            DevicePage::Alerts => {
                let nvr = gates.is(DeviceKind::Nvr);
                r.gated(MSG_ID_RECORD_ENABLE, NoExt, nvr);
                r.gated(MSG_ID_EMAIL_ENABLE, NoExt, nvr);
                r.gated(MSG_ID_PUSH_ENABLE, NoExt, nvr);
                r.gated(MSG_ID_BUZZER_ENABLE, NoExt, nvr);
            }
            DevicePage::Users => {
                r.plain(MSG_ID_ABILITY_SUPPORT, UserName(gates.username.clone()));
                r.plain(MSG_ID_ACCESS_USERS, UserName(gates.username.clone()));
                r.plain(MSG_ID_SYSTEM_GENERAL, NoExt);
            }
            DevicePage::Maintenance => r.plain(MSG_ID_AUTO_REBOOT, NoExt),
        },
    }
    r.0
}

// ---------------------------------------------------------------------------
// Collecting a page's replies
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct SlotState {
    slot: Slot,
    msg_id: u32,
    /// The message number the read went out under (`None` until sent, or
    /// never when the gates ruled it out).
    msg_num: Option<u16>,
    outcome: Option<Read<SettingsBody>>,
}

/// The replies of one page as they come in, shared between the task that
/// sent the reads, whichever task reads the connection, and the timeout.
#[derive(Debug)]
pub struct PageCollector {
    target: SettingsTarget,
    slots: Vec<SlotState>,
    /// The page has been reported (or superseded); nothing more is emitted.
    done: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl PageCollector {
    pub fn new(target: SettingsTarget, reads: &[SettingsRead]) -> Self {
        let slots = reads
            .iter()
            .map(|r| SlotState {
                slot: r.slot,
                msg_id: r.msg_id,
                msg_num: None,
                outcome: if r.send { None } else { Some(Err(ReadFailure::NotSent)) },
            })
            .collect();
        PageCollector { target, slots, done: false }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Stops this page from ever being reported (a newer read of the same
    /// page replaced it).
    pub fn mark_done(&mut self) {
        self.done = true;
    }

    /// The read at `index` went out under `msg_num`.
    pub fn sent(&mut self, index: usize, msg_num: u16) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.msg_num = Some(msg_num);
        }
    }

    /// The read at `index` has this outcome without a reply (it could not
    /// be sent).
    pub fn failed(&mut self, index: usize, failure: ReadFailure) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.outcome = Some(Err(failure));
        }
    }

    /// Whether a reply with this number and id belongs to one of this
    /// page's reads.
    pub fn expects(&self, msg_num: u16, msg_id: u32) -> bool {
        self.slots.iter().any(|s| s.msg_num == Some(msg_num) && s.msg_id == msg_id)
    }

    /// Records the reply to the read sent under `msg_num`. A smart-detection
    /// reply goes to the slot of the type it names, if that one is still
    /// open, else to the slot of its number. `false` when the number is not
    /// this page's or the slot already has an outcome.
    pub fn record(&mut self, msg_num: u16, outcome: Read<SettingsBody>) -> bool {
        let Some(mut index) = self.slots.iter().position(|s| s.msg_num == Some(msg_num)) else {
            return false;
        };
        if let Ok(body) = &outcome {
            let named = body.ai_detect.as_ref().and_then(|d| d.kind.as_deref()).and_then(AiKind::from_wire);
            if let Some(kind) = named {
                if let Some(i) = self.slots.iter().position(|s| s.slot == Slot::AiDetect(kind) && s.outcome.is_none()) {
                    index = i;
                }
            }
        }
        if self.slots[index].outcome.is_some() {
            return false;
        }
        self.slots[index].outcome = Some(outcome);
        true
    }

    /// Every read has an outcome.
    pub fn complete(&self) -> bool {
        self.slots.iter().all(|s| s.outcome.is_some())
    }

    /// Gives every read still open this outcome (the timeout's `NoReply`).
    pub fn fill_open(&mut self, failure: ReadFailure) {
        for slot in self.slots.iter_mut().filter(|s| s.outcome.is_none()) {
            slot.outcome = Some(Err(failure));
        }
    }

    /// The page, once: when every read has an outcome and it was not
    /// reported before.
    pub fn finish_if_complete(&mut self) -> Option<SettingsPage> {
        if self.done || !self.complete() {
            return None;
        }
        self.done = true;
        Some(self.assemble())
    }

    fn read(&self, slot: Slot) -> Read<&SettingsBody> {
        match self.slots.iter().find(|s| s.slot == slot) {
            Some(SlotState { outcome: Some(Ok(body)), .. }) => Ok(body),
            Some(SlotState { outcome: Some(Err(failure)), .. }) => Err(*failure),
            Some(SlotState { outcome: None, .. }) => Err(ReadFailure::NoReply),
            None => Err(ReadFailure::NotSent),
        }
    }

    fn pick<T: Clone>(&self, slot: Slot, root: impl Fn(&SettingsBody) -> &Option<T>) -> Read<T> {
        self.read(slot).and_then(|body| root(body).clone().ok_or(ReadFailure::Missing))
    }

    fn network_rows(&self) -> NetworkRows {
        let slot = Slot::Msg(MSG_ID_NETWORK);
        NetworkRows {
            dhcp: self.pick(slot, |b| &b.dhcp),
            auto_dns: self.pick(slot, |b| &b.auto_dns),
            ip: self.pick(slot, |b| &b.ip),
            dns: self.pick(slot, |b| &b.dns),
        }
    }

    /// The page from the outcomes so far (a read without one counts as
    /// `NoReply`).
    pub fn assemble(&self) -> SettingsPage {
        let m = Slot::Msg;
        match self.target {
            SettingsTarget::Camera { channel, page } => match page {
                CameraPage::Info => SettingsPage::CameraInfo {
                    channel,
                    page: CameraInfoPage { version: self.pick(m(MSG_ID_CHANNEL_VERSION), |b| &b.version_info) },
                },
                CameraPage::Image => SettingsPage::CameraImage {
                    channel,
                    page: CameraImagePage {
                        video_input: self.pick(m(MSG_ID_VIDEO_INPUT), |b| &b.video_input),
                        advanced: self.pick(m(MSG_ID_VIDEO_INPUT), |b| &b.input_advance),
                        threshold: self.pick(m(MSG_ID_DAY_NIGHT_THRESHOLD), |b| &b.day_night_threshold),
                    },
                },
                CameraPage::Display => SettingsPage::CameraDisplay {
                    channel,
                    page: CameraDisplayPage {
                        datetime: self.pick(m(MSG_ID_OSD), |b| &b.osd_datetime),
                        name: self.pick(m(MSG_ID_OSD), |b| &b.osd_channel_name),
                    },
                },
                CameraPage::Detection => SettingsPage::CameraDetection {
                    channel,
                    page: CameraDetectionPage {
                        ai: self.pick(m(MSG_ID_AI_CFG), |b| &b.ai_cfg),
                        motion: self.pick(m(MSG_ID_MOTION_DETECT), |b| &b.motion),
                        people: self.pick(Slot::AiDetect(AiKind::People), |b| &b.ai_detect),
                        vehicle: self.pick(Slot::AiDetect(AiKind::Vehicle), |b| &b.ai_detect),
                        animal: self.pick(Slot::AiDetect(AiKind::Animal), |b| &b.ai_detect),
                        track_limit: self.pick(m(MSG_ID_TRACK_LIMIT), |b| &b.track_limit),
                        track_schedule: self.pick(m(MSG_ID_TRACK_SCHEDULE), |b| &b.track_schedule),
                    },
                },
                CameraPage::LightSound => SettingsPage::CameraLightSound {
                    channel,
                    page: CameraLightSoundPage {
                        led: self.pick(m(MSG_ID_LED_STATE), |b| &b.led_state),
                        spotlight: self.pick(m(MSG_ID_FLOODLIGHT_TASK), |b| &b.floodlight_task),
                        audio: self.pick(m(MSG_ID_AUDIO_CFG), |b| &b.audio_cfg),
                        denoise: self.pick(m(MSG_ID_AI_DENOISE), |b| &b.ai_denoise),
                        record_audio: self.pick(m(MSG_ID_GET_COMPRESSION), |b| &b.compression),
                    },
                },
                CameraPage::Schedules => SettingsPage::CameraSchedules {
                    channel,
                    page: CameraSchedulesPage {
                        record: self.pick(m(MSG_ID_RECORD_TASK), |b| &b.record),
                        record_cfg: self.pick(m(MSG_ID_RECORD_CFG), |b| &b.record_cfg),
                        email: self.pick(m(MSG_ID_EMAIL_TASK), |b| &b.email_task),
                        push: self.pick(m(MSG_ID_PUSH_TASK), |b| &b.push_task),
                        push_cfg: self.pick(m(MSG_ID_PUSH_CFG), |b| &b.push_cfg),
                        ftp: self.pick(m(MSG_ID_FTP_TASK), |b| &b.ftp_task),
                        siren: self.pick(m(MSG_ID_AUDIO_TASK), |b| &b.audio_task),
                        hub_siren: self.pick(m(MSG_ID_BUZZER_TASK), |b| &b.buzzer_task),
                    },
                },
                CameraPage::Network => SettingsPage::CameraNetwork { channel, page: self.network_rows() },
            },
            SettingsTarget::Device { page } => match page {
                DevicePage::Info => {
                    SettingsPage::DeviceInfo(DeviceInfoPage { version: self.pick(m(MSG_ID_VERSION), |b| &b.version_info) })
                }
                DevicePage::Storage => SettingsPage::DeviceStorage(DeviceStoragePage {
                    disks: self.pick(m(MSG_ID_HDD_INFO), |b| &b.hdd_info_list),
                    record_cfg: self.pick(m(MSG_ID_RECORD_CFG), |b| &b.record_cfg),
                    encryption: self.pick(m(MSG_ID_REC_ENC_CFG), |b| &b.rec_enc_cfg),
                }),
                DevicePage::Network => SettingsPage::DeviceNetwork(DeviceNetworkPage {
                    network: self.network_rows(),
                    server: self.pick(m(MSG_ID_SERVICE_PORTS), |b| &b.server_port),
                    http: self.pick(m(MSG_ID_SERVICE_PORTS), |b| &b.http_port),
                    https: self.pick(m(MSG_ID_SERVICE_PORTS), |b| &b.https_port),
                    rtsp: self.pick(m(MSG_ID_SERVICE_PORTS), |b| &b.rtsp_port),
                    rtmp: self.pick(m(MSG_ID_SERVICE_PORTS), |b| &b.rtmp_port),
                    onvif: self.pick(m(MSG_ID_SERVICE_PORTS), |b| &b.onvif_port),
                    upnp: self.pick(m(MSG_ID_UPNP), |b| &b.upnp),
                    p2p: self.pick(m(MSG_ID_PTOP), |b| &b.ptop),
                    ddns: self.pick(m(MSG_ID_DDNS), |b| &b.ddns),
                    ntp: self.pick(m(MSG_ID_NTP), |b| &b.ntp),
                    certificate: self.pick(m(MSG_ID_CERTIFICATE), |b| &b.certificate_info),
                    hub_ap: self.pick(m(MSG_ID_KIT_AP), |b| &b.kit_ap_cfg),
                }),
                DevicePage::Time => SettingsPage::DeviceTime(DeviceTimePage {
                    general: self.pick(m(MSG_ID_SYSTEM_GENERAL), |b| &b.system_general),
                    norm: self.pick(m(MSG_ID_SYSTEM_GENERAL), |b| &b.norm),
                    dst: self.pick(m(MSG_ID_DST), |b| &b.dst),
                    ntp: self.pick(m(MSG_ID_NTP), |b| &b.ntp),
                }),
                DevicePage::LightSound => SettingsPage::DeviceLightSound(DeviceLightSoundPage {
                    led: self.pick(m(MSG_ID_LED_STATE), |b| &b.led_state),
                    audio: self.pick(m(MSG_ID_DEVICE_AUDIO), |b| &b.device_audio_cfg),
                }),
                DevicePage::Alerts => SettingsPage::DeviceAlerts(DeviceAlertsPage {
                    record: self.pick(m(MSG_ID_RECORD_ENABLE), |b| &b.record_enable),
                    email: self.pick(m(MSG_ID_EMAIL_ENABLE), |b| &b.email_enable),
                    push: self.pick(m(MSG_ID_PUSH_ENABLE), |b| &b.push_enable),
                    buzzer: self.pick(m(MSG_ID_BUZZER_ENABLE), |b| &b.buzzer_enable),
                }),
                DevicePage::Users => SettingsPage::DeviceUsers(DeviceUsersPage {
                    users: self.pick(m(MSG_ID_ABILITY_SUPPORT), |b| &b.user_list),
                    shared: self.pick(m(MSG_ID_ACCESS_USERS), |b| &b.access_user_list),
                    general: self.pick(m(MSG_ID_SYSTEM_GENERAL), |b| &b.system_general),
                }),
                DevicePage::Maintenance => SettingsPage::DeviceMaintenance(DeviceMaintenancePage {
                    auto_reboot: self.pick(m(MSG_ID_AUTO_REBOOT), |b| &b.auto_reboot),
                }),
            },
        }
    }

    /// One line per read, for diagnostics: `msg N: ok, roots [...]`,
    /// `refused C`, `no reply`, `unreadable` or `not sent`.
    pub fn summary(&self) -> Vec<String> {
        self.slots
            .iter()
            .map(|s| {
                let what = match s.slot {
                    Slot::AiDetect(kind) => format!("msg {} ({})", s.msg_id, kind.wire()),
                    Slot::Msg(id) => format!("msg {id}"),
                };
                let outcome = match &s.outcome {
                    Some(Ok(body)) => format!("ok, roots {:?}", body.roots()),
                    Some(Err(ReadFailure::Refused(code))) => format!("refused {code}"),
                    Some(Err(ReadFailure::NoReply)) | None => "no reply".to_string(),
                    Some(Err(ReadFailure::Unreadable)) => "unreadable".to_string(),
                    Some(Err(ReadFailure::Missing)) => "missing".to_string(),
                    Some(Err(ReadFailure::NotSent)) => "not sent".to_string(),
                };
                format!("{what}: {outcome}")
            })
            .collect()
    }
}

/// The outcome a reply message carries: a parsed body for 200 or 300, a
/// refusal for any other code.
fn reply_outcome(bc: &Bc) -> Read<SettingsBody> {
    let code = bc.meta.response_code;
    if code != 200 && code != 300 {
        return Err(ReadFailure::Refused(code));
    }
    match &bc.body {
        BcBody::Modern(ModernMsg { payload: Some(payload), .. }) => SettingsBody::from_bytes(payload).ok_or(ReadFailure::Unreadable),
        _ => Err(ReadFailure::Unreadable),
    }
}

/// What `settings_reply` made of a message.
#[derive(Debug)]
pub(super) enum Routed {
    /// Not a reply to a settings read: route it as usual.
    NotOurs,
    /// A reply to a settings read, taken by its page; the page itself when
    /// this reply completed it. Nothing else may be done with the message.
    Consumed(Option<DeviceUpdate>),
}

/// The first check of `pushed_update`: a reply whose message number and id
/// are one of a settings page's reads is recorded there and consumed, and
/// the page is returned once it is complete. A reply that comes too late
/// (its page already given up or read again) is consumed all the same, so
/// its id is never mistaken for a message of its own. Only the id, channel,
/// code and length of such a reply are ever printed, never its body.
pub(super) fn settings_reply(bc: &Bc, pending: &Mutex<HashMap<u16, ImageKind>>) -> Routed {
    let msg_num = bc.meta.msg_num;
    let collector = match lock(pending).get(&msg_num) {
        Some(ImageKind::SettingsPage(collector)) => Arc::clone(collector),
        _ => return Routed::NotOurs,
    };
    if !lock(&collector).expects(msg_num, bc.meta.msg_id) {
        return Routed::NotOurs;
    }
    if std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
        let bytes = match &bc.body {
            BcBody::Modern(ModernMsg { payload: Some(p), .. }) => p.len(),
            _ => 0,
        };
        eprintln!(
            "DEBUG settings reply: msg={} channel={} code={} bytes={bytes}",
            bc.meta.msg_id, bc.meta.channel_id, bc.meta.response_code
        );
    }
    lock(pending).remove(&msg_num);
    let outcome = reply_outcome(bc);
    let mut collector = lock(&collector);
    if !collector.record(msg_num, outcome) {
        return Routed::Consumed(None);
    }
    Routed::Consumed(collector.finish_if_complete().map(DeviceUpdate::Settings))
}

/// A request is about to go out under `msg_num`, a number that comes round
/// again after 65536 requests: whatever an unanswered read of long ago left
/// under it goes, so the new request's reply is routed as its own and not
/// taken for the old read's (the two can share an id: the Light & sound
/// page reads 56, as the Encoding tab's query does).
pub(super) fn number_reused(pending: &Mutex<HashMap<u16, ImageKind>>, msg_num: u16) {
    lock(pending).remove(&msg_num);
}

impl ReolinkClient {
    /// Reads one settings page: sends its queries one after the other and
    /// reports the page as `DeviceUpdate::Settings` once every reply is in
    /// (or `SETTINGS_REPLY_TIMEOUT` has passed, the unanswered reads marked
    /// `NoReply`). A page still pending for the same target is dropped. The
    /// pending entries of a dropped or given-up read stay until its reply
    /// comes (`settings_reply` then consumes it) or the number comes round
    /// again (`number_reused`).
    pub async fn query_settings(&mut self, target: SettingsTarget, gates: &SettingsGates) {
        let reads = page_reads(target, gates);
        if let Some(old) = self.settings_pages.remove(&target) {
            lock(&old).mark_done();
        }
        let collector = Arc::new(Mutex::new(PageCollector::new(target, &reads)));
        let channel_id = target.channel();
        for (index, read) in reads.iter().enumerate() {
            if !read.send {
                continue;
            }
            let msg_num = self.next_msg_num();
            let request = Bc {
                meta: BcMeta { msg_id: read.msg_id, channel_id, stream_type: 0, msg_num, response_code: 0, class: 0x6414 },
                body: BcBody::Modern(ModernMsg { extension_xml: read.extension.to_bytes(), payload: read.body.clone() }),
            };
            lock(&collector).sent(index, msg_num);
            // Registered before the send: another task may read the reply
            // the moment the connection lock is released.
            lock(&self.pending_images).insert(msg_num, ImageKind::SettingsPage(Arc::clone(&collector)));
            if self.connection.lock().await.send_bc(&request, &self.encryption).await.is_err() {
                lock(&self.pending_images).remove(&msg_num);
                lock(&collector).failed(index, ReadFailure::NoReply);
            }
        }
        self.settings_pages.insert(target, Arc::clone(&collector));
        // Nothing to wait for (every read ruled out, or none could be sent).
        if let Some(page) = lock(&collector).finish_if_complete() {
            let _ = self.channel_updates_tx.send(DeviceUpdate::Settings(page));
            return;
        }
        let updates = self.channel_updates_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SETTINGS_REPLY_TIMEOUT).await;
            let page = {
                let mut collector = lock(&collector);
                if collector.is_done() {
                    return;
                }
                collector.fill_open(ReadFailure::NoReply);
                collector.finish_if_complete()
            };
            if let Some(page) = page {
                let _ = updates.send(DeviceUpdate::Settings(page));
            }
        });
    }

    /// Diagnostic-only (`REOLING_PROBE_SETTINGS=<channel>`): reads every
    /// device page and every camera page of `channel`, with no gating at
    /// all, and prints each page and a one-line outcome per read. Nothing
    /// is written; nothing is retried. Lines are tagged with the model,
    /// never the UID.
    pub async fn probe_settings(&mut self, channel: u8, identity: &DeviceIdentity, username: &str) {
        let tag = identity.model.as_deref().map(str::trim).filter(|m| !m.is_empty()).unwrap_or("device").to_string();
        let kind = DeviceKind::from_model(&tag);
        let gates = SettingsGates::everything(kind, username);
        eprintln!(
            "PROBE settings [{tag}]: kind {kind:?}; every read of every page is sent, whatever the model and abilities; camera pages use channel {channel}"
        );
        let targets = DevicePage::ALL
            .iter()
            .map(|page| SettingsTarget::Device { page: *page })
            .chain(CameraPage::ALL.iter().map(|page| SettingsTarget::Camera { channel, page: *page }));
        for target in targets {
            let started = Instant::now();
            self.query_settings(target, &gates).await;
            let page = self.probe_collect(target).await;
            let elapsed = started.elapsed();
            let lines = self.settings_pages.get(&target).map(|c| lock(c).summary()).unwrap_or_default();
            eprintln!("PROBE settings [{tag}]: {target:?} took {elapsed:.1?}");
            for line in lines {
                eprintln!("PROBE settings [{tag}]:   {line}");
            }
            match page {
                Some(page) => eprintln!("PROBE settings [{tag}]: {target:?}: {page:#?}"),
                None => eprintln!("PROBE settings [{tag}]: {target:?}: no page assembled"),
            }
        }
        eprintln!("PROBE settings [{tag}]: done");
    }

    /// Reads the connection until `target`'s page is complete, forwarding
    /// everything else the device sends as usual; after the timeout, the
    /// page as far as it got.
    async fn probe_collect(&mut self, target: SettingsTarget) -> Option<SettingsPage> {
        let wait = async {
            let mut image_chunks = HashMap::new();
            loop {
                let received = self.connection.lock().await.recv_bc(&self.encryption).await;
                let bc = match received {
                    Ok(bc) => bc,
                    Err(crate::Error::ReplyTimeout) => continue,
                    Err(_) => return None,
                };
                match pushed_update(&bc, &mut image_chunks, &self.pending_images, &self.channel_uids) {
                    Some(DeviceUpdate::Settings(page)) if page.target() == target => return Some(page),
                    Some(update) => {
                        let _ = self.channel_updates_tx.send(update);
                    }
                    None => {}
                }
            }
        };
        let page = tokio::time::timeout(SETTINGS_REPLY_TIMEOUT + Duration::from_secs(2), wait).await.ok().flatten();
        page.or_else(|| self.settings_pages.get(&target).map(|c| lock(c).assemble()))
    }
}

/// The channel `REOLING_PROBE_SETTINGS` asks the probe to run on.
pub fn probe_channel() -> Option<u8> {
    let value = std::env::var("REOLING_PROBE_SETTINGS").ok()?;
    match value.trim().parse() {
        Ok(channel) => Some(channel),
        Err(_) => {
            eprintln!("PROBE settings: REOLING_PROBE_SETTINGS must be a channel number, got {value:?}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_abilities() -> ChannelAbilities {
        ChannelAbilities {
            siren: true,
            spotlight: true,
            talk: true,
            pan: true,
            tilt: true,
            zoom: true,
            monitor_point: true,
            calibration: true,
            status_led: true,
            tracking: true,
            binning: true,
            pet_label: false,
        }
    }

    fn hub() -> SettingsGates {
        SettingsGates::new(DeviceKind::Hub, Some(all_abilities()), "someone")
    }

    fn nvr() -> SettingsGates {
        SettingsGates::new(DeviceKind::Nvr, Some(all_abilities()), "someone")
    }

    fn ids(reads: &[SettingsRead], sent: bool) -> Vec<u32> {
        reads.iter().filter(|r| r.send == sent).map(|r| r.msg_id).collect()
    }

    fn camera(channel: u8, page: CameraPage) -> SettingsTarget {
        SettingsTarget::Camera { channel, page }
    }

    fn device(page: DevicePage) -> SettingsTarget {
        SettingsTarget::Device { page }
    }

    #[test]
    fn device_kind_from_the_model_string() {
        assert_eq!(DeviceKind::from_model("Reolink Home Hub Pro"), DeviceKind::Hub);
        assert_eq!(DeviceKind::from_model("RLN36"), DeviceKind::Nvr);
        assert_eq!(DeviceKind::from_model("some NVR"), DeviceKind::Nvr);
        assert_eq!(DeviceKind::from_model("E1 Outdoor PoE"), DeviceKind::Camera);
        assert_eq!(DeviceKind::from_model(""), DeviceKind::Camera);
    }

    #[test]
    fn camera_pages_send_the_captured_reads_and_extensions() {
        use ReadExtension::{Channel, ChannelTyped};
        let g = hub();
        let reads = page_reads(camera(2, CameraPage::Info), &g);
        assert_eq!(reads.len(), 1);
        assert_eq!((reads[0].msg_id, &reads[0].extension), (318, &ChannelTyped(2)));
        let reads = page_reads(camera(2, CameraPage::Image), &g);
        assert_eq!(reads.iter().map(|r| (r.msg_id, r.extension.clone())).collect::<Vec<_>>(), vec![(26, Channel(2)), (296, ChannelTyped(2))]);
        let reads = page_reads(camera(2, CameraPage::Display), &g);
        assert_eq!(reads.iter().map(|r| (r.msg_id, r.extension.clone())).collect::<Vec<_>>(), vec![(44, Channel(2))]);
        let reads = page_reads(camera(2, CameraPage::Detection), &g);
        assert_eq!(ids(&reads, true), vec![299, 46, 342, 342, 342, 434, 436]);
        assert!(reads[0].extension == ChannelTyped(2) && reads[1].extension == ChannelTyped(2));
        for (read, kind) in reads[2..5].iter().zip(AiKind::ALL) {
            assert_eq!(read.extension, Channel(2));
            assert_eq!(read.slot, Slot::AiDetect(kind));
            let body = String::from_utf8(read.body.clone().unwrap()).unwrap();
            assert_eq!(
                body,
                format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<AiDetectCfg version=\"1.1\">\n<chn>2</chn>\n<type>{}</type>\n</AiDetectCfg>\n</body>\n", kind.wire())
            );
        }
        assert!(reads[5..].iter().all(|r| r.extension == ChannelTyped(2) && r.body.is_none()));
        let reads = page_reads(camera(2, CameraPage::LightSound), &g);
        assert_eq!(ids(&reads, true), vec![208, 289, 264, 439, 56]);
        assert!(reads.iter().all(|r| r.extension == ChannelTyped(2)));
        let reads = page_reads(camera(2, CameraPage::Schedules), &g);
        assert_eq!(ids(&reads, true), vec![81, 54, 217, 219, 363, 70, 232, 306]);
        assert!(reads.iter().all(|r| r.extension == ChannelTyped(2)));
        let reads = page_reads(camera(2, CameraPage::Network), &g);
        assert_eq!(reads.iter().map(|r| (r.msg_id, r.extension.clone())).collect::<Vec<_>>(), vec![(76, ChannelTyped(2))]);
    }

    #[test]
    fn device_pages_send_the_captured_reads() {
        use ReadExtension::{ChannelTyped, None as NoExt};
        let g = hub();
        let no_ext = |reads: &[SettingsRead]| reads.iter().all(|r| r.extension == NoExt && r.body.is_none());
        let reads = page_reads(device(DevicePage::Info), &g);
        assert_eq!(ids(&reads, true), vec![80]);
        assert!(no_ext(&reads));
        let reads = page_reads(device(DevicePage::Storage), &g);
        assert_eq!(ids(&reads, true), vec![102, 54, 507]);
        assert_eq!(reads[1].extension, ChannelTyped(0));
        assert!(reads[0].extension == NoExt && reads[2].extension == NoExt);
        let reads = page_reads(device(DevicePage::Network), &g);
        assert_eq!(ids(&reads, true), vec![76, 37, 97, 210, 40, 38, 455, 481]);
        assert!(no_ext(&reads));
        let reads = page_reads(device(DevicePage::Time), &g);
        assert_eq!(ids(&reads, true), vec![104, 106, 38]);
        assert!(no_ext(&reads));
        let reads = page_reads(device(DevicePage::LightSound), &g);
        assert_eq!(ids(&reads, true), vec![208, 504]);
        assert!(no_ext(&reads));
        let reads = page_reads(device(DevicePage::Users), &g);
        assert_eq!(ids(&reads, true), vec![58, 511, 104]);
        assert_eq!(reads[0].extension, ReadExtension::UserName("someone".to_string()));
        assert_eq!(reads[1].extension, ReadExtension::UserName("someone".to_string()));
        assert_eq!(reads[2].extension, NoExt);
        let bytes = reads[0].extension.to_bytes().unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<Extension version=\"1.1\">\n<userName>someone</userName>\n</Extension>\n"
        );
        assert!(!format!("{:?}", reads[0]).contains("someone"), "the account name never reaches a log");
        let reads = page_reads(device(DevicePage::Maintenance), &g);
        assert_eq!(ids(&reads, true), vec![101]);
        assert!(no_ext(&reads));
        let typed = ReadExtension::ChannelTyped(3).to_bytes().unwrap();
        assert_eq!(typed, super::monitor_point_read_extension(3));
        let plain = ReadExtension::Channel(3).to_bytes().unwrap();
        assert_eq!(Extension::from_bytes(&plain).unwrap().channel_id, Some(3));
    }

    #[test]
    fn gates_rule_reads_out_per_kind_and_ability() {
        // An NVR: no FTP or hub siren per channel, no camera Network tab,
        // no device Light & sound, no hub Wi-Fi; the alert switches instead.
        let g = nvr();
        let reads = page_reads(camera(0, CameraPage::Schedules), &g);
        assert_eq!(ids(&reads, false), vec![70, 306]);
        assert_eq!(ids(&page_reads(camera(0, CameraPage::Network), &g), false), vec![76]);
        assert_eq!(ids(&page_reads(device(DevicePage::LightSound), &g), false), vec![208, 504]);
        assert_eq!(ids(&page_reads(device(DevicePage::Network), &g), false), vec![481]);
        assert_eq!(ids(&page_reads(device(DevicePage::Alerts), &g), true), vec![308, 310, 314, 316]);
        // A Hub: the alert switches are an NVR's.
        let g = hub();
        assert_eq!(ids(&page_reads(device(DevicePage::Alerts), &g), false), vec![308, 310, 314, 316]);
        assert!(ids(&page_reads(camera(0, CameraPage::Schedules), &g), false).is_empty());
        // Abilities: no spotlight, no siren, no tracking.
        let plain = SettingsGates::new(DeviceKind::Hub, Some(ChannelAbilities::default()), "someone");
        assert_eq!(ids(&page_reads(camera(0, CameraPage::LightSound), &plain), false), vec![289, 264]);
        assert_eq!(ids(&page_reads(camera(0, CameraPage::Schedules), &plain), false), vec![232]);
        assert_eq!(ids(&page_reads(camera(0, CameraPage::Detection), &plain), false), vec![434, 436]);
        // Unknown abilities send everything the kind allows.
        let unknown = SettingsGates::new(DeviceKind::Nvr, None, "someone");
        assert!(ids(&page_reads(camera(0, CameraPage::LightSound), &unknown), false).is_empty());
        assert_eq!(ids(&page_reads(camera(0, CameraPage::Schedules), &unknown), false), vec![70, 306]);
        // The probe's gates send everything.
        let all = SettingsGates::everything(DeviceKind::Nvr, "someone");
        for page in CameraPage::ALL {
            assert!(ids(&page_reads(camera(0, page), &all), false).is_empty(), "{page:?}");
        }
        for page in DevicePage::ALL {
            assert!(ids(&page_reads(device(page), &all), false).is_empty(), "{page:?}");
        }
        assert!(!format!("{all:?}").contains("someone"));
    }

    fn body(inner: &str) -> SettingsBody {
        SettingsBody::from_bytes(format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n{inner}</body>\n").as_bytes()).unwrap()
    }

    #[test]
    fn collector_assembles_mixed_outcomes() {
        let reads = page_reads(camera(1, CameraPage::Image), &hub());
        let mut c = PageCollector::new(camera(1, CameraPage::Image), &reads);
        assert!(!c.complete());
        c.sent(0, 10);
        c.sent(1, 11);
        assert!(c.expects(10, 26) && c.expects(11, 296) && !c.expects(10, 296) && !c.expects(12, 26));
        // The picture reply lacks its second root; the threshold read is refused.
        assert!(c.record(10, Ok(body("<VideoInput version=\"1.1\">\n<channelId>1</channelId>\n<bright>128</bright>\n</VideoInput>\n"))));
        assert!(!c.record(10, Err(ReadFailure::Unreadable)), "a duplicate is ignored");
        assert!(!c.record(99, Err(ReadFailure::Unreadable)), "a stranger is ignored");
        assert!(!c.complete());
        assert!(c.record(11, Err(ReadFailure::Refused(400))));
        assert!(c.complete());
        let page = c.finish_if_complete().unwrap();
        assert!(c.finish_if_complete().is_none(), "a page is reported once");
        let SettingsPage::CameraImage { channel, page } = page else { panic!("wrong page") };
        assert_eq!(channel, 1);
        assert_eq!(page.video_input.unwrap().bright, Some(128));
        assert_eq!(page.advanced, Err(ReadFailure::Missing));
        assert_eq!(page.threshold, Err(ReadFailure::Refused(400)));
    }

    #[test]
    fn timeout_fills_the_open_reads_and_not_sent_stays() {
        let plain = SettingsGates::new(DeviceKind::Nvr, Some(ChannelAbilities::default()), "someone");
        let reads = page_reads(camera(0, CameraPage::Schedules), &plain);
        let mut c = PageCollector::new(camera(0, CameraPage::Schedules), &reads);
        for (i, _) in reads.iter().enumerate().filter(|(_, r)| r.send) {
            c.sent(i, 20 + i as u16);
        }
        assert!(c.record(20, Ok(body(
            "<Record version=\"1.1\">\n<channelId>0</channelId>\n<enable>1</enable>\n<typeScheduleList>\n<item>\n<type>MD</type>\n<valueTable>1</valueTable>\n</item>\n</typeScheduleList>\n</Record>\n"
        ))));
        c.failed(1, ReadFailure::NoReply);
        assert!(!c.complete());
        c.fill_open(ReadFailure::NoReply);
        let SettingsPage::CameraSchedules { page, .. } = c.finish_if_complete().unwrap() else { panic!("wrong page") };
        assert_eq!(page.record.as_ref().unwrap().enable, Some(1));
        assert_eq!(page.record.unwrap().items()[0].1, None);
        assert_eq!(page.record_cfg, Err(ReadFailure::NoReply));
        assert_eq!(page.email, Err(ReadFailure::NoReply));
        assert_eq!(page.ftp, Err(ReadFailure::NotSent));
        assert_eq!(page.siren, Err(ReadFailure::NotSent));
        assert_eq!(page.hub_siren, Err(ReadFailure::NotSent));
    }

    #[test]
    fn smart_detection_replies_are_assigned_by_type() {
        let reads = page_reads(camera(0, CameraPage::Detection), &hub());
        let mut c = PageCollector::new(camera(0, CameraPage::Detection), &reads);
        for i in 0..reads.len() {
            c.sent(i, 30 + i as u16);
        }
        let detect = |kind: &str, sensitivity: u32| {
            body(&format!("<AiDetectCfg version=\"1.1\">\n<chn>0</chn>\n<type>{kind}</type>\n<sensitivity>{sensitivity}</sensitivity>\n</AiDetectCfg>\n"))
        };
        // The replies come back under swapped numbers: the type decides.
        assert!(c.record(32, Ok(detect("dog_cat", 3))));
        assert!(c.record(34, Ok(detect("people", 1))));
        // A type that is already filled falls back to the number's slot.
        assert!(c.record(33, Ok(detect("people", 2))));
        c.fill_open(ReadFailure::NoReply);
        let SettingsPage::CameraDetection { page, .. } = c.assemble() else { panic!("wrong page") };
        assert_eq!(page.people.unwrap().sensitivity, Some(1));
        assert_eq!(page.vehicle.unwrap().sensitivity, Some(2));
        assert_eq!(page.animal.unwrap().sensitivity, Some(3));
        assert_eq!(page.ai, Err(ReadFailure::NoReply));
    }

    #[test]
    fn every_page_assembles_to_its_own_target() {
        let all = SettingsGates::everything(DeviceKind::Hub, "someone");
        let targets = CameraPage::ALL
            .iter()
            .map(|p| camera(5, *p))
            .chain(DevicePage::ALL.iter().map(|p| device(*p)))
            .collect::<Vec<_>>();
        for target in targets {
            let reads = page_reads(target, &all);
            assert!(!reads.is_empty(), "{target:?}");
            let mut c = PageCollector::new(target, &reads);
            c.fill_open(ReadFailure::NoReply);
            assert_eq!(c.assemble().target(), target);
            assert_eq!(c.summary().len(), reads.len());
            assert!(c.summary().iter().all(|l| l.ends_with(": no reply")), "{:?}", c.summary());
        }
    }

    #[test]
    fn device_network_and_time_pages_split_their_shared_replies() {
        let reads = page_reads(device(DevicePage::Network), &hub());
        let mut c = PageCollector::new(device(DevicePage::Network), &reads);
        for i in 0..reads.len() {
            c.sent(i, 40 + i as u16);
        }
        assert!(c.record(40, Ok(body(
            "<Dhcp version=\"1.1\">\n<enable>1</enable>\n</Dhcp>\n<AutoDns version=\"1.1\">\n<enable>1</enable>\n</AutoDns>\n<Ip version=\"1.1\">\n<ip>192.0.2.5</ip>\n<mask>255.255.255.0</mask>\n<mac>00:00:00:00:00:00</mac>\n<gateway>192.0.2.1</gateway>\n</Ip>\n<Dns version=\"1.1\">\n<dns1>192.0.2.1</dns1>\n<dns2></dns2>\n</Dns>\n"
        ))));
        assert!(c.record(41, Ok(body(
            "<ServerPort version=\"1.1\">\n<serverPort>9000</serverPort>\n<enable>0</enable>\n</ServerPort>\n<HttpPort version=\"1.1\">\n<httpPort>80</httpPort>\n<enable>0</enable>\n</HttpPort>\n"
        ))));
        assert!(c.record(47, Err(ReadFailure::Refused(405))));
        c.fill_open(ReadFailure::NoReply);
        let SettingsPage::DeviceNetwork(page) = c.assemble() else { panic!("wrong page") };
        assert_eq!(page.network.dhcp.unwrap().enable, Some(1));
        assert_eq!(page.network.ip.unwrap().gateway.as_deref(), Some("192.0.2.1"));
        assert_eq!(page.server.unwrap().port(), Some(9000));
        assert_eq!(page.http.unwrap().port(), Some(80));
        assert_eq!(page.https, Err(ReadFailure::Missing));
        assert_eq!(page.upnp, Err(ReadFailure::NoReply));
        assert_eq!(page.hub_ap, Err(ReadFailure::Refused(405)));
        let summary = c.summary();
        assert_eq!(summary[0], "msg 76: ok, roots [\"Dhcp\", \"AutoDns\", \"Ip\", \"Dns\"]");
        assert_eq!(summary[7], "msg 481: refused 405");
        assert!(!summary.concat().contains("00:00:00"));
    }

    fn reply(msg_id: u32, msg_num: u16, code: u16, payload: Option<&str>) -> Bc {
        Bc {
            meta: BcMeta { msg_id, channel_id: 0, stream_type: 0, msg_num, response_code: code, class: 0x6414 },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: payload.map(|p| p.as_bytes().to_vec()) }),
        }
    }

    #[test]
    fn replies_are_routed_through_the_pending_map() {
        let reads = page_reads(device(DevicePage::Maintenance), &hub());
        let collector = Arc::new(Mutex::new(PageCollector::new(device(DevicePage::Maintenance), &reads)));
        lock(&collector).sent(0, 7);
        let pending = Mutex::new(HashMap::new());
        lock(&pending).insert(7u16, ImageKind::SettingsPage(Arc::clone(&collector)));
        lock(&pending).insert(8u16, ImageKind::Preset(2));
        // Not ours: an image request's number, an unknown number, a wrong id.
        assert!(matches!(settings_reply(&reply(101, 8, 200, None), &pending), Routed::NotOurs));
        assert!(matches!(settings_reply(&reply(101, 9, 200, None), &pending), Routed::NotOurs));
        assert!(matches!(settings_reply(&reply(80, 7, 200, None), &pending), Routed::NotOurs));
        assert!(lock(&pending).contains_key(&7));
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<AutoReboot version=\"1.1\">\n<enable>1</enable>\n<weekDay>Sunday</weekDay>\n<hour>11</hour>\n<minute>0</minute>\n<second>0</second>\n</AutoReboot>\n</body>\n";
        let Routed::Consumed(Some(DeviceUpdate::Settings(SettingsPage::DeviceMaintenance(page)))) =
            settings_reply(&reply(101, 7, 200, Some(xml)), &pending)
        else {
            panic!("the complete page is returned")
        };
        assert_eq!(page.auto_reboot.unwrap().week_day.as_deref(), Some("Sunday"));
        assert!(!lock(&pending).contains_key(&7), "the number is freed");
        assert!(lock(&collector).is_done());
        // A reply that does not complete its page is consumed all the same:
        // the Light & sound page's 56 must never become an encoding update.
        let reads = page_reads(camera(0, CameraPage::LightSound), &hub());
        let collector = Arc::new(Mutex::new(PageCollector::new(camera(0, CameraPage::LightSound), &reads)));
        for i in 0..reads.len() {
            lock(&collector).sent(i, 20 + i as u16);
            lock(&pending).insert(20 + i as u16, ImageKind::SettingsPage(Arc::clone(&collector)));
        }
        let compression = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<Compression version=\"1.1\">\n<channelId>0</channelId>\n<mainStream>\n<audio>1</audio>\n</mainStream>\n</Compression>\n</body>\n";
        assert!(matches!(settings_reply(&reply(56, 24, 200, Some(compression)), &pending), Routed::Consumed(None)));
        assert!(!lock(&pending).contains_key(&24) && lock(&pending).contains_key(&20));
        assert!(!lock(&collector).is_done());
        // Given up (the timeout): a late reply is still consumed, not routed
        // by its id, and its number is freed.
        lock(&collector).fill_open(ReadFailure::NoReply);
        assert!(lock(&collector).finish_if_complete().is_some());
        assert!(matches!(settings_reply(&reply(208, 20, 200, None), &pending), Routed::Consumed(None)));
        assert!(!lock(&pending).contains_key(&20));
        // A number another request took over is that request's.
        lock(&pending).insert(21, ImageKind::Preset(1));
        assert!(matches!(settings_reply(&reply(289, 21, 200, None), &pending), Routed::NotOurs));
        // Outcomes of the other reply shapes.
        assert_eq!(reply_outcome(&reply(101, 1, 400, None)), Err(ReadFailure::Refused(400)));
        assert_eq!(reply_outcome(&reply(101, 1, 200, None)), Err(ReadFailure::Unreadable));
        assert_eq!(reply_outcome(&reply(101, 1, 200, Some("<nonsense"))), Err(ReadFailure::Unreadable));
        assert!(reply_outcome(&reply(101, 1, 300, Some(xml))).is_ok());
    }

    #[test]
    fn a_number_taken_again_belongs_to_the_new_request() {
        // A Light & sound read of 56 the device never answered: given up
        // (the timeout), its entry stays for the late reply...
        let reads = page_reads(camera(0, CameraPage::LightSound), &hub());
        let collector = Arc::new(Mutex::new(PageCollector::new(camera(0, CameraPage::LightSound), &reads)));
        let index = reads.iter().position(|r| r.msg_id == MSG_ID_GET_COMPRESSION).unwrap();
        lock(&collector).sent(index, 40);
        let pending = Mutex::new(HashMap::new());
        lock(&pending).insert(40u16, ImageKind::SettingsPage(Arc::clone(&collector)));
        lock(&collector).fill_open(ReadFailure::NoReply);
        assert!(lock(&collector).finish_if_complete().is_some());
        // ...until the counter comes round and the Encoding tab's query
        // (56 as well) goes out under the same number: its reply is that
        // query's, not the old read's to swallow.
        number_reused(&pending, 40);
        assert!(!lock(&pending).contains_key(&40));
        let compression = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<Compression version=\"1.1\">\n<channelId>0</channelId>\n</Compression>\n</body>\n";
        assert!(matches!(settings_reply(&reply(56, 40, 200, Some(compression)), &pending), Routed::NotOurs));
        // A number nobody holds is left alone.
        number_reused(&pending, 41);
        assert!(lock(&pending).is_empty());
    }

    #[test]
    fn ai_kind_names_round_trip() {
        for kind in AiKind::ALL {
            assert_eq!(AiKind::from_wire(kind.wire()), Some(kind));
        }
        assert_eq!(AiKind::from_wire(" vehicle "), Some(AiKind::Vehicle));
        assert_eq!(AiKind::from_wire("face"), None);
    }
}
