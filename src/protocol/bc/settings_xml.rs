//! The XML bodies a device answers the read-only settings queries with (see
//! `client::settings`): one serde struct per root element, with the element
//! names exactly as the devices write them (`DeiveAudioCfg` included; the
//! misspelt `AbilitySuppport` root of the user-list reply is not parsed at
//! all, only its `UserList`). Every field is optional, every list defaults
//! to empty, unknown elements are skipped, and parsing never panics: a body
//! that cannot be read at all is `None`.
//!
//! Nothing that holds a secret is parsed. The elements that are deliberately
//! not fields anywhere in this module: `password`, `pwdValid`,
//! `validpassword`, `pwdMaxLen`, `key`, `certName`, `keyName`, `ddnsName`,
//! `Ddns/userName`, `notes`, `userId`, `phoneModel`, `serialNumber`, `uid`,
//! `fakeUid`, the image names of the tracking-range reply, and the whole of
//! the e-mail and FTP server settings. The MAC address, the hub's Wi-Fi
//! network name and account names are parsed into newtypes whose `Debug`
//! hides them, so a diagnostic print of a parsed body never shows them.

use serde::{Deserialize, Deserializer};
use std::fmt;
use std::str::FromStr;

pub use crate::protocol::bc::xml::VersionInfo;

/// A number as the device writes it. An empty or unparsable element gives
/// `None` instead of failing the whole body, so one odd value never hides
/// the rest of a reply.
fn num<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
{
    let text: Option<String> = Option::deserialize(deserializer)?;
    Ok(text.and_then(|t| t.trim().parse().ok()))
}

/// A list of repeated numeric elements, read as `num` reads one: an item
/// that is empty or not a number is left out instead of failing the body.
fn num_list<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
{
    let texts: Vec<String> = Vec::deserialize(deserializer)?;
    Ok(texts.iter().filter_map(|t| t.trim().parse().ok()).collect())
}

/// A reply body: every root this module knows, each optional. A reply
/// carries one root, or a few that belong together (`Dhcp`, `AutoDns`, `Ip`
/// and `Dns` come in one body; so do `VideoInput` and `InputAdvanceCfg`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename = "body")]
pub struct SettingsBody {
    #[serde(rename = "VersionInfo")]
    pub version_info: Option<VersionInfo>,
    #[serde(rename = "VideoInput")]
    pub video_input: Option<VideoInput>,
    #[serde(rename = "InputAdvanceCfg")]
    pub input_advance: Option<InputAdvanceCfg>,
    #[serde(rename = "DayNightThreshold")]
    pub day_night_threshold: Option<DayNightThreshold>,
    #[serde(rename = "OsdDatetime")]
    pub osd_datetime: Option<OsdDatetime>,
    #[serde(rename = "OsdChannelName")]
    pub osd_channel_name: Option<OsdName>,
    #[serde(rename = "AiCfg")]
    pub ai_cfg: Option<AiCfg>,
    #[serde(rename = "MD")]
    pub motion: Option<MotionDetect>,
    #[serde(rename = "AiDetectCfg")]
    pub ai_detect: Option<AiDetectCfg>,
    #[serde(rename = "trackLimit")]
    pub track_limit: Option<TrackLimit>,
    #[serde(rename = "trackSchedule")]
    pub track_schedule: Option<TrackSchedule>,
    #[serde(rename = "LedState")]
    pub led_state: Option<LedState>,
    #[serde(rename = "FloodlightTask")]
    pub floodlight_task: Option<FloodlightTask>,
    #[serde(rename = "audioCfg")]
    pub audio_cfg: Option<AudioCfg>,
    #[serde(rename = "aiDenoise")]
    pub ai_denoise: Option<AiDenoise>,
    #[serde(rename = "Compression")]
    pub compression: Option<CompressionAudio>,
    #[serde(rename = "Record")]
    pub record: Option<ScheduledTask>,
    #[serde(rename = "EmailTask")]
    pub email_task: Option<ScheduledTask>,
    #[serde(rename = "PushTask")]
    pub push_task: Option<ScheduledTask>,
    #[serde(rename = "FtpTask")]
    pub ftp_task: Option<ScheduledTask>,
    #[serde(rename = "AudioTask")]
    pub audio_task: Option<ScheduledTask>,
    #[serde(rename = "BuzzerTask")]
    pub buzzer_task: Option<ScheduledTask>,
    #[serde(rename = "RecordCfg")]
    pub record_cfg: Option<RecordCfg>,
    #[serde(rename = "PushCfg")]
    pub push_cfg: Option<PushCfg>,
    #[serde(rename = "Dhcp")]
    pub dhcp: Option<Enable>,
    #[serde(rename = "AutoDns")]
    pub auto_dns: Option<Enable>,
    #[serde(rename = "Ip")]
    pub ip: Option<IpConfig>,
    #[serde(rename = "Dns")]
    pub dns: Option<DnsConfig>,
    #[serde(rename = "HddInfoList")]
    pub hdd_info_list: Option<HddInfoList>,
    #[serde(rename = "RecEncCfg")]
    pub rec_enc_cfg: Option<Enable>,
    #[serde(rename = "ServerPort")]
    pub server_port: Option<ServicePort>,
    #[serde(rename = "HttpPort")]
    pub http_port: Option<ServicePort>,
    #[serde(rename = "HttpsPort")]
    pub https_port: Option<ServicePort>,
    #[serde(rename = "RtspPort")]
    pub rtsp_port: Option<ServicePort>,
    #[serde(rename = "RtmpPort")]
    pub rtmp_port: Option<ServicePort>,
    #[serde(rename = "OnvifPort")]
    pub onvif_port: Option<ServicePort>,
    #[serde(rename = "Upnp")]
    pub upnp: Option<Enable>,
    #[serde(rename = "PTOP")]
    pub ptop: Option<Enable>,
    #[serde(rename = "Ddns")]
    pub ddns: Option<Ddns>,
    #[serde(rename = "Ntp")]
    pub ntp: Option<Ntp>,
    #[serde(rename = "certificateInfo")]
    pub certificate_info: Option<Enable>,
    #[serde(rename = "kitApCfg")]
    pub kit_ap_cfg: Option<KitApCfg>,
    #[serde(rename = "SystemGeneral")]
    pub system_general: Option<SystemGeneral>,
    #[serde(rename = "Norm")]
    pub norm: Option<Norm>,
    #[serde(rename = "Dst")]
    pub dst: Option<Dst>,
    /// The device spells the element `DeiveAudioCfg`.
    #[serde(rename = "DeiveAudioCfg")]
    pub device_audio_cfg: Option<DeviceAudioCfg>,
    #[serde(rename = "RecordEnable")]
    pub record_enable: Option<ChannelSwitch>,
    #[serde(rename = "EmailEnable")]
    pub email_enable: Option<ChannelSwitch>,
    #[serde(rename = "PushEnable")]
    pub push_enable: Option<ChannelSwitch>,
    #[serde(rename = "BuzzerEnable")]
    pub buzzer_enable: Option<ChannelSwitch>,
    #[serde(rename = "UserList")]
    pub user_list: Option<UserList>,
    #[serde(rename = "accessUserList")]
    pub access_user_list: Option<AccessUserList>,
    #[serde(rename = "AutoReboot")]
    pub auto_reboot: Option<AutoReboot>,
}

impl SettingsBody {
    /// Parses a reply payload; `None` when it is not a readable body.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        quick_xml::de::from_reader(bytes).ok()
    }

    /// The names of the roots this body carries (for diagnostics).
    pub fn roots(&self) -> Vec<&'static str> {
        let mut roots = Vec::new();
        macro_rules! root {
            ($($field:ident => $name:literal),* $(,)?) => {
                $( if self.$field.is_some() { roots.push($name); } )*
            };
        }
        root!(
            version_info => "VersionInfo", video_input => "VideoInput", input_advance => "InputAdvanceCfg",
            day_night_threshold => "DayNightThreshold", osd_datetime => "OsdDatetime",
            osd_channel_name => "OsdChannelName", ai_cfg => "AiCfg", motion => "MD", ai_detect => "AiDetectCfg",
            track_limit => "trackLimit", track_schedule => "trackSchedule", led_state => "LedState",
            floodlight_task => "FloodlightTask", audio_cfg => "audioCfg", ai_denoise => "aiDenoise",
            compression => "Compression", record => "Record", email_task => "EmailTask", push_task => "PushTask",
            ftp_task => "FtpTask", audio_task => "AudioTask", buzzer_task => "BuzzerTask", record_cfg => "RecordCfg",
            push_cfg => "PushCfg", dhcp => "Dhcp", auto_dns => "AutoDns", ip => "Ip", dns => "Dns",
            hdd_info_list => "HddInfoList", rec_enc_cfg => "RecEncCfg", server_port => "ServerPort",
            http_port => "HttpPort", https_port => "HttpsPort", rtsp_port => "RtspPort", rtmp_port => "RtmpPort",
            onvif_port => "OnvifPort", upnp => "Upnp", ptop => "PTOP", ddns => "Ddns", ntp => "Ntp",
            certificate_info => "certificateInfo", kit_ap_cfg => "kitApCfg", system_general => "SystemGeneral",
            norm => "Norm", dst => "Dst", device_audio_cfg => "DeiveAudioCfg", record_enable => "RecordEnable",
            email_enable => "EmailEnable", push_enable => "PushEnable", buzzer_enable => "BuzzerEnable",
            user_list => "UserList", access_user_list => "accessUserList", auto_reboot => "AutoReboot",
        );
        roots
    }
}

// ---------------------------------------------------------------------------
// Masked newtypes: values a settings page may show but a log must never hold.
// ---------------------------------------------------------------------------

macro_rules! masked_text {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Default, PartialEq, Eq, Hash, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(text: impl Into<String>) -> Self {
                $name(text.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn is_empty(&self) -> bool {
                self.0.trim().is_empty()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "(<hidden>)"))
            }
        }
    };
}

masked_text!(
    /// A device's MAC address. Shown in the settings window, never logged:
    /// `Debug` hides it and there is no `Display`.
    MacAddress
);
masked_text!(
    /// The network name of the hub's own Wi-Fi for its cameras. Shown in
    /// the settings window, never logged.
    Ssid
);
masked_text!(
    /// An account name on the device. Shown in the settings window, never
    /// logged.
    AccountName
);

// ---------------------------------------------------------------------------
// Shared pieces
// ---------------------------------------------------------------------------

/// A lone `enable` flag (`Dhcp`, `AutoDns`, `RecEncCfg`, `Upnp`, `PTOP`,
/// `certificateInfo`, `BLC`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Enable {
    #[serde(default, deserialize_with = "num")]
    pub enable: Option<u32>,
}

/// A value with its range.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct MinMaxCur {
    #[serde(default, deserialize_with = "num")]
    pub min: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub max: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub cur: Option<u32>,
}

/// An element whose only read part is its `mode` text.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ModeOnly {
    pub mode: Option<String>,
}

/// A daily span of hours and minutes.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct HourSpan {
    #[serde(rename = "startHour", default, deserialize_with = "num")]
    pub start_hour: Option<u32>,
    #[serde(rename = "startMin", default, deserialize_with = "num")]
    pub start_min: Option<u32>,
    #[serde(rename = "endHour", default, deserialize_with = "num")]
    pub end_hour: Option<u32>,
    #[serde(rename = "endMin", default, deserialize_with = "num")]
    pub end_min: Option<u32>,
}

/// A 7 x 24 on/off table as the task schedules and the tracking schedule
/// carry it: 168 characters of `0` and `1`, index `day * 24 + hour`. The
/// first day is taken as Sunday; the devices give no day names, so the day
/// order is an assumption until a capture with a one-day schedule settles
/// it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ScheduleTable([bool; 168]);

impl ScheduleTable {
    pub const DAYS: usize = 7;
    pub const HOURS: usize = 24;

    /// Exactly 168 characters of `0`/`1`; anything else is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.len() != Self::DAYS * Self::HOURS {
            return None;
        }
        let mut cells = [false; 168];
        for (cell, c) in cells.iter_mut().zip(text.chars()) {
            *cell = match c {
                '0' => false,
                '1' => true,
                _ => return None,
            };
        }
        Some(ScheduleTable(cells))
    }

    /// Whether the cell at `day` (0 = the first day, Sunday) and `hour` is
    /// set; out of range is `false`.
    pub fn on(&self, day: usize, hour: usize) -> bool {
        if day >= Self::DAYS || hour >= Self::HOURS {
            return false;
        }
        self.0[day * Self::HOURS + hour]
    }

    pub fn any(&self) -> bool {
        self.0.iter().any(|c| *c)
    }

    pub fn all(&self) -> bool {
        self.0.iter().all(|c| *c)
    }

    pub fn cells(&self) -> &[bool; 168] {
        &self.0
    }
}

impl fmt::Debug for ScheduleTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ScheduleTable(")?;
        for day in 0..Self::DAYS {
            if day > 0 {
                write!(f, "/")?;
            }
            for hour in 0..Self::HOURS {
                write!(f, "{}", u8::from(self.on(day, hour)))?;
            }
        }
        write!(f, ")")
    }
}

// ---------------------------------------------------------------------------
// Camera: image
// ---------------------------------------------------------------------------

/// Picture settings (the first root of the reply to 26).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct VideoInput {
    #[serde(rename = "channelId", default, deserialize_with = "num")]
    pub channel_id: Option<u8>,
    #[serde(default, deserialize_with = "num")]
    pub bright: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub contrast: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub saturation: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub hue: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub sharpen: Option<u32>,
    #[serde(rename = "corridorMode")]
    pub corridor_mode: Option<String>,
    #[serde(rename = "corridorAbility", default, deserialize_with = "num")]
    pub corridor_ability: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct PowerLineFrequency {
    pub mode: Option<String>,
    #[serde(default, deserialize_with = "num")]
    pub enable: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct DayNight {
    pub mode: Option<String>,
    #[serde(rename = "IrcutMode")]
    pub ircut_mode: Option<String>,
    #[serde(rename = "Threshold")]
    pub threshold: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct NoiseReduction {
    pub value: Option<String>,
    #[serde(default, deserialize_with = "num")]
    pub enable: Option<u32>,
}

/// Advanced picture settings (the second root of the reply to 26). The
/// exposure, scene, iris and first-frame parts are not read.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct InputAdvanceCfg {
    #[serde(rename = "channelId", default, deserialize_with = "num")]
    pub channel_id: Option<u8>,
    #[serde(default, deserialize_with = "num")]
    pub flip: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub mirror: Option<u32>,
    #[serde(rename = "PowerLineFrequency")]
    pub power_line_frequency: Option<PowerLineFrequency>,
    #[serde(rename = "DayNight")]
    pub day_night: Option<DayNight>,
    #[serde(rename = "bdDayColor")]
    pub bd_day_color: Option<ModeOnly>,
    #[serde(rename = "bdNight")]
    pub bd_night: Option<ModeOnly>,
    #[serde(rename = "bdNightColor")]
    pub bd_night_color: Option<ModeOnly>,
    #[serde(rename = "BLC")]
    pub blc: Option<Enable>,
    #[serde(rename = "nr3d")]
    pub nr3d: Option<NoiseReduction>,
    #[serde(rename = "constantFrameRate", default, deserialize_with = "num")]
    pub constant_frame_rate: Option<u32>,
    #[serde(rename = "constantFrameRateAbility", default, deserialize_with = "num")]
    pub constant_frame_rate_ability: Option<u32>,
    #[serde(rename = "encType", default, deserialize_with = "num")]
    pub enc_type: Option<u32>,
    #[serde(rename = "encTypeAbility", default, deserialize_with = "num")]
    pub enc_type_ability: Option<u32>,
    #[serde(rename = "binning_mode", default, deserialize_with = "num")]
    pub binning_mode: Option<u32>,
}

/// The day/night switching threshold (reply to 296).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct DayNightThreshold {
    #[serde(rename = "channelId", default, deserialize_with = "num")]
    pub channel_id: Option<u8>,
    pub threshold: Option<String>,
    pub stat: Option<String>,
    #[serde(rename = "thresholdval")]
    pub threshold_value: Option<MinMaxCur>,
}

// ---------------------------------------------------------------------------
// Camera: display
// ---------------------------------------------------------------------------

/// The on-screen clock (first root of the reply to 44). Positions: 0 or 1 is
/// the start edge, 65536 the far edge, 65537 the centre.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct OsdDatetime {
    #[serde(rename = "channelId", default, deserialize_with = "num")]
    pub channel_id: Option<u8>,
    #[serde(default, deserialize_with = "num")]
    pub enable: Option<u32>,
    #[serde(rename = "topLeftX", default, deserialize_with = "num")]
    pub top_left_x: Option<u32>,
    #[serde(rename = "topLeftY", default, deserialize_with = "num")]
    pub top_left_y: Option<u32>,
    pub language: Option<String>,
}

/// The on-screen camera name (second root of the reply to 44).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct OsdName {
    #[serde(rename = "channelId", default, deserialize_with = "num")]
    pub channel_id: Option<u8>,
    pub name: Option<String>,
    #[serde(default, deserialize_with = "num")]
    pub enable: Option<u32>,
    #[serde(rename = "topLeftX", default, deserialize_with = "num")]
    pub top_left_x: Option<u32>,
    #[serde(rename = "topLeftY", default, deserialize_with = "num")]
    pub top_left_y: Option<u32>,
    #[serde(rename = "enWatermark", default, deserialize_with = "num")]
    pub watermark: Option<u32>,
    #[serde(rename = "enBgcolor", default, deserialize_with = "num")]
    pub background: Option<u32>,
}

// ---------------------------------------------------------------------------
// Camera: detection
// ---------------------------------------------------------------------------

/// Smart detection and auto tracking (reply to 299).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct AiCfg {
    #[serde(rename = "channelId", default, deserialize_with = "num")]
    pub channel_id: Option<u8>,
    #[serde(rename = "smartTrack", default, deserialize_with = "num")]
    pub smart_track: Option<u32>,
    #[serde(rename = "smartTrackMode", default, deserialize_with = "num")]
    pub smart_track_mode: Option<u32>,
    #[serde(rename = "smartTrackModeAbility", default, deserialize_with = "num")]
    pub smart_track_mode_ability: Option<u32>,
    /// Comma separated, e.g. `people,vehicle,dog_cat`.
    #[serde(rename = "detectType")]
    pub detect_type: Option<String>,
    #[serde(rename = "smartTrackType")]
    pub smart_track_type: Option<String>,
    #[serde(rename = "smartTrackPt", default, deserialize_with = "num")]
    pub smart_track_pt: Option<u32>,
    #[serde(rename = "smartTrackObjectStopDelay", default, deserialize_with = "num")]
    pub smart_track_object_stop_delay: Option<u32>,
    #[serde(rename = "smartTrackObjectDisappearDelay", default, deserialize_with = "num")]
    pub smart_track_object_disappear_delay: Option<u32>,
    #[serde(rename = "cryDetectLevel", default, deserialize_with = "num")]
    pub cry_detect_level: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct SensitivityInfo {
    #[serde(default, deserialize_with = "num")]
    pub id: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub sensitivity: Option<u32>,
    #[serde(rename = "beginHour", default, deserialize_with = "num")]
    pub begin_hour: Option<u32>,
    #[serde(rename = "beginMinute", default, deserialize_with = "num")]
    pub begin_minute: Option<u32>,
    #[serde(rename = "endHour", default, deserialize_with = "num")]
    pub end_hour: Option<u32>,
    #[serde(rename = "endMinute", default, deserialize_with = "num")]
    pub end_minute: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct SensitivityInfoList {
    #[serde(rename = "sensitivityInfo", default)]
    pub items: Vec<SensitivityInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct SensitivityNew {
    #[serde(rename = "sensitivityDefault", default, deserialize_with = "num")]
    pub sensitivity_default: Option<u32>,
}

/// Motion detection (reply to 46). The detection-area bitmap (`scope`) is
/// not read.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct MotionDetect {
    #[serde(rename = "channelId", default, deserialize_with = "num")]
    pub channel_id: Option<u8>,
    #[serde(default, deserialize_with = "num")]
    pub enable: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub usepir: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub width: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub height: Option<u32>,
    #[serde(rename = "sensitivityInfoList")]
    pub sensitivity_info_list: Option<SensitivityInfoList>,
    #[serde(rename = "sensInfoNew")]
    pub sens_info_new: Option<SensitivityNew>,
}

impl MotionDetect {
    /// The legacy four-period sensitivities.
    pub fn periods(&self) -> &[SensitivityInfo] {
        self.sensitivity_info_list.as_ref().map(|l| l.items.as_slice()).unwrap_or(&[])
    }
}

/// One smart-detection type's settings (reply to 342, asked per type). The
/// detection-area bitmap (`area`) is not read.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct AiDetectCfg {
    #[serde(rename = "chn", default, deserialize_with = "num")]
    pub channel: Option<u8>,
    /// `people`, `vehicle` or `dog_cat`.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    #[serde(default, deserialize_with = "num")]
    pub sensitivity: Option<u32>,
    #[serde(rename = "stayTime", default, deserialize_with = "num")]
    pub stay_time: Option<u32>,
    #[serde(rename = "minTargetWidth", default, deserialize_with = "num")]
    pub min_target_width: Option<u32>,
    #[serde(rename = "minTargetHeight", default, deserialize_with = "num")]
    pub min_target_height: Option<u32>,
    #[serde(rename = "maxTargetWidth", default, deserialize_with = "num")]
    pub max_target_width: Option<u32>,
    #[serde(rename = "maxTargetHeight", default, deserialize_with = "num")]
    pub max_target_height: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub width: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub height: Option<u32>,
}

/// The horizontal auto-tracking range (reply to 434); the two image names
/// are not read.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct TrackLimit {
    #[serde(rename = "channelId", default, deserialize_with = "num")]
    pub channel_id: Option<u8>,
    #[serde(rename = "leftLimit", default, deserialize_with = "num")]
    pub left_limit: Option<u32>,
    #[serde(rename = "rightLimit", default, deserialize_with = "num")]
    pub right_limit: Option<u32>,
}

/// When auto tracking is active (reply to 436): a 7 x 24 table.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct TrackSchedule {
    #[serde(rename = "channelId", default, deserialize_with = "num")]
    pub channel_id: Option<u8>,
    #[serde(rename = "timeTable")]
    pub time_table: Option<String>,
}

impl TrackSchedule {
    pub fn table(&self) -> Option<ScheduleTable> {
        self.time_table.as_deref().and_then(ScheduleTable::parse)
    }
}

// ---------------------------------------------------------------------------
// Camera: light and sound
// ---------------------------------------------------------------------------

/// Status and infrared LEDs (reply to 208, per channel or device-level).
/// `state` is `auto`/`open`/`close`; `lightState` `open`/`close`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct LedState {
    #[serde(rename = "channelId", default, deserialize_with = "num")]
    pub channel_id: Option<u8>,
    #[serde(rename = "ledVersion", default, deserialize_with = "num")]
    pub led_version: Option<u32>,
    #[serde(rename = "IRLedBrightness", default, deserialize_with = "num")]
    pub ir_led_brightness: Option<u32>,
    pub state: Option<String>,
    #[serde(rename = "lightState")]
    pub light_state: Option<String>,
    #[serde(rename = "doorbellLightState")]
    pub doorbell_light_state: Option<String>,
}

/// The spotlight's automatic behaviour (reply to 289). The light-sensor
/// threshold and multi-brightness parts are not read.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct FloodlightTask {
    #[serde(default, deserialize_with = "num")]
    pub channel: Option<u8>,
    #[serde(rename = "alarmMode", default, deserialize_with = "num")]
    pub alarm_mode: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub duration: Option<u32>,
    #[serde(rename = "brightness_cur", default, deserialize_with = "num")]
    pub brightness_cur: Option<u32>,
    #[serde(rename = "brightness_min", default, deserialize_with = "num")]
    pub brightness_min: Option<u32>,
    #[serde(rename = "brightness_max", default, deserialize_with = "num")]
    pub brightness_max: Option<u32>,
    pub schedule: Option<HourSpan>,
    #[serde(rename = "lightAlarmSchedule")]
    pub light_alarm_schedule: Option<HourSpan>,
    #[serde(rename = "detectType")]
    pub detect_type: Option<String>,
}

/// The camera's speaker volume (reply to 264); the other fields are not read.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct AudioCfg {
    #[serde(rename = "channelId", default, deserialize_with = "num")]
    pub channel_id: Option<u8>,
    #[serde(default, deserialize_with = "num")]
    pub volume: Option<u32>,
}

/// Audio noise reduction (reply to 439).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct AiDenoise {
    #[serde(rename = "channelId", default, deserialize_with = "num")]
    pub channel_id: Option<u8>,
    #[serde(default, deserialize_with = "num")]
    pub enable: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub level: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct StreamAudio {
    #[serde(default, deserialize_with = "num")]
    pub audio: Option<u32>,
}

/// Only the main stream's audio flag of the encoder settings (reply to 56);
/// `encoding::Encoding` reads the rest.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct CompressionAudio {
    #[serde(rename = "channelId", default, deserialize_with = "num")]
    pub channel_id: Option<u8>,
    #[serde(rename = "mainStream")]
    pub main_stream: Option<StreamAudio>,
}

// ---------------------------------------------------------------------------
// Schedules
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ScheduleItem {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    #[serde(rename = "valueTable")]
    pub value_table: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct TypeScheduleList {
    #[serde(rename = "item", default)]
    pub items: Vec<ScheduleItem>,
}

/// A task with a per-type weekly schedule: recording (81), e-mail (217),
/// push (219), FTP (70), the camera's siren (232) and the hub's siren (306)
/// all share this shape.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ScheduledTask {
    #[serde(rename = "channelId", default, deserialize_with = "num")]
    pub channel_id: Option<u8>,
    #[serde(default, deserialize_with = "num")]
    pub enable: Option<u32>,
    #[serde(rename = "typeScheduleList")]
    pub type_schedule_list: Option<TypeScheduleList>,
}

impl ScheduledTask {
    /// Each type's name with its table (`None` for a table that is not 168
    /// characters of `0`/`1`), in the device's order.
    pub fn items(&self) -> Vec<(String, Option<ScheduleTable>)> {
        self.type_schedule_list
            .as_ref()
            .map(|l| {
                l.items
                    .iter()
                    .map(|i| {
                        (
                            i.kind.clone().unwrap_or_default(),
                            i.value_table.as_deref().and_then(ScheduleTable::parse),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct CycleList {
    #[serde(rename = "item", default, deserialize_with = "num_list")]
    pub items: Vec<i64>,
}

/// Recording parameters (reply to 54). `cycle` 1 is overwrite, 0 off, a
/// negative value a number of days to keep.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct RecordCfg {
    #[serde(rename = "channelId", default, deserialize_with = "num")]
    pub channel_id: Option<u8>,
    #[serde(default, deserialize_with = "num")]
    pub cycle: Option<i64>,
    #[serde(rename = "recordDelayTime", default, deserialize_with = "num")]
    pub record_delay_time: Option<u32>,
    #[serde(rename = "preRecordTime", default, deserialize_with = "num")]
    pub pre_record_time: Option<u32>,
    #[serde(rename = "packageTime", default, deserialize_with = "num")]
    pub package_time: Option<u32>,
    #[serde(rename = "cyclelist")]
    pub cycle_list: Option<CycleList>,
}

/// Push notification parameters (reply to 363).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct PushCfg {
    #[serde(default, deserialize_with = "num")]
    pub interval: Option<u32>,
    #[serde(rename = "richNotificationEnable", default, deserialize_with = "num")]
    pub rich_notification_enable: Option<u32>,
    #[serde(rename = "consentAgreement", default, deserialize_with = "num")]
    pub consent_agreement: Option<u32>,
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

/// The address block of the reply to 76.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct IpConfig {
    pub ip: Option<String>,
    pub mask: Option<String>,
    pub gateway: Option<String>,
    pub mac: Option<MacAddress>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct DnsConfig {
    pub dns1: Option<String>,
    pub dns2: Option<String>,
}

/// One of the service-port roots of the reply to 37; each carries one port
/// element and an `enable` flag.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ServicePort {
    #[serde(rename = "serverPort", default, deserialize_with = "num")]
    pub server_port: Option<u32>,
    #[serde(rename = "httpPort", default, deserialize_with = "num")]
    pub http_port: Option<u32>,
    #[serde(rename = "httpsPort", default, deserialize_with = "num")]
    pub https_port: Option<u32>,
    #[serde(rename = "rtspPort", default, deserialize_with = "num")]
    pub rtsp_port: Option<u32>,
    #[serde(rename = "rtmpPort", default, deserialize_with = "num")]
    pub rtmp_port: Option<u32>,
    #[serde(rename = "onvifPort", default, deserialize_with = "num")]
    pub onvif_port: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub enable: Option<u32>,
}

impl ServicePort {
    /// Whichever port element the root carries.
    pub fn port(&self) -> Option<u32> {
        self.server_port
            .or(self.http_port)
            .or(self.https_port)
            .or(self.rtsp_port)
            .or(self.rtmp_port)
            .or(self.onvif_port)
    }
}

/// DDNS (reply to 40): only the switch and the provider; the host name and
/// the account are never read.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Ddns {
    #[serde(default, deserialize_with = "num")]
    pub enable: Option<u32>,
    #[serde(rename = "ddnsType")]
    pub ddns_type: Option<String>,
}

/// Time synchronisation (reply to 38).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Ntp {
    #[serde(default, deserialize_with = "num")]
    pub enable: Option<u32>,
    pub server: Option<String>,
    #[serde(default, deserialize_with = "num")]
    pub port: Option<u32>,
    #[serde(rename = "synchronizeInterval", default, deserialize_with = "num")]
    pub synchronize_interval: Option<u32>,
}

/// One band of the hub's Wi-Fi for its cameras; the key is never read.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ApCfg {
    pub freq: Option<String>,
    #[serde(default, deserialize_with = "num")]
    pub channel: Option<u32>,
    #[serde(rename = "hideSSID", default, deserialize_with = "num")]
    pub hide_ssid: Option<u32>,
    pub ssid: Option<Ssid>,
    #[serde(rename = "countryCode")]
    pub country_code: Option<String>,
    #[serde(rename = "enableWifi", default, deserialize_with = "num")]
    pub enable_wifi: Option<u32>,
}

/// The hub's Wi-Fi for its cameras (reply to 481).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct KitApCfg {
    #[serde(rename = "apCfg", default)]
    pub bands: Vec<ApCfg>,
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// One disk of the reply to 102. Sizes are GiB plus a MiB remainder; newer
/// firmware adds the exact byte counts.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct HddInfo {
    #[serde(default, deserialize_with = "num")]
    pub number: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub capacity: Option<u32>,
    #[serde(rename = "capacityM", default, deserialize_with = "num")]
    pub capacity_m: Option<u32>,
    #[serde(rename = "remainSize", default, deserialize_with = "num")]
    pub remain_size: Option<u32>,
    #[serde(rename = "remainSizeM", default, deserialize_with = "num")]
    pub remain_size_m: Option<u32>,
    #[serde(rename = "capacityV2", default, deserialize_with = "num")]
    pub capacity_v2: Option<u64>,
    #[serde(rename = "remainSizeV2", default, deserialize_with = "num")]
    pub remain_size_v2: Option<u64>,
    #[serde(default, deserialize_with = "num")]
    pub format: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub mount: Option<u32>,
}

impl HddInfo {
    /// (total, free) in GiB: the byte counts when present, else the GiB
    /// plus MiB / 1024 pair.
    pub fn gib(&self) -> (f64, f64) {
        const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
        let total = match self.capacity_v2 {
            Some(bytes) => bytes as f64 / GIB,
            None => f64::from(self.capacity.unwrap_or(0)) + f64::from(self.capacity_m.unwrap_or(0)) / 1024.0,
        };
        let free = match self.remain_size_v2 {
            Some(bytes) => bytes as f64 / GIB,
            None => f64::from(self.remain_size.unwrap_or(0)) + f64::from(self.remain_size_m.unwrap_or(0)) / 1024.0,
        };
        (total, free)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct HddInfoList {
    #[serde(rename = "HddInfo", default)]
    pub disks: Vec<HddInfo>,
}

// ---------------------------------------------------------------------------
// System
// ---------------------------------------------------------------------------

/// Clock, time zone, display formats and the login lockout (reply to 104,
/// first root). `timeZone` is in seconds west of UTC.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct SystemGeneral {
    #[serde(rename = "timeZone", default, deserialize_with = "num")]
    pub time_zone: Option<i64>,
    #[serde(rename = "osdFormat")]
    pub osd_format: Option<String>,
    #[serde(default, deserialize_with = "num")]
    pub year: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub month: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub day: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub hour: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub minute: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub second: Option<u32>,
    #[serde(rename = "timeFormat", default, deserialize_with = "num")]
    pub time_format: Option<u32>,
    pub language: Option<String>,
    #[serde(rename = "deviceName")]
    pub device_name: Option<String>,
    #[serde(rename = "loginLock", default, deserialize_with = "num")]
    pub login_lock: Option<u32>,
    #[serde(rename = "lockTime", default, deserialize_with = "num")]
    pub lock_time: Option<u32>,
    #[serde(rename = "allowedTimes", default, deserialize_with = "num")]
    pub allowed_times: Option<u32>,
    #[serde(rename = "isDst", default, deserialize_with = "num")]
    pub is_dst: Option<u32>,
}

/// The video standard (reply to 104, second root).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Norm {
    pub norm: Option<String>,
}

/// Daylight saving time (reply to 106). Week index 5 means the last week.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Dst {
    #[serde(default, deserialize_with = "num")]
    pub enable: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub offset: Option<u32>,
    #[serde(rename = "startMonth", default, deserialize_with = "num")]
    pub start_month: Option<u32>,
    #[serde(rename = "startWeekIndex", default, deserialize_with = "num")]
    pub start_week_index: Option<u32>,
    #[serde(rename = "startWeekday")]
    pub start_weekday: Option<String>,
    #[serde(rename = "startHour", default, deserialize_with = "num")]
    pub start_hour: Option<u32>,
    #[serde(rename = "startMinute", default, deserialize_with = "num")]
    pub start_minute: Option<u32>,
    #[serde(rename = "startSecond", default, deserialize_with = "num")]
    pub start_second: Option<u32>,
    #[serde(rename = "endMonth", default, deserialize_with = "num")]
    pub end_month: Option<u32>,
    #[serde(rename = "endWeekIndex", default, deserialize_with = "num")]
    pub end_week_index: Option<u32>,
    #[serde(rename = "endWeekday")]
    pub end_weekday: Option<String>,
    #[serde(rename = "endHour", default, deserialize_with = "num")]
    pub end_hour: Option<u32>,
    #[serde(rename = "endMinute", default, deserialize_with = "num")]
    pub end_minute: Option<u32>,
    #[serde(rename = "endSecond", default, deserialize_with = "num")]
    pub end_second: Option<u32>,
}

/// The hub's own sounds (reply to 504); the per-channel music list is not
/// read.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct DeviceAudioCfg {
    #[serde(rename = "alarmVolume", default, deserialize_with = "num")]
    pub alarm_volume: Option<u32>,
    #[serde(rename = "cuesVolume", default, deserialize_with = "num")]
    pub cues_volume: Option<u32>,
    #[serde(rename = "alarmPauseTime", default, deserialize_with = "num")]
    pub alarm_pause_time: Option<u32>,
}

/// A device-level switch with a per-channel bit mask (replies to 308, 310,
/// 314 and 316): bit `n` of `enable` is channel `n`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ChannelSwitch {
    #[serde(default, deserialize_with = "num")]
    pub enable: Option<u64>,
    #[serde(rename = "mainSwitch", default, deserialize_with = "num")]
    pub main_switch: Option<u32>,
}

impl ChannelSwitch {
    pub fn channel_on(&self, channel: u8) -> bool {
        channel < 64 && (self.enable.unwrap_or(0) >> channel) & 1 == 1
    }
}

/// One account of the reply to 58; its id and password flag are never read.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct UserEntry {
    #[serde(rename = "userName")]
    pub user_name: Option<AccountName>,
    /// 1 is an administrator, 0 a user.
    #[serde(rename = "userLevel", default, deserialize_with = "num")]
    pub user_level: Option<u32>,
    #[serde(rename = "loginState", default, deserialize_with = "num")]
    pub login_state: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct UserList {
    #[serde(rename = "User", default)]
    pub users: Vec<UserEntry>,
}

/// One shared-access user of the reply to 511; its id, note and phone model
/// are never read.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct AccessUser {
    #[serde(rename = "userLevel", default, deserialize_with = "num")]
    pub user_level: Option<u32>,
    #[serde(rename = "validHours", default, deserialize_with = "num")]
    pub valid_hours: Option<i64>,
    #[serde(rename = "loginState", default, deserialize_with = "num")]
    pub login_state: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub ability: Option<u32>,
    #[serde(rename = "channelAbility", default, deserialize_with = "num")]
    pub channel_ability: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct AccessUserList {
    #[serde(rename = "maxAccessUserNum", default, deserialize_with = "num")]
    pub max_access_user_num: Option<u32>,
    #[serde(rename = "availableAbility", default, deserialize_with = "num")]
    pub available_ability: Option<u32>,
    #[serde(rename = "User", default)]
    pub users: Vec<AccessUser>,
}

/// The scheduled reboot (reply to 101).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct AutoReboot {
    #[serde(default, deserialize_with = "num")]
    pub enable: Option<u32>,
    #[serde(rename = "weekDay")]
    pub week_day: Option<String>,
    #[serde(default, deserialize_with = "num")]
    pub hour: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub minute: Option<u32>,
    #[serde(default, deserialize_with = "num")]
    pub second: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n";

    fn body(inner: &str) -> SettingsBody {
        SettingsBody::from_bytes(format!("{HEAD}{inner}</body>\n").as_bytes()).expect("fixture parses")
    }

    const ONES: &str = "111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111111";
    const ZEROS: &str = "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

    // Fixtures shaped like the devices' replies, with the identifying values
    // replaced by placeholders.

    const VERSION_INFO_CAMERA: &str = "<VersionInfo version=\"1.1\">\n<channelId>2</channelId>\n<name>Camera1</name>\n<type>E1 Outdoor PoE</type>\n<serialNumber>SERIAL0000000000</serialNumber>\n<buildDay>build 2510172100</buildDay>\n<hardwareVersion>IPC_560SD88MP</hardwareVersion>\n<cfgVersion>v3.1.0.0</cfgVersion>\n<firmwareVersion>v3.1.0.5223_2510172100</firmwareVersion>\n<detail>IPC_560SD88MPS16A1T60E1W0110000</detail>\n<IEClient>IEClient</IEClient>\n<cc3200Version></cc3200Version>\n<spVersion></spVersion>\n<pakSuffix>pak</pakSuffix>\n<helpVersion>blackPointsLevel=0</helpVersion>\n</VersionInfo>\n";

    const VERSION_INFO_DEVICE: &str = "<VersionInfo version=\"1.1\">\n<name>Hub</name>\n<type>Reolink Home Hub Pro</type>\n<serialNumber>SERIAL0000000000</serialNumber>\n<buildDay>build 26090343</buildDay>\n<hardwareVersion>BASE_WENNT3NA5</hardwareVersion>\n<cfgVersion>v3.0.0.0</cfgVersion>\n<firmwareVersion>v3.3.0.605_26090343</firmwareVersion>\n<detail>181042EH01F</detail>\n<aiVersion>1.2.3</aiVersion>\n<IEClient>IEClient</IEClient>\n<pakSuffix>pak</pakSuffix>\n<helpVersion>blackPointsLevel=0</helpVersion>\n</VersionInfo>\n";

    const VIDEO_INPUT: &str = "<VideoInput version=\"1.1\">\n<channelId>0</channelId>\n<bright>128</bright>\n<contrast>129</contrast>\n<saturation>130</saturation>\n<hue>131</hue>\n<sharpen>132</sharpen>\n<corridorMode>close</corridorMode>\n<corridorAbility>0</corridorAbility>\n</VideoInput>\n<InputAdvanceCfg version=\"1.1\">\n<channelId>0</channelId>\n<digitalChannel>1</digitalChannel>\n<separateCfg>\n<encType>0</encType>\n<constantFrameRate>0</constantFrameRate>\n</separateCfg>\n<PowerLineFrequency>\n<mode>50hz</mode>\n<enable>0</enable>\n</PowerLineFrequency>\n<Exposure>\n<mode>auto</mode>\n<Gainctl>\n<defMin>1</defMin>\n<defMax>100</defMax>\n<curMin>1</curMin>\n<curMax>62</curMax>\n</Gainctl>\n<shutterLevel>1/30</shutterLevel>\n<gainLevel>50</gainLevel>\n</Exposure>\n<Scene>\n<mode>auto</mode>\n<modeList>auto, manual</modeList>\n<Redgain>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</Redgain>\n</Scene>\n<DayNight>\n<mode>auto</mode>\n<IrcutMode>ir</IrcutMode>\n<Threshold>medium</Threshold>\n</DayNight>\n<BLC>\n<enable>0</enable>\n<mode>backLight</mode>\n<backlight>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</backlight>\n</BLC>\n<bdDayColor>\n<mode>auto</mode>\n<bright>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</bright>\n</bdDayColor>\n<bdNight>\n<mode>manual</mode>\n</bdNight>\n<bdNightColor>\n<mode>auto</mode>\n</bdNightColor>\n<mirror>0</mirror>\n<flip>1</flip>\n<Iris>\n<enable>0</enable>\n<state>success</state>\n<focusAutoiris>0</focusAutoiris>\n</Iris>\n<nr3d>\n<value>high</value>\n<enable>1</enable>\n</nr3d>\n<firstFrameStrategy>0</firstFrameStrategy>\n<constantFrameRate>2</constantFrameRate>\n<constantFrameRateAbility>7</constantFrameRateAbility>\n<encType>1</encType>\n<encTypeAbility>0</encTypeAbility>\n<binning_mode>1</binning_mode>\n</InputAdvanceCfg>\n";

    const DAY_NIGHT_THRESHOLD: &str = "<DayNightThreshold version=\"1.1\">\n<channelId>0</channelId>\n<threshold>custom</threshold>\n<stat>night</stat>\n<thresholdval>\n<min>0</min>\n<max>100</max>\n<cur>50</cur>\n</thresholdval>\n</DayNightThreshold>\n";

    const OSD: &str = "<OsdDatetime version=\"1.1\">\n<channelId>0</channelId>\n<enable>1</enable>\n<topLeftX>65537</topLeftX>\n<topLeftY>1</topLeftY>\n<width>0</width>\n<height>0</height>\n<language>Chinese</language>\n</OsdDatetime>\n<OsdChannelName version=\"1.1\">\n<channelId>0</channelId>\n<name>Camera2</name>\n<enable>1</enable>\n<topLeftX>65536</topLeftX>\n<topLeftY>65536</topLeftY>\n<enWatermark>0</enWatermark>\n<enBgcolor>1</enBgcolor>\n</OsdChannelName>\n";

    const AI_CFG: &str = "<AiCfg version=\"1.1\">\n<channelId>1</channelId>\n<smartTrack>1</smartTrack>\n<smartTrackMode>1</smartTrackMode>\n<smartTrackModeAbility>1</smartTrackModeAbility>\n<detectType>people,vehicle,dog_cat</detectType>\n<smartTrackType>people</smartTrackType>\n<smartTrackPt>1</smartTrackPt>\n<smartTrackObjectStopDelay>20</smartTrackObjectStopDelay>\n<smartTrackObjectDisappearDelay>10</smartTrackObjectDisappearDelay>\n<cryDetectLevel>3</cryDetectLevel>\n</AiCfg>\n";

    const MOTION: &str = "<MD version=\"1.1\">\n<channelId>0</channelId>\n<separateCfg>\n<nonDetectArea>0</nonDetectArea>\n</separateCfg>\n<enable>1</enable>\n<usepir>0</usepir>\n<width>141</width>\n<height>78</height>\n<scope>\n<columns>141</columns>\n<rows>78</rows>\n<valueTable>////A==</valueTable>\n</scope>\n<sensitivityInfoList>\n<sensitivityInfo>\n<id>0</id>\n<sensitivity>10</sensitivity>\n<beginHour>0</beginHour>\n<beginMinute>0</beginMinute>\n<endHour>6</endHour>\n<endMinute>0</endMinute>\n</sensitivityInfo>\n<sensitivityInfo>\n<id>1</id>\n<sensitivity>20</sensitivity>\n<beginHour>6</beginHour>\n<beginMinute>0</beginMinute>\n<endHour>12</endHour>\n<endMinute>0</endMinute>\n</sensitivityInfo>\n<sensitivityInfo>\n<id>2</id>\n<sensitivity>10</sensitivity>\n<beginHour>12</beginHour>\n<beginMinute>0</beginMinute>\n<endHour>18</endHour>\n<endMinute>0</endMinute>\n</sensitivityInfo>\n<sensitivityInfo>\n<id>3</id>\n<sensitivity>10</sensitivity>\n<beginHour>18</beginHour>\n<beginMinute>0</beginMinute>\n<endHour>23</endHour>\n<endMinute>59</endMinute>\n</sensitivityInfo>\n</sensitivityInfoList>\n<sensInfoNew>\n<sensitivityDefault>10</sensitivityDefault>\n<sensInfoList />\n</sensInfoNew>\n<timeBlockList />\n</MD>\n";

    const AI_DETECT: &str = "<AiDetectCfg version=\"1.1\">\n<chn>0</chn>\n<separateCfg>\n<targetSize>0</targetSize>\n<nonDetectArea>0</nonDetectArea>\n</separateCfg>\n<type>vehicle</type>\n<sensitivity>60</sensitivity>\n<stayTime>5</stayTime>\n<minTargetHeight>1</minTargetHeight>\n<minTargetWidth>2</minTargetWidth>\n<maxTargetHeight>3</maxTargetHeight>\n<maxTargetWidth>4</maxTargetWidth>\n<width>141</width>\n<height>78</height>\n<area>////A==</area>\n</AiDetectCfg>\n";

    const TRACK_LIMIT: &str = "<trackLimit version=\"1.1\">\n<channelId>11</channelId>\n<leftLimit>3169</leftLimit>\n<leftImageName>left_picture</leftImageName>\n<rightLimit>669</rightLimit>\n<rightImageName>right_picture</rightImageName>\n</trackLimit>\n";

    const LED_STATE: &str = "<LedState version=\"1.1\">\n<channelId>0</channelId>\n<ledVersion>0</ledVersion>\n<IRLedBrightness>0</IRLedBrightness>\n<state>auto</state>\n<lightState>open</lightState>\n<doorbellLightState>close</doorbellLightState>\n</LedState>\n";

    const FLOODLIGHT: &str = "<FloodlightTask version=\"1.1\">\n<channel>0</channel>\n<alarmMode>1</alarmMode>\n<preview_auto>0</preview_auto>\n<duration>180</duration>\n<brightness_cur>85</brightness_cur>\n<brightness_max>100</brightness_max>\n<brightness_min>1</brightness_min>\n<schedule>\n<startHour>18</startHour>\n<startMin>0</startMin>\n<endHour>6</endHour>\n<endMin>0</endMin>\n</schedule>\n<lightAlarmSchedule>\n<startHour>18</startHour>\n<startMin>0</startMin>\n<endHour>7</endHour>\n<endMin>0</endMin>\n</lightAlarmSchedule>\n<lightSensThreshold>\n<min>0</min>\n<max>0</max>\n<lightCur>0</lightCur>\n<darkCur>0</darkCur>\n<lightDef>0</lightDef>\n<darkDef>0</darkDef>\n</lightSensThreshold>\n<FloodlightScheduleList>\n<maxNum>0</maxNum>\n</FloodlightScheduleList>\n<nightLongViewMultiBrightness>\n<enable>0</enable>\n<alarmBrightness>\n<min>0</min>\n<max>0</max>\n<cur>0</cur>\n<def>0</def>\n</alarmBrightness>\n</nightLongViewMultiBrightness>\n<detectType>people,vehicle,dog_cat</detectType>\n</FloodlightTask>\n";

    const AUDIO_CFG: &str = "<audioCfg version=\"1.1\">\n<channelId>0</channelId>\n<timeout>0</timeout>\n<audioSelect>0</audioSelect>\n<volume>92</volume>\n<preAlarm>0</preAlarm>\n<audioListId>0</audioListId>\n<visitorLoudspeaker>0</visitorLoudspeaker>\n</audioCfg>\n";

    const AI_DENOISE: &str = "<aiDenoise Version=\"1.1\">\n<channelId>2</channelId>\n<enable>1</enable>\n<level>4</level>\n</aiDenoise>\n";

    const COMPRESSION: &str = "<Compression version=\"1.1\">\n<channelId>0</channelId>\n<separateCfg>\n<encodeCfg>0</encodeCfg>\n</separateCfg>\n<isNoTranslateFrame>1</isNoTranslateFrame>\n<mainStream>\n<audio>1</audio>\n<resolutionName>3840*2160</resolutionName>\n<width>3840</width>\n<height>2160</height>\n<encoderType>vbr</encoderType>\n<frame>25</frame>\n<bitRate>8192</bitRate>\n<encoderProfile>high</encoderProfile>\n<videoEncType>1</videoEncType>\n<gop>\n<cur>1</cur>\n<max>2</max>\n<min>1</min>\n</gop>\n</mainStream>\n<subStream>\n<audio>0</audio>\n<resolutionName>640*360</resolutionName>\n<width>640</width>\n<height>360</height>\n<encoderType>vbr</encoderType>\n<frame>10</frame>\n<bitRate>256</bitRate>\n<encoderProfile>high</encoderProfile>\n<videoEncType>0</videoEncType>\n</subStream>\n</Compression>\n";

    const RECORD_CFG: &str = "<RecordCfg version=\"1.1\">\n<channelId>0</channelId>\n<cycle>-7</cycle>\n<recordDelayTime>60</recordDelayTime>\n<preRecordTime>10</preRecordTime>\n<packageTime>30</packageTime>\n<batteryRecordTime>8</batteryRecordTime>\n<cyclelist>\n<item>0</item>\n<item>1</item>\n<item>-1</item>\n<item>-2</item>\n<item>-3</item>\n<item>-7</item>\n<item>-14</item>\n<item>-30</item>\n</cyclelist>\n</RecordCfg>\n";

    const PUSH_CFG: &str = "<PushCfg version=\"1.1\">\n<interval>30</interval>\n<consentAgreement>1</consentAgreement>\n</PushCfg>\n";

    const NETWORK: &str = "<Dhcp version=\"1.1\">\n<enable>1</enable>\n</Dhcp>\n<AutoDns version=\"1.1\">\n<enable>0</enable>\n</AutoDns>\n<Ip version=\"1.1\">\n<ip>192.0.2.5</ip>\n<mask>255.255.255.0</mask>\n<mac>00:00:00:00:00:00</mac>\n<gateway>192.0.2.1</gateway>\n</Ip>\n<Dns version=\"1.1\">\n<dns1>192.0.2.1</dns1>\n<dns2></dns2>\n</Dns>\n";

    const HDD_LIST: &str = "<HddInfoList version=\"1.1\">\n<HddInfo>\n<number>0</number>\n<capacity>1803</capacity>\n<capacityM>422</capacityM>\n<format>1</format>\n<mount>1</mount>\n<remainSize>71</remainSize>\n<remainSizeM>897</remainSizeM>\n<capacityV2>1936399007744</capacityV2>\n<remainSizeV2>77176766464</remainSizeV2>\n<type>0</type>\n<index>0</index>\n</HddInfo>\n<HddInfo>\n<number>2</number>\n<capacity>1803</capacity>\n<capacityM>422</capacityM>\n<format>0</format>\n<mount>0</mount>\n<remainSize>71</remainSize>\n<remainSizeM>897</remainSizeM>\n</HddInfo>\n</HddInfoList>\n";

    const REC_ENC: &str = "<RecEncCfg version=\"1.1\">\n<enable>0</enable>\n<pwdValid>PLACEHOLDER</pwdValid>\n</RecEncCfg>\n";

    const PORTS: &str = "<ServerPort version=\"1.1\">\n<serverPort>9000</serverPort>\n<enable>0</enable>\n</ServerPort>\n<HttpPort version=\"1.1\">\n<httpPort>80</httpPort>\n<enable>1</enable>\n</HttpPort>\n<HttpsPort version=\"1.1\">\n<httpsPort>443</httpsPort>\n<enable>0</enable>\n</HttpsPort>\n<RtspPort version=\"1.1\">\n<rtspPort>554</rtspPort>\n<enable>0</enable>\n</RtspPort>\n<RtmpPort version=\"1.1\">\n<rtmpPort>1935</rtmpPort>\n<enable>0</enable>\n</RtmpPort>\n<OnvifPort version=\"1.1\">\n<onvifPort>8000</onvifPort>\n<enable>0</enable>\n</OnvifPort>\n";

    const DDNS: &str = "<Ddns version=\"1.1\">\n<enable>0</enable>\n<ddnsType>no-ip</ddnsType>\n<ddnsName>host.example</ddnsName>\n<userName>someone</userName>\n<password>PLACEHOLDER</password>\n</Ddns>\n";

    const NTP: &str = "<Ntp version=\"1.1\">\n<enable>1</enable>\n<server>ntp.example</server>\n<synchronizeInterval>1440</synchronizeInterval>\n<port>123</port>\n</Ntp>\n";

    const CERTIFICATE: &str = "<certificateInfo version=\"1.1\">\n<enable>0</enable>\n<certName>cert.pem</certName>\n<keyName>key.pem</keyName>\n</certificateInfo>\n";

    const KIT_AP: &str = "<kitApCfg version=\"1.1\">\n<apCfg>\n<freq>2.4G</freq>\n<channel>0</channel>\n<hideSSID>0</hideSSID>\n<ssid>NetworkName</ssid>\n<key>PLACEHOLDER</key>\n<countryCode>FR</countryCode>\n<enableWifi>0</enableWifi>\n</apCfg>\n<apCfg>\n<freq>5G</freq>\n<enableDfs>1</enableDfs>\n<channel>36</channel>\n<hideSSID>1</hideSSID>\n<ssid>NetworkName</ssid>\n<key>PLACEHOLDER</key>\n<countryCode>FR</countryCode>\n<enableWifi>1</enableWifi>\n</apCfg>\n<gateway></gateway>\n<meshFreq>1</meshFreq>\n<meshEnable>1</meshEnable>\n<wifiAbility>3</wifiAbility>\n<wifi2D4GList>1,2,3,4,5,6,7,8,9,10,11,12,13</wifi2D4GList>\n<wifi5GList>36,40,44,48</wifi5GList>\n</kitApCfg>\n";

    const SYSTEM_GENERAL: &str = "<SystemGeneral version=\"1.1\">\n<timeZone>-7200</timeZone>\n<osdFormat>YMD</osdFormat>\n<year>2026</year>\n<month>10</month>\n<day>18</day>\n<hour>17</hour>\n<minute>24</minute>\n<second>40</second>\n<deviceId>0</deviceId>\n<timeFormat>0</timeFormat>\n<language>Chinese</language>\n<deviceName>Hub</deviceName>\n<loginLock>1</loginLock>\n<lockTime>300</lockTime>\n<allowedTimes>10</allowedTimes>\n<isDst>1</isDst>\n</SystemGeneral>\n<Norm version=\"1.1\">\n<norm>NTSC</norm>\n</Norm>\n";

    const DST: &str = "<Dst version=\"1.1\">\n<enable>1</enable>\n<offset>1</offset>\n<startMonth>3</startMonth>\n<startWeekIndex>5</startWeekIndex>\n<startWeekday>Sunday</startWeekday>\n<startHour>3</startHour>\n<startMinute>0</startMinute>\n<startSecond>0</startSecond>\n<endMonth>10</endMonth>\n<endWeekIndex>4</endWeekIndex>\n<endWeekday>Sunday</endWeekday>\n<endHour>4</endHour>\n<endMinute>0</endMinute>\n<endSecond>0</endSecond>\n<version>1</version>\n</Dst>\n";

    const DEVICE_AUDIO: &str = "<DeiveAudioCfg version=\"1.1\">\n<alarmVolume>0</alarmVolume>\n<cuesVolume>5</cuesVolume>\n<alarmPauseTime>20</alarmPauseTime>\n<musicIdList>\n<musicId>\n<channel>0</channel>\n<id>0</id>\n<alarmMusicId>-1</alarmMusicId>\n</musicId>\n<musicId>\n<channel>1</channel>\n<id>0</id>\n<alarmMusicId>-1</alarmMusicId>\n</musicId>\n</musicIdList>\n</DeiveAudioCfg>\n";

    const RECORD_ENABLE: &str = "<RecordEnable version=\"1.1\">\n<enable>68719476735</enable>\n<mainSwitch>1</mainSwitch>\n</RecordEnable>\n";

    const USERS: &str = "<AbilitySuppport version=\"1.1\">\n<userName>someone</userName>\n<system>1</system>\n<streaming>1</streaming>\n<record>1</record>\n<network>1</network>\n<PTZ>1</PTZ>\n<IO>0</IO>\n<alarm>1</alarm>\n<image>1</image>\n<video>1</video>\n<audio>1</audio>\n<security>1</security>\n<replay>1</replay>\n<disk>1</disk>\n</AbilitySuppport>\n<UserList version=\"1.1\">\n<User>\n<userId>0</userId>\n<userName>someone</userName>\n<userLevel>1</userLevel>\n<validpassword>PLACEHOLDER</validpassword>\n<loginState>1</loginState>\n<userSetState>none</userSetState>\n<validHours>-1</validHours>\n<channelAbility>16777215</channelAbility>\n</User>\n<User>\n<userId>1</userId>\n<userName>guest</userName>\n<userLevel>0</userLevel>\n<validpassword>PLACEHOLDER</validpassword>\n<loginState>0</loginState>\n</User>\n</UserList>\n";

    const ACCESS_USERS: &str = "<accessUserList version=\"1.1\">\n<maxAccessUserNum>10</maxAccessUserNum>\n<availableAbility>63</availableAbility>\n<User>\n<userId>PLACEHOLDERPLACEHOLDERPLACEHOLDER</userId>\n<notes>someone@example.invalid</notes>\n<phoneModel>phone</phoneModel>\n<userLevel>0</userLevel>\n<validHours>25279</validHours>\n<loginState>0</loginState>\n<ability>39</ability>\n<channelAbility>7</channelAbility>\n</User>\n</accessUserList>\n";

    const AUTO_REBOOT: &str = "<AutoReboot version=\"1.1\">\n<enable>1</enable>\n<weekDay>Sunday</weekDay>\n<hour>11</hour>\n<minute>0</minute>\n<second>0</second>\n</AutoReboot>\n";

    fn task(root: &str, items: &[(&str, &str)]) -> String {
        let mut xml = format!("<{root} version=\"1.1\">\n<channelId>1</channelId>\n<enable>1</enable>\n<typeScheduleList>\n");
        for (kind, table) in items {
            xml.push_str(&format!("<item>\n<type>{kind}</type>\n<valueTable>{table}</valueTable>\n</item>\n"));
        }
        xml.push_str(&format!("</typeScheduleList>\n</{root}>\n"));
        xml
    }

    #[test]
    fn version_info_of_a_camera_and_of_a_device() {
        let v = body(VERSION_INFO_CAMERA).version_info.unwrap();
        assert_eq!(v.channel_id, Some(2));
        assert_eq!(v.name.as_deref(), Some("Camera1"));
        assert_eq!(v.model.as_deref(), Some("E1 Outdoor PoE"));
        assert_eq!(v.build_day.as_deref(), Some("build 2510172100"));
        assert_eq!(v.hardware_version.as_deref(), Some("IPC_560SD88MP"));
        assert_eq!(v.cfg_version.as_deref(), Some("v3.1.0.0"));
        assert_eq!(v.firmware_version.as_deref(), Some("v3.1.0.5223_2510172100"));
        assert_eq!(v.detail.as_deref(), Some("IPC_560SD88MPS16A1T60E1W0110000"));
        assert_eq!(v.ai_version, None);
        let d = body(VERSION_INFO_DEVICE).version_info.unwrap();
        assert_eq!(d.channel_id, None);
        assert_eq!(d.ai_version.as_deref(), Some("1.2.3"));
        assert!(!format!("{d:?}").contains("SERIAL"), "the serial number is never parsed");
    }

    #[test]
    fn image_settings() {
        let b = body(VIDEO_INPUT);
        let v = b.video_input.unwrap();
        assert_eq!((v.bright, v.contrast, v.saturation, v.hue, v.sharpen), (Some(128), Some(129), Some(130), Some(131), Some(132)));
        assert_eq!(v.corridor_mode.as_deref(), Some("close"));
        assert_eq!(v.corridor_ability, Some(0));
        let a = b.input_advance.unwrap();
        assert_eq!((a.flip, a.mirror), (Some(1), Some(0)));
        let plf = a.power_line_frequency.unwrap();
        assert_eq!((plf.mode.as_deref(), plf.enable), (Some("50hz"), Some(0)));
        let dn = a.day_night.unwrap();
        assert_eq!((dn.mode.as_deref(), dn.ircut_mode.as_deref(), dn.threshold.as_deref()), (Some("auto"), Some("ir"), Some("medium")));
        assert_eq!(a.bd_day_color.unwrap().mode.as_deref(), Some("auto"));
        assert_eq!(a.bd_night.unwrap().mode.as_deref(), Some("manual"));
        assert_eq!(a.bd_night_color.unwrap().mode.as_deref(), Some("auto"));
        assert_eq!(a.blc.unwrap().enable, Some(0));
        let nr = a.nr3d.unwrap();
        assert_eq!((nr.value.as_deref(), nr.enable), (Some("high"), Some(1)));
        // The top-level values, not the copies inside `separateCfg`.
        assert_eq!((a.constant_frame_rate, a.constant_frame_rate_ability), (Some(2), Some(7)));
        assert_eq!((a.enc_type, a.enc_type_ability, a.binning_mode), (Some(1), Some(0), Some(1)));
        let t = body(DAY_NIGHT_THRESHOLD).day_night_threshold.unwrap();
        assert_eq!((t.threshold.as_deref(), t.stat.as_deref()), (Some("custom"), Some("night")));
        assert_eq!(t.threshold_value, Some(MinMaxCur { min: Some(0), max: Some(100), cur: Some(50) }));
    }

    #[test]
    fn display_settings() {
        let b = body(OSD);
        let dt = b.osd_datetime.unwrap();
        assert_eq!((dt.enable, dt.top_left_x, dt.top_left_y, dt.language.as_deref()), (Some(1), Some(65537), Some(1), Some("Chinese")));
        let n = b.osd_channel_name.unwrap();
        assert_eq!(n.name.as_deref(), Some("Camera2"));
        assert_eq!((n.enable, n.top_left_x, n.top_left_y, n.watermark, n.background), (Some(1), Some(65536), Some(65536), Some(0), Some(1)));
    }

    #[test]
    fn detection_settings() {
        let ai = body(AI_CFG).ai_cfg.unwrap();
        assert_eq!((ai.smart_track, ai.smart_track_mode_ability), (Some(1), Some(1)));
        assert_eq!(ai.detect_type.as_deref(), Some("people,vehicle,dog_cat"));
        assert_eq!(ai.smart_track_type.as_deref(), Some("people"));
        assert_eq!((ai.smart_track_object_stop_delay, ai.smart_track_object_disappear_delay), (Some(20), Some(10)));
        let md = body(MOTION).motion.unwrap();
        assert_eq!((md.enable, md.width, md.height), (Some(1), Some(141), Some(78)));
        assert_eq!(md.periods().len(), 4);
        assert_eq!(md.periods()[1].sensitivity, Some(20));
        assert_eq!((md.periods()[3].end_hour, md.periods()[3].end_minute), (Some(23), Some(59)));
        assert_eq!(md.sens_info_new.unwrap().sensitivity_default, Some(10));
        let d = body(AI_DETECT).ai_detect.unwrap();
        assert_eq!(d.kind.as_deref(), Some("vehicle"));
        assert_eq!((d.sensitivity, d.stay_time), (Some(60), Some(5)));
        assert_eq!((d.min_target_width, d.min_target_height, d.max_target_width, d.max_target_height), (Some(2), Some(1), Some(4), Some(3)));
        let l = body(TRACK_LIMIT).track_limit.unwrap();
        assert_eq!((l.channel_id, l.left_limit, l.right_limit), (Some(11), Some(3169), Some(669)));
        let s = body(&format!("<trackSchedule version=\"1.1\">\n<channelId>11</channelId>\n<timeTable>{ONES}</timeTable>\n</trackSchedule>\n"))
            .track_schedule
            .unwrap();
        assert!(s.table().unwrap().all());
    }

    #[test]
    fn light_and_sound_settings() {
        let led = body(LED_STATE).led_state.unwrap();
        assert_eq!((led.state.as_deref(), led.light_state.as_deref(), led.ir_led_brightness), (Some("auto"), Some("open"), Some(0)));
        assert_eq!(led.doorbell_light_state.as_deref(), Some("close"));
        let f = body(FLOODLIGHT).floodlight_task.unwrap();
        assert_eq!((f.alarm_mode, f.duration), (Some(1), Some(180)));
        assert_eq!((f.brightness_cur, f.brightness_min, f.brightness_max), (Some(85), Some(1), Some(100)));
        let s = f.schedule.unwrap();
        assert_eq!((s.start_hour, s.start_min, s.end_hour, s.end_min), (Some(18), Some(0), Some(6), Some(0)));
        assert_eq!(f.light_alarm_schedule.unwrap().end_hour, Some(7));
        assert_eq!(f.detect_type.as_deref(), Some("people,vehicle,dog_cat"));
        assert_eq!(body(AUDIO_CFG).audio_cfg.unwrap().volume, Some(92));
        let d = body(AI_DENOISE).ai_denoise.unwrap();
        assert_eq!((d.channel_id, d.enable, d.level), (Some(2), Some(1), Some(4)));
        let c = body(COMPRESSION).compression.unwrap();
        assert_eq!(c.main_stream.unwrap().audio, Some(1));
    }

    #[test]
    fn scheduled_tasks_of_every_root() {
        let half = format!("{}{}", &ONES[..84], &ZEROS[..84]);
        for (root, pick) in [
            ("Record", (|b: &SettingsBody| b.record.clone()) as fn(&SettingsBody) -> Option<ScheduledTask>),
            ("EmailTask", |b| b.email_task.clone()),
            ("PushTask", |b| b.push_task.clone()),
            ("FtpTask", |b| b.ftp_task.clone()),
            ("AudioTask", |b| b.audio_task.clone()),
            ("BuzzerTask", |b| b.buzzer_task.clone()),
        ] {
            let xml = task(root, &[("MD", ONES), ("none", ZEROS), ("people", &half), ("vehicle", "bad")]);
            let t = pick(&body(&xml)).unwrap_or_else(|| panic!("{root} parses"));
            assert_eq!((t.channel_id, t.enable), (Some(1), Some(1)));
            let items = t.items();
            assert_eq!(items.len(), 4);
            assert_eq!(items[0].0, "MD");
            assert!(items[0].1.unwrap().all());
            assert!(!items[1].1.unwrap().any());
            let people = items[2].1.unwrap();
            assert!(people.on(0, 0) && people.on(3, 11) && !people.on(3, 12) && !people.on(6, 23));
            assert_eq!(items[3].1, None, "a table that is not 168 characters of 0/1");
        }
        let r = body(RECORD_CFG).record_cfg.unwrap();
        assert_eq!((r.cycle, r.record_delay_time, r.pre_record_time, r.package_time), (Some(-7), Some(60), Some(10), Some(30)));
        assert_eq!(r.cycle_list.unwrap().items, vec![0, 1, -1, -2, -3, -7, -14, -30]);
        // An odd item in the list leaves the rest of the reply readable.
        let r = body("<RecordCfg version=\"1.1\">\n<cycle>1</cycle>\n<preRecordTime>10</preRecordTime>\n<cyclelist>\n<item>0</item>\n<item></item>\n<item>x</item>\n<item>-7</item>\n</cyclelist>\n</RecordCfg>\n")
            .record_cfg
            .unwrap();
        assert_eq!((r.cycle, r.pre_record_time), (Some(1), Some(10)));
        assert_eq!(r.cycle_list.unwrap().items, vec![0, -7]);
        let p = body(PUSH_CFG).push_cfg.unwrap();
        assert_eq!((p.interval, p.consent_agreement, p.rich_notification_enable), (Some(30), Some(1), None));
    }

    #[test]
    fn network_settings() {
        let b = body(NETWORK);
        assert_eq!(b.dhcp.unwrap().enable, Some(1));
        assert_eq!(b.auto_dns.unwrap().enable, Some(0));
        let ip = b.ip.unwrap();
        assert_eq!((ip.ip.as_deref(), ip.mask.as_deref(), ip.gateway.as_deref()), (Some("192.0.2.5"), Some("255.255.255.0"), Some("192.0.2.1")));
        assert_eq!(ip.mac.as_ref().map(MacAddress::as_str), Some("00:00:00:00:00:00"));
        assert!(!format!("{ip:?}").contains("00:00:00"), "the MAC never reaches a log");
        let dns = b.dns.unwrap();
        assert_eq!(dns.dns1.as_deref(), Some("192.0.2.1"));
        assert!(dns.dns2.as_deref().unwrap_or("").is_empty());
        let p = body(PORTS);
        assert_eq!(p.server_port.as_ref().unwrap().port(), Some(9000));
        let http = p.http_port.unwrap();
        assert_eq!((http.port(), http.enable), (Some(80), Some(1)));
        assert_eq!(p.https_port.unwrap().port(), Some(443));
        assert_eq!(p.rtsp_port.unwrap().port(), Some(554));
        assert_eq!(p.rtmp_port.unwrap().port(), Some(1935));
        assert_eq!(p.onvif_port.unwrap().port(), Some(8000));
        let d = body(DDNS).ddns.unwrap();
        assert_eq!((d.enable, d.ddns_type.as_deref()), (Some(0), Some("no-ip")));
        let shown = format!("{d:?}");
        assert!(!shown.contains("host.example") && !shown.contains("someone") && !shown.contains("PLACEHOLDER"));
        let n = body(NTP).ntp.unwrap();
        assert_eq!((n.enable, n.server.as_deref(), n.port, n.synchronize_interval), (Some(1), Some("ntp.example"), Some(123), Some(1440)));
        let c = body(CERTIFICATE).certificate_info.unwrap();
        assert_eq!(c.enable, Some(0));
        let k = body(KIT_AP).kit_ap_cfg.unwrap();
        assert_eq!(k.bands.len(), 2);
        assert_eq!(k.bands[0].freq.as_deref(), Some("2.4G"));
        assert_eq!((k.bands[1].channel, k.bands[1].hide_ssid, k.bands[1].enable_wifi), (Some(36), Some(1), Some(1)));
        assert_eq!(k.bands[1].ssid.as_ref().map(Ssid::as_str), Some("NetworkName"));
        assert_eq!(k.bands[1].country_code.as_deref(), Some("FR"));
        let shown = format!("{k:?}");
        assert!(!shown.contains("NetworkName") && !shown.contains("PLACEHOLDER"));
        assert!(b.upnp.is_none() && body("<Upnp version=\"1.1\">\n<enable>0</enable>\n</Upnp>\n").upnp.is_some());
        assert_eq!(body("<PTOP version=\"1.1\">\n<enable>1</enable>\n</PTOP>\n").ptop.unwrap().enable, Some(1));
    }

    #[test]
    fn storage_settings() {
        let list = body(HDD_LIST).hdd_info_list.unwrap();
        assert_eq!(list.disks.len(), 2);
        let (total, free) = list.disks[0].gib();
        assert!((total - 1803.41).abs() < 0.01 && (free - 71.88).abs() < 0.01, "{total} {free}");
        let (total, free) = list.disks[1].gib();
        assert!((total - 1803.41).abs() < 0.01 && (free - 71.88).abs() < 0.01, "{total} {free}");
        assert_eq!((list.disks[0].format, list.disks[0].mount, list.disks[1].number), (Some(1), Some(1), Some(2)));
        let e = body(REC_ENC).rec_enc_cfg.unwrap();
        assert_eq!(e.enable, Some(0));
    }

    #[test]
    fn system_settings() {
        let b = body(SYSTEM_GENERAL);
        let g = b.system_general.unwrap();
        assert_eq!(g.time_zone, Some(-7200));
        assert_eq!(g.osd_format.as_deref(), Some("YMD"));
        assert_eq!((g.year, g.month, g.day, g.hour, g.minute, g.second), (Some(2026), Some(10), Some(18), Some(17), Some(24), Some(40)));
        assert_eq!((g.time_format, g.login_lock, g.lock_time, g.allowed_times, g.is_dst), (Some(0), Some(1), Some(300), Some(10), Some(1)));
        assert_eq!(g.device_name.as_deref(), Some("Hub"));
        assert_eq!(b.norm.unwrap().norm.as_deref(), Some("NTSC"));
        let d = body(DST).dst.unwrap();
        assert_eq!((d.enable, d.offset, d.start_month, d.start_week_index, d.start_weekday.as_deref()), (Some(1), Some(1), Some(3), Some(5), Some("Sunday")));
        assert_eq!((d.end_month, d.end_week_index, d.end_hour), (Some(10), Some(4), Some(4)));
        let a = body(DEVICE_AUDIO).device_audio_cfg.unwrap();
        assert_eq!((a.alarm_volume, a.cues_volume, a.alarm_pause_time), (Some(0), Some(5), Some(20)));
        let r = body(AUTO_REBOOT).auto_reboot.unwrap();
        assert_eq!((r.enable, r.week_day.as_deref(), r.hour, r.minute, r.second), (Some(1), Some("Sunday"), Some(11), Some(0), Some(0)));
    }

    #[test]
    fn channel_switches() {
        let r = body(RECORD_ENABLE).record_enable.unwrap();
        assert_eq!((r.enable, r.main_switch), (Some(68_719_476_735), Some(1)));
        assert!(r.channel_on(0) && r.channel_on(35) && !r.channel_on(36) && !r.channel_on(64));
        for root in ["EmailEnable", "PushEnable", "BuzzerEnable"] {
            let b = body(&format!("<{root} version=\"1.1\">\n<enable>8191</enable>\n<mainSwitch>1</mainSwitch>\n</{root}>\n"));
            let s = match root {
                "EmailEnable" => b.email_enable,
                "PushEnable" => b.push_enable,
                _ => b.buzzer_enable,
            }
            .unwrap();
            assert!((0..13).all(|c| s.channel_on(c)) && !s.channel_on(13));
        }
    }

    #[test]
    fn user_settings_hide_names_and_skip_secrets() {
        let b = body(USERS);
        let users = b.user_list.unwrap();
        assert_eq!(users.users.len(), 2);
        assert_eq!(users.users[0].user_name.as_ref().map(AccountName::as_str), Some("someone"));
        assert_eq!((users.users[0].user_level, users.users[0].login_state), (Some(1), Some(1)));
        assert_eq!((users.users[1].user_level, users.users[1].login_state), (Some(0), Some(0)));
        let shown = format!("{users:?}");
        assert!(!shown.contains("someone") && !shown.contains("guest") && !shown.contains("PLACEHOLDER"));
        let a = body(ACCESS_USERS).access_user_list.unwrap();
        assert_eq!((a.max_access_user_num, a.available_ability), (Some(10), Some(63)));
        assert_eq!(a.users.len(), 1);
        assert_eq!((a.users[0].user_level, a.users[0].valid_hours, a.users[0].ability, a.users[0].channel_ability), (Some(0), Some(25279), Some(39), Some(7)));
        let shown = format!("{a:?}");
        assert!(!shown.contains("example") && !shown.contains("phone") && !shown.contains("PLACEHOLDER"));
    }

    #[test]
    fn schedule_table_parsing() {
        assert!(ScheduleTable::parse(ONES).unwrap().all());
        assert!(!ScheduleTable::parse(ZEROS).unwrap().any());
        assert!(ScheduleTable::parse(&ONES[..167]).is_none());
        assert!(ScheduleTable::parse(&format!("{ONES}1")).is_none());
        assert!(ScheduleTable::parse(&format!("{}2{}", &ONES[..100], &ONES[101..])).is_none());
        assert!(ScheduleTable::parse("").is_none());
        // Index order: day * 24 + hour, first day first.
        let mut text = ZEROS.to_string();
        text.replace_range(2 * 24 + 5..2 * 24 + 6, "1");
        let t = ScheduleTable::parse(&text).unwrap();
        assert!(t.on(2, 5) && !t.on(5, 2) && t.any() && !t.all());
        assert!(!t.on(7, 0) && !t.on(0, 24));
        let shown = format!("{t:?}");
        assert_eq!(shown.len(), "ScheduleTable()".len() + 168 + 6);
        assert!(shown.starts_with("ScheduleTable(000000000000000000000000/"));
    }

    #[test]
    fn odd_numbers_do_not_hide_the_rest_of_a_body() {
        let b = body("<audioCfg version=\"1.1\">\n<channelId>0</channelId>\n<volume></volume>\n</audioCfg>\n<aiDenoise version=\"1.1\">\n<channelId>abc</channelId>\n<enable>-1</enable>\n<level>4</level>\n</aiDenoise>\n");
        let a = b.audio_cfg.unwrap();
        assert_eq!((a.channel_id, a.volume), (Some(0), None));
        let d = b.ai_denoise.unwrap();
        assert_eq!((d.channel_id, d.enable, d.level), (None, None, Some(4)));
        let b = body("<LedState version=\"1.1\">\n<IRLedBrightness/>\n<state>auto</state>\n</LedState>\n");
        let led = b.led_state.unwrap();
        assert_eq!((led.ir_led_brightness, led.state.as_deref()), (None, Some("auto")));
    }

    #[test]
    fn roots_lists_what_a_body_carries() {
        assert_eq!(body(NETWORK).roots(), vec!["Dhcp", "AutoDns", "Ip", "Dns"]);
        assert_eq!(body(USERS).roots(), vec!["UserList"]);
        assert!(body("").roots().is_empty());
    }

    #[test]
    fn malformed_replies_never_panic() {
        let roots = [
            "VersionInfo", "VideoInput", "InputAdvanceCfg", "DayNightThreshold", "OsdDatetime", "OsdChannelName", "AiCfg",
            "MD", "AiDetectCfg", "trackLimit", "trackSchedule", "LedState", "FloodlightTask", "audioCfg", "aiDenoise",
            "Compression", "Record", "EmailTask", "PushTask", "FtpTask", "AudioTask", "BuzzerTask", "RecordCfg", "PushCfg",
            "Dhcp", "AutoDns", "Ip", "Dns", "HddInfoList", "RecEncCfg", "ServerPort", "HttpPort", "HttpsPort", "RtspPort",
            "RtmpPort", "OnvifPort", "Upnp", "PTOP", "Ddns", "Ntp", "certificateInfo", "kitApCfg", "SystemGeneral", "Norm",
            "Dst", "DeiveAudioCfg", "RecordEnable", "EmailEnable", "PushEnable", "BuzzerEnable", "UserList", "accessUserList",
            "AutoReboot",
        ];
        for root in roots {
            for bad in [
                format!("</{root}><{root}>"),
                format!("<{root}><subStream/></subStream></{root}>"),
                format!("<{root}><enable/>x</enable></{root}>"),
                format!("<body><{root}><enable>4294967295999999999999</enable><cycle>x</cycle></{root}></body>"),
                format!("<body><{root}><item><type>MD</type><valueTable>1</valueTable></item></{root}></body>"),
                format!("<body><{root}><HddInfo><capacityV2>-1</capacityV2></HddInfo><User><userLevel>1<userLevel></User></{root}></body>"),
                format!("<body><{root}>"),
                format!("<body><{root}/></body>"),
                format!("<body><{root}>\u{0}\u{1}</{root}></body>"),
            ] {
                let _ = SettingsBody::from_bytes(bad.as_bytes());
            }
        }
        for bad in [&b""[..], b"<", b"\xff\xfe\x00", b"<body><body></body></body>", b"<?xml version=\"1.0\"?>"] {
            let _ = SettingsBody::from_bytes(bad);
        }
    }

    #[test]
    fn masked_newtypes_print_no_value() {
        assert_eq!(format!("{:?}", MacAddress::new("00:11:22:33:44:55")), "MacAddress(<hidden>)");
        assert_eq!(format!("{:?}", Ssid::new("Home")), "Ssid(<hidden>)");
        assert_eq!(format!("{:?}", AccountName::new("admin")), "AccountName(<hidden>)");
        assert_eq!(format!("{:?}", Some(AccountName::new("admin"))), "Some(AccountName(<hidden>))");
        assert!(AccountName::new(" ").is_empty());
    }
}
