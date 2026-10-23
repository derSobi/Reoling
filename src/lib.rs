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

pub use protocol::{login_refusal_reason, Error, Result};
pub use client::{looks_multi_channel, ChannelAbilities, ChannelInfo, DeviceIdentity, DeviceUpdate, DeviceInfoSummary, MonitorPoint, PtzPreset, ReolinkClient, StreamProfile, VideoFrame, VideoType};
pub use client::{
    file_for_time, next_file, sort_and_dedup_files, DownloadCancel, DownloadProgress, DownloadStream, DownloadSummary,
    DownloadUnit, FootageAnchor, RecordedEvent, RecordingFile, ReplayKeyframe, ReplayProgress, ReplayResume,
    ReplaySpeed, ReplayStatus, ReplayStream, ReplayUnit,
};
pub use protocol::bc::xml::{CameraUid, RecordStream, RecordingId};
pub use protocol::bcmedia::model::InfoBlock;
pub use protocol::time::CivilTime;
