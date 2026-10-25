//! Reoling: an unofficial Linux client for Reolink cameras, NVRs and Home Hubs
//! over the Baichuan P2P protocol.
//!
//! - [`protocol`]: the wire format (binary framing, XML payloads,
//!   BCEncrypt/AES, BCUDP), with no transport or session logic.
//! - [`transport`]: UDP/P2P discovery and the reliable BCUDP connection.
//! - [`client`]: resolve a UID, connect (direct or via relay), log in, and
//!   stream video, with no GUI-toolkit dependency.
//!
//! The GTK4 desktop app itself lives in the `reoling` binary (`src/main.rs`).

pub mod protocol;
pub mod transport;
pub mod client;
pub mod media_trace;
pub mod media_guard;
pub mod talk;
pub mod encoding;

pub use protocol::{Error, LoginRefusal, Result};
pub use client::{looks_multi_channel, ChannelAbilities, ChannelInfo, DeviceIdentity, DeviceUpdate, DeviceInfoSummary, MonitorPoint, PtzPreset, ReolinkClient, StreamProfile, VideoFrame, VideoType};
pub use client::{
    file_for_time, next_file, sort_and_dedup_files, DownloadCancel, DownloadProgress, DownloadStream, DownloadSummary,
    DownloadUnit, FootageAnchor, RecordedEvent, RecordingFile, ReplayKeyframe, ReplayProgress, ReplayResume,
    ReplaySpeed, ReplayStatus, ReplayStream, ReplayUnit,
};
pub use client::{
    settings_probe_channel, CameraDetectionPage, CameraDisplayPage, CameraImagePage, CameraInfoPage, CameraLightSoundPage,
    CameraPage, CameraSchedulesPage, DeviceAlertsPage, DeviceInfoPage, DeviceKind, DeviceLightSoundPage,
    DeviceMaintenancePage, DeviceNetworkPage, DevicePage, DeviceStoragePage, DeviceTimePage, DeviceUsersPage, NetworkRows,
    Read, ReadFailure, SettingsGates, SettingsPage, SettingsTarget, SETTINGS_REPLY_TIMEOUT,
};
pub use protocol::bc::settings_xml::{
    AccessUser, AccessUserList, AccountName, AiCfg, AiDenoise, AiDetectCfg, ApCfg, AudioCfg, AutoReboot, ChannelSwitch,
    CompressionAudio, DayNightThreshold, Ddns, DeviceAudioCfg, DnsConfig, Dst, Enable, FloodlightTask, HddInfo,
    HddInfoList, HourSpan, InputAdvanceCfg, IpConfig, KitApCfg, LedState, MacAddress, MinMaxCur, ModeOnly, MotionDetect,
    NoiseReduction, Norm, Ntp, OsdDatetime, OsdName, PowerLineFrequency, PushCfg, RecordCfg, ScheduleItem, ScheduleTable,
    ScheduledTask, SensitivityInfo, SensitivityInfoList, SensitivityNew, ServicePort, Ssid, StreamAudio, SystemGeneral,
    TrackLimit, TrackSchedule, TypeScheduleList, UserEntry, UserList,
};
pub use protocol::bc::xml::{CameraUid, RecordStream, RecordingId};
pub use protocol::bcmedia::model::InfoBlock;
pub use protocol::time::CivilTime;
