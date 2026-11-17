use crate::protocol::bc::model::*;
use crate::protocol::bc::xml::*;
use crate::protocol::bcmedia::model::{parse_one, BcMediaMessage};
use crate::protocol::crypto::{aes_key_from_password, bc_xor, EncryptionProtocol};
use crate::media_guard::{MediaGuard, Verdict};
use crate::transport::connection::BcConnection;
use crate::transport::discovery::{connect_by_uid_with_path, race_note, PeerHandle, DEBUG_RACE};
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
    /// The device says that it has a third stream between main and sub (`noExternStream` 0 in its support
    /// table, which is not per channel). Read for a device that sends no channel list; a channel list names
    /// each channel's streams itself.
    pub extern_stream: bool,
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
            Self::NotSent(e) => format!("version request not sent: {}", error_word(e)),
            Self::RecvError(e) => format!("receive error after {ms} ms ({others}): {}", error_word(e)),
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

/// The device's Baichuan TCP port, the one `connect_and_login_by_uid` tries
/// beside P2P.
const BASIC_SERVICE_PORT: u16 = 9000;

/// Which path a leg of `connect_and_login_by_uid` takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegKind {
    /// The P2P session `connect_race` confirms.
    P2pUdp,
    /// TCP to the device's LAN address, as the register server announced it and a local `D2C_CFM` from it confirmed.
    P2pTcp,
    /// The device that answered the LAN discovery, over UDP or TCP.
    LanUdp,
    LanTcp,
}

/// What the LAN legs of one race know about their host's answer to the credentials. The two go to the same
/// host, so they send it their credentials one after the other, each holding the lock from its request to the
/// reply (`ReolinkClient::login`): the cameras count failed logins, and a second request with the same password
/// would only be refused again. A leg whose request gets no usable answer leaves the turn open, and the other
/// then sends its own; that one can wait for the first for as long as a login reply may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LanTurn {
    /// No leg has an answer yet: the next one sends its credentials.
    Open,
    /// The host refused this password (401): the other leg does not send it again.
    Refused,
    /// A leg is logged in and has read the device's reply: the other one would only be logged out again.
    LoggedIn,
}

impl LanTurn {
    fn shared() -> Arc<tokio::sync::Mutex<Self>> {
        Arc::new(tokio::sync::Mutex::new(Self::Open))
    }
}

/// One leg of `connect_and_login_by_uid`: its client, connected when polled (or already logged in, as the P2P
/// UDP leg's is).
struct Leg {
    kind: LegKind,
    client: futures_util::future::BoxFuture<'static, crate::Result<ReolinkClient>>,
    /// Set once the race is won, when the leg goes on after that: the P2P UDP leg then starts no new attempt.
    race_over: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// How a P2P attempt of `connect_and_login_by_uid` is timed (`P2P_ATTEMPT_TIMES`; shorter in the tests).
#[derive(Debug, Clone, Copy)]
struct AttemptTimes {
    /// From the first `C2R_C` to the abandon when no `D2C_CFM` comes.
    unconfirmed: std::time::Duration,
    /// From the first `D2C_CFM` to the abandon when the login does not complete.
    confirmed: std::time::Duration,
    /// Between the abandon rounds (see `discovery::abandon`).
    gap: std::time::Duration,
    /// From the last abandon round to the next attempt's lookup.
    pause: std::time::Duration,
}

/// The official app abandons a silent attempt about 12 s after its first `D2C_CFM` (three cases in our captures)
/// and, in the one case without a `D2C_CFM`, 13 s after its `C2R_C`; it sends the abandon rounds 0.5 s apart and
/// looks up again 2-4.7 s after the last one (one case 8.6 s).
const P2P_ATTEMPT_TIMES: AttemptTimes = AttemptTimes {
    unconfirmed: std::time::Duration::from_secs(13),
    confirmed: std::time::Duration::from_secs(12),
    gap: std::time::Duration::from_millis(500),
    pause: std::time::Duration::from_secs(3),
};

/// How many P2P attempts `connect_and_login_by_uid` makes in all. Our captures show the official app making up to
/// three (two silent, the third logged in) and never giving up; the limit of three is ours.
const P2P_ATTEMPTS: u32 = 3;

/// How long `close_losers` waits for the legs still running: each that can
/// still log in ends by itself sooner (a TCP or LAN leg: its connect, then up
/// to two login steps of `LOGIN_REPLY_TIMEOUT`; the P2P UDP leg: its current
/// attempt, at most `unconfirmed` + `confirmed` of `P2P_ATTEMPT_TIMES` and its
/// abandon, as the race's end starts no new one), so none that logs in is left
/// without a logout. A P2P attempt still registering then is dropped.
const LOSERS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// Logs out of every leg that logs in after the race was won, until
/// `LOSERS_TIMEOUT`; a leg still pending then is dropped.
fn close_losers<S>(mut logins: S)
where
    S: futures_util::Stream<Item = (LegKind, crate::Result<ReolinkClient>)> + Send + Unpin + 'static,
{
    use futures_util::StreamExt;
    let close = tokio::time::timeout(LOSERS_TIMEOUT, async move {
        while let Some((kind, result)) = logins.next().await {
            match result {
                Ok(mut client) => {
                    race_note(format_args!("{kind:?} logged in after the race was won, logged out"));
                    client.disconnect().await;
                }
                Err(e) => race_note(format_args!("{kind:?} ended after the race was won: {}", error_word(&e))),
            }
        }
    });
    // The spawned task keeps the race's number and the device's place for its debug lines.
    let race = DEBUG_RACE.try_with(|race| *race).ok();
    tokio::spawn(crate::diag::with_device(async move {
        let _ = match race {
            Some(race) => DEBUG_RACE.scope(race, close).await,
            None => close.await,
        };
    }));
}

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
    /// or one built via `from_connection`. Aborted when the client is
    /// dropped (dropping the handle alone would only detach the task, which
    /// would go on sending); never polled directly.
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
    /// Set on the two LAN legs of `connect_and_login_by_uid`, which go to one host: they send it their credentials
    /// one after the other (see `login`).
    lan_turn: Option<Arc<tokio::sync::Mutex<LanTurn>>>,
    /// What turns on the legacy cipher step (see `ask_version`): `REOLING_TRY_LEGACY_CIPHER`, or a trial
    /// (`set_try_legacy`); `None`: off.
    try_legacy: Option<&'static str>,
    /// `REOLING_TRY_PREVIEW_POLICY`: the sub stream's Preview carries the policy element (see `preview_body`).
    try_policy: bool,
    /// The version reply did not read as XML under AES and does under BCEncrypt: a trial may reconnect with the
    /// legacy cipher step armed.
    legacy_candidate: bool,
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

/// How a trial asks for live video besides the stream (see `ReolinkClient::start_video_form`): only forms the code
/// already had, and that our captures of the official app show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PreviewForm {
    /// `<policy>newestIFrame</policy>` in the Preview, as the official app sends it for the sub stream.
    pub policy: bool,
}

/// One request form of a trial: the stream and how its Preview asks for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrialForm {
    pub stream: StreamProfile,
    pub form: PreviewForm,
}

impl TrialForm {
    /// As the trial lines name it: "subStream with policy", "mainStream under BCEncrypt".
    fn describe(&self, legacy: bool) -> String {
        let policy = if self.form.policy { " with policy" } else { "" };
        let cipher = if legacy { " under BCEncrypt" } else { "" };
        format!("{}{policy}{cipher}", stream_word(self.stream))
    }
}

/// How a trial ended on its connection (see `ReolinkClient::try_live`).
#[derive(Debug)]
pub enum TrialEnd {
    /// The device accepted `form`: its stream plays.
    Accepted { form: TrialForm, frames: ReceiverStream<crate::Result<VideoFrame>> },
    /// Every form was refused, and none is left.
    NoneAccepted,
    /// Every form on this connection was refused, and the version reply read as XML only under BCEncrypt: the
    /// device is to be connected again with the legacy cipher step armed (`set_try_legacy`), then
    /// `try_live_after_cipher`.
    Reconnect,
    /// A form went unanswered for `TRIAL_STEP`.
    TimedOut,
    /// The window cancelled the trial on its `TrialGate`: no form's send is admitted after that, and a stream a form
    /// started meanwhile is stopped.
    Cancelled,
    /// The connection broke during a form.
    Lost,
}

/// A device's trial number, shared by the window and the device's connections (see `ReolinkClient::try_live`).
/// The window cancels a trial by moving the number on; a trial's Preview is admitted for sending only while the
/// number still equals the trial's own. Cancel and send meet at this lock, held only for a synchronous check made at
/// the client's last point before the send (`begin`), never across the send, so `cancel` never waits for network
/// I/O. Once `cancel` has returned no new send of the trial is admitted. A send admitted earlier may still be under
/// way in the connection (waiting for its send lock over UDP, writing over TCP): it may go out after `cancel`
/// returned, or be dropped when the window drops the trial and closes the connection. A stream a Preview started is
/// stopped (`TrialEnd::Cancelled`).
#[derive(Debug, Clone, Default)]
pub struct TrialGate(Arc<std::sync::Mutex<u64>>);

impl TrialGate {
    fn lock(&self) -> std::sync::MutexGuard<'_, u64> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The number a trial started now goes under.
    pub fn number(&self) -> u64 {
        *self.lock()
    }

    /// Cancels the trial under the current number: once this has returned, no new send of a Preview of it is admitted.
    pub fn cancel(&self) {
        *self.lock() += 1;
    }

    /// Whether the trial under `trial` was cancelled.
    pub fn cancelled(&self, trial: u64) -> bool {
        self.number() != trial
    }

    /// Whether a send of the trial under `trial` is admitted: false once it was cancelled. Nothing is marked; the lock
    /// is released on return.
    fn begin(&self, trial: u64) -> bool {
        *self.lock() == trial
    }
}

/// How long a trial waits for the answer to one form.
const TRIAL_STEP: std::time::Duration = std::time::Duration::from_secs(10);

/// A stream as the Preview names it.
fn stream_word(stream: StreamProfile) -> &'static str {
    match stream {
        StreamProfile::Main => "mainStream",
        StreamProfile::Sub => "subStream",
        StreamProfile::Extern => "externStream",
    }
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
fn encoding_update(bc: &Bc, debug_id: u32) -> Option<DeviceUpdate> {
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
    if crate::diag::on() {
        crate::diag::line(format_args!(
            "{}DEBUG encoding reply: msg={id} channel={channel_id} code={} bytes={}",
            debug_prefix(debug_id, None),
            bc.meta.response_code,
            body.len()
        ));
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
    debug_id: u32,
) -> Option<DeviceUpdate> {
    // A reply to one of a settings page's reads goes to its page and
    // nowhere else (its id would mean something else below); nothing else
    // is routed by message number.
    match settings::settings_reply(bc, pending_images, debug_id) {
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
    if let Some(update) = encoding_update(bc, debug_id) {
        return Some(update);
    }
    if bc.meta.msg_id == MSG_ID_SET_COMPRESSION && crate::diag::on() {
        crate::diag::line(format_args!(
            "{}DEBUG encoding write reply: channel={} code={}",
            debug_prefix(debug_id, None),
            bc.meta.channel_id,
            bc.meta.response_code
        ));
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
    if bc.meta.msg_id == MSG_ID_OSD && crate::diag::on() {
        // The body holds the camera's name: its length only.
        let bytes = match &bc.body {
            BcBody::Modern(ModernMsg { payload: Some(p), .. }) => p.len(),
            _ => 0,
        };
        crate::diag::line(format_args!(
            "{}DEBUG osd reply: channel={} code={} bytes={bytes}",
            debug_prefix(debug_id, None),
            bc.meta.channel_id,
            bc.meta.response_code
        ));
    }
    // Each way the support table can be missing gets a debug line of its own; other messages pay nothing.
    let debug_support = bc.meta.msg_id == MSG_ID_SUPPORT && crate::diag::on();
    let BcBody::Modern(ModernMsg { extension_xml, payload: Some(payload) }) = &bc.body else {
        if debug_support {
            crate::diag::line(format_args!(
                "{}DEBUG support reply: msg={} code={} bytes=0, no payload",
                debug_prefix(debug_id, None),
                bc.meta.msg_id,
                bc.meta.response_code
            ));
        }
        return None;
    };
    let xml = match BcXml::from_bytes(payload) {
        Ok(xml) => xml,
        Err(e) => {
            if debug_support {
                let at = debug_prefix(debug_id, None);
                crate::diag::line(format_args!(
                    "{at}DEBUG support reply: msg={} code={} bytes={}, did not parse ({})",
                    bc.meta.msg_id,
                    bc.meta.response_code,
                    payload.len(),
                    parse_error_kind(payload, &e)
                ));
                // The session's cipher is not known here: the text as read only.
                crate::diag::masked(format_args!("{at}{}", body_text("support", None, payload)));
            }
            return None;
        }
    };
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
        if crate::diag::on() {
            let at = debug_prefix(debug_id, None);
            // One file line per channel: a multi-channel device's list is longer than a line.
            let channels: Vec<String> =
                abilities.iter().map(|(channel, a)| format!("{at}DEBUG abilities: channel {channel}: {a:?}")).collect();
            crate::diag::line_split(format_args!("{at}DEBUG abilities: {abilities:?}"), &channels);
        }
        return Some(DeviceUpdate::Abilities(abilities));
    }
    if debug_support {
        crate::diag::line(format_args!(
            "{}DEBUG support reply: msg={} code={} bytes={}, parsed, no Support in the body",
            debug_prefix(debug_id, None),
            bc.meta.msg_id,
            bc.meta.response_code,
            payload.len()
        ));
    }
    if let Some(list) = xml.channel_info_list {
        let channels = list.into_channels();
        remember_channel_uids(channel_uids, &channels);
        if crate::diag::on() {
            // Names stay out: counts only.
            crate::diag::line(format_args!(
                "{}DEBUG channel list pushed: {} channel(s), {} online, {} named",
                debug_prefix(debug_id, None),
                channels.len(),
                channels.iter().filter(|c| c.online).count(),
                channels.iter().filter(|c| !c.name.trim().is_empty()).count()
            ));
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

/// Names this build in a pasted negotiation log; goes with the crate version.
const DIAG_MARKER: &str = "diag-1";

/// The first negotiation debug line of the process, once: the crate version and `DIAG_MARKER`, nothing of the
/// machine or the build.
fn debug_banner() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    if crate::diag::on() {
        ONCE.call_once(|| {
            crate::diag::line(format_args!(
                "NEGOTIATION diagnostic build: reoling {} {DIAG_MARKER}",
                env!("CARGO_PKG_VERSION")
            ))
        });
    }
}

/// An error as a fixed word for the debug lines: the errors' own texts can quote an address or a device's XML.
fn error_word(e: &Error) -> std::borrow::Cow<'static, str> {
    match e {
        Error::LoginFailed { code } => format!("refused, code {code}").into(),
        Error::RegisterRejected { rsp } => format!("register rejected with rsp={rsp}").into(),
        Error::UidResolutionFailed => "lookup failed".into(),
        Error::P2pNegotiationTimeout | Error::ReplyTimeout => "timed out".into(),
        Error::ConnectionLost => "connection lost".into(),
        // `connect_by_ip`'s two texts, told apart by how they start.
        Error::ProtocolError(text) if text.starts_with("connecting to") => "connect timed out".into(),
        Error::ProtocolError(text) if text.starts_with("could not connect") => "connect failed".into(),
        Error::ProtocolError(_) => "protocol error".into(),
        Error::Io(_) => "socket error".into(),
        Error::Xml(_) => "reply did not parse".into(),
        _ => "other".into(),
    }
}

/// A stream table's `type` as a fixed word for the debug lines; any other text gives its length only.
fn stream_type_word(type_: &str) -> std::borrow::Cow<'static, str> {
    match type_ {
        "mainStream" => "mainStream".into(),
        "subStream" => "subStream".into(),
        "externStream" => "externStream".into(),
        "" => "empty".into(),
        other => format!("other ({} bytes)", other.len()).into(),
    }
}

/// The session cipher as the debug lines name it.
fn cipher_word(encryption: &EncryptionProtocol) -> &'static str {
    match encryption {
        EncryptionProtocol::Unencrypted => "none",
        EncryptionProtocol::BcEncrypt => "BCEncrypt",
        EncryptionProtocol::Aes { .. } => "aes",
    }
}

/// The `type` of a legacy login reply's `Encryption` as a fixed word; any other text gives its length only.
fn cipher_type_word(type_: &str) -> std::borrow::Cow<'static, str> {
    match type_ {
        "md5" => "md5".into(),
        "aes" => "aes".into(),
        "none" => "none".into(),
        "" => "empty".into(),
        other => format!("other ({} bytes)", other.len()).into(),
    }
}

/// Whether a body starts as XML ("<?xml") the way the session read it, and whether it would under the older
/// BCEncrypt scheme instead: (as read, under BCEncrypt). `payload` is what `read_bc` made of the body with
/// `encryption`, so the bytes as received are no longer held; encrypting again gives them back exactly (both
/// schemes undo byte for byte), and those are then read the older way. `offset` is the header's payload offset,
/// the Extension's length. Works on a copy of the first 16 bytes.
fn xml_under_ciphers(encryption: &EncryptionProtocol, offset: u32, payload: &[u8]) -> (bool, bool) {
    const XML: &[u8] = b"<?xml";
    let head = &payload[..payload.len().min(16)];
    let received = encryption.encrypt(offset, head);
    (head.starts_with(XML), bc_xor(offset, &received).starts_with(XML))
}

/// Why a body did not parse, as a fixed word for the debug lines: the parser's own text can quote the body. Reads
/// only the first `diag::BODY_INPUT_LIMIT` bytes, as the log does; a character the cut splits is no error.
fn parse_error_kind(payload: &[u8], e: &Error) -> &'static str {
    use quick_xml::DeError;
    let head = &payload[..payload.len().min(crate::diag::BODY_INPUT_LIMIT)];
    if std::str::from_utf8(head).is_err_and(|bad| bad.error_len().is_some() || head.len() == payload.len()) {
        return "bad utf-8";
    }
    if !head.trim_ascii_start().starts_with(b"<") {
        return "not xml";
    }
    let Error::Xml(e) = e else {
        return "other";
    };
    match e {
        DeError::InvalidXml(_) => "malformed xml",
        DeError::InvalidInt(e) => match e.kind() {
            std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => "number out of range",
            _ => "bad number",
        },
        DeError::InvalidFloat(_) => "bad number",
        DeError::InvalidBoolean(_) => "bad boolean",
        DeError::UnexpectedStart(_) | DeError::UnexpectedEnd(_) => "unexpected element",
        DeError::UnexpectedEof => "truncated",
        DeError::ExpectedStart => "not xml",
        // serde's own texts, told apart by how they start.
        DeError::Custom(text) if text.starts_with("duplicate field") => "duplicate element",
        DeError::Custom(text) if text.starts_with("missing field") => "missing element",
        DeError::Custom(text) if text.starts_with("invalid") => "unexpected value",
        _ => "other",
    }
}

/// What the debug lines say of a body that did not parse: its length, `xml_under_ciphers`' two answers and
/// `parse_error_kind`'s word; nothing of the body itself.
fn unparsed_body(encryption: &EncryptionProtocol, offset: u32, payload: &[u8], e: &Error) -> String {
    let yes_no = |yes| if yes { "yes" } else { "no" };
    let (as_read, legacy) = xml_under_ciphers(encryption, offset, payload);
    format!(
        "{} bytes, xml as read under {}: {}, xml under BCEncrypt: {}, parse error: {}",
        payload.len(),
        cipher_word(encryption),
        yes_no(as_read),
        yes_no(legacy),
        parse_error_kind(payload, e)
    )
}

/// What the debug lines say of a support reply that came during the version exchange: `unparsed_body` when it does
/// not parse, `None` when it does. This parse serves the log alone, so a body longer than `diag::BODY_INPUT_LIMIT`
/// is not parsed, only counted.
fn support_body_line(encryption: &EncryptionProtocol, offset: u32, payload: &[u8]) -> Option<String> {
    let limit = crate::diag::BODY_INPUT_LIMIT;
    if payload.len() > limit {
        return Some(format!("{} bytes, not parsed for the log (longer than {limit} bytes)", payload.len()));
    }
    BcXml::from_bytes(payload).err().map(|e| unparsed_body(encryption, offset, payload, &e))
}

/// EXPERIMENT (`REOLING_TRY_LEGACY_CIPHER`, off unless set): whether the session goes on under BCEncrypt. Only
/// with the switch, on a session under AES, whose version reply did not read as XML as received but does under
/// BCEncrypt: a device that reads AES is never touched.
fn legacy_cipher_wanted(switch: bool, encryption: &EncryptionProtocol, as_read: bool, legacy: bool) -> bool {
    switch && matches!(encryption, EncryptionProtocol::Aes { .. }) && !as_read && legacy
}

/// EXPERIMENT (`REOLING_TRY_PREVIEW_POLICY`, off unless set): `body`, a Preview request's, with the
/// `<policy>newestIFrame</policy>` the official app sends for the sub stream in our captures, after `streamType`.
/// Without `policy` the body is returned as it is.
fn preview_body(mut body: Vec<u8>, policy: bool) -> Vec<u8> {
    const END: &[u8] = b"</Preview>";
    if policy {
        if let Some(at) = body.windows(END.len()).position(|w| w == END) {
            body.splice(at..at, b"<policy>newestIFrame</policy>".iter().copied());
        }
    }
    body
}

/// A reply body's text for the debug lines, structure-masked (`crate::diag::mask_body`): as read when it is UTF-8,
/// else under BCEncrypt when `cipher` (the session's, and the payload offset) is known and that reads as markup.
/// Never a hex dump: BCEncrypt is a fixed XOR, so hex of such a body is plain text to anyone. Never called for a
/// login reply, which carries the nonce. Only the first `diag::BODY_INPUT_LIMIT` bytes are read, deciphered or
/// checked; the rest is counted.
fn body_text(kind: &str, cipher: Option<(&EncryptionProtocol, u32)>, payload: &[u8]) -> String {
    let head = &payload[..payload.len().min(crate::diag::BODY_INPUT_LIMIT)];
    let unread = payload.len() - head.len();
    let unread = if unread > 0 { format!(" ({unread} bytes not read)") } else { String::new() };
    // A character the cut split in two is no reason to read the body as not text.
    let text = |bytes: &[u8]| match std::str::from_utf8(bytes) {
        Ok(text) => Some(text.to_string()),
        Err(e) if e.error_len().is_none() && !unread.is_empty() => {
            Some(String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned())
        }
        Err(_) => None,
    };
    if let Some(text) = text(head) {
        return format!("DEBUG {kind} body text (as read): {}{unread}", crate::diag::mask_body(&text));
    }
    if let Some((encryption, offset)) = cipher {
        let legacy = bc_xor(offset, &encryption.encrypt(offset, head));
        if let Some(text) = text(&legacy).filter(|t| t.trim_start().starts_with('<')) {
            return format!("DEBUG {kind} body text (under BCEncrypt): {}{unread}", crate::diag::mask_body(&text));
        }
    }
    format!("DEBUG {kind} body text: none (not text under either cipher){unread}")
}

/// The kinds of `legs` for the race's debug lines ("P2pUdp, P2pTcp").
fn leg_kinds(legs: &[Leg]) -> String {
    legs.iter().map(|leg| format!("{:?}", leg.kind)).collect::<Vec<_>>().join(", ")
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
            .map(|t| format!("{} {}x{} {} fps", stream_type_word(&t.stream_type), t.width, t.height, t.default_frame_rate))
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
    /// direct, NAT-external "map", and relay) simultaneously — see
    /// `transport::discovery::connect_race` for the real-capture evidence this
    /// mirrors, and `transport::discovery::connect_by_uid`'s doc comment for
    /// how this replaced an earlier, disproven "skip direct whenever relay is
    /// available" strategy. The P2P part of `connect_and_login_by_uid`,
    /// without its TCP and LAN legs; used by the examples.
    pub async fn connect_by_uid(uid: &str) -> crate::Result<Self> {
        let socket = Arc::new(crate::transport::bind_udp_socket_with_large_rcvbuf().await?);
        let (peer, path): (PeerHandle, _) = connect_by_uid_with_path(&socket, uid).await?;
        Ok(Self::from_udp(socket, peer, path))
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

    /// Connects to the device `uid` and logs in, racing its paths like the
    /// official app, which runs TCP to port 9000 beside P2P and keeps the
    /// first login (our captures): the P2P session (`connect_by_uid`), TCP to
    /// the device's LAN address once a local `D2C_CFM` came from it (the
    /// official app connects before any reply; we wait so that no other host
    /// at that address gets the login), and a device that answers the LAN
    /// discovery (`discovery::lan_connect`),
    /// over UDP and TCP. The first login the device accepts wins; the others
    /// are logged out. The LAN legs join the race as soon as the LAN
    /// discovery is answered, whatever the registration does; after a refused
    /// registration (`rsp` != 0) the race waits for that answer.
    pub async fn connect_and_login_by_uid(uid: &str, username: &str, password: &str) -> crate::Result<Self> {
        use futures_util::FutureExt;
        let login = (uid.to_string(), username.to_string(), password.to_string());
        let p2p = Self::p2p_legs(login, BASIC_SERVICE_PORT).boxed();
        let lan = Self::lan_legs(uid, crate::transport::discovery::LAN_DISCOVERY, BASIC_SERVICE_PORT).boxed();
        Self::first_login(p2p, lan, username, password).await
    }

    /// The P2P branch of `connect_and_login_by_uid` for `login` (UID, user
    /// name, password): lookup and registration on a fresh socket, then
    /// `p2p_session_legs`.
    async fn p2p_legs(login: (String, String, String), tcp_port: u16) -> crate::Result<Vec<Leg>> {
        let socket = Arc::new(crate::transport::bind_udp_socket_with_large_rcvbuf().await?);
        let client_id = crate::transport::discovery::new_client_id();
        let first = crate::transport::discovery::register_attempt(&socket, &login.0, client_id, None).await?;
        Ok(Self::p2p_session_legs(socket, first, login, tcp_port, P2P_ATTEMPT_TIMES))
    }

    /// The legs of the registered P2P session `first`, neither started until
    /// polled: the UDP session, logged in by `p2p_attempts`, and TCP to
    /// `tcp_port` on the device's LAN address (`dev` of the register reply),
    /// if the reply gave one. The TCP leg connects only after the UDP leg's
    /// `connect_race` accepted a local `D2C_CFM` from that address, and then
    /// runs beside the UDP login; without one it ends without connecting.
    /// A new attempt of the UDP leg gives no new TCP leg.
    fn p2p_session_legs(
        socket: Arc<tokio::net::UdpSocket>,
        first: crate::transport::discovery::Registration,
        login: (String, String, String),
        tcp_port: u16,
        times: AttemptTimes,
    ) -> Vec<Leg> {
        use futures_util::FutureExt;
        let dev = first.register.dev;
        let race_over = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (local_confirmed, confirmed_ip) = tokio::sync::oneshot::channel();
        let udp = Self::p2p_attempts(socket, first, login, times, race_over.clone(), Some(local_confirmed));
        let mut legs = vec![Leg { kind: LegKind::P2pUdp, client: udp.boxed(), race_over: Some(race_over) }];
        if let Some(dev) = dev {
            // The register server's word alone does not tie this address to the UID: another host on the user's
            // network can hold the same private address. Only a host that answers our C2D_T with a local D2C_CFM
            // for this sid and cid, from this address, gets the TCP login; one that merely holds the address sends
            // none. In all our captures the device's local D2C_CFM came from `dev`'s address.
            let tcp = async move {
                if confirmed_ip.await.ok() != Some(dev.ip()) {
                    race_note(format_args!("P2pTcp not opened: no local D2C_CFM from the announced address"));
                    return Err(Error::P2pNegotiationTimeout);
                }
                let mut client = Self::connect_by_ip(dev.ip(), tcp_port).await?;
                client.p2p_path = Some("TCP");
                Ok(client)
            };
            legs.push(Leg { kind: LegKind::P2pTcp, client: tcp.boxed(), race_over: None });
        }
        legs
    }

    /// The P2P UDP leg of `connect_and_login_by_uid`, as the official app
    /// makes it (our captures): `connect_race` on `socket` for the registered
    /// session `first` and the login over the path it confirms, for `login`
    /// (UID, user name, password). An attempt that stays silent, without a
    /// `D2C_CFM` by `times.unconfirmed` after its registration or without a
    /// login reply by `times.confirmed` after its first `D2C_CFM`, is
    /// abandoned (`discovery::abandon`), and after `times.pause` a new one
    /// looks up and registers again on a new socket (`register_again`), up to
    /// `P2P_ATTEMPTS` in all and none once `race_over` is set. Gives the
    /// logged-in client; any other error ends the attempts with that error,
    /// and the last silent attempt ends them with `P2pNegotiationTimeout`.
    /// The first `D2C_CFM` on the local path sends its source IP to
    /// `local_confirmed`, before the login over UDP.
    async fn p2p_attempts(
        mut socket: Arc<tokio::net::UdpSocket>,
        first: crate::transport::discovery::Registration,
        login: (String, String, String),
        times: AttemptTimes,
        race_over: Arc<std::sync::atomic::AtomicBool>,
        mut local_confirmed: Option<tokio::sync::oneshot::Sender<std::net::IpAddr>>,
    ) -> crate::Result<Self> {
        use crate::transport::discovery::{abandon, connect_race_until, path_name, register_again};
        let (uid, username, password) = login;
        let mut attempt = first;
        for number in 1..=P2P_ATTEMPTS {
            let relay = attempt.register.relay.unwrap_or(attempt.lookup.relay);
            let (register, client_id) = (attempt.lookup.reg, attempt.client_id);
            let c2d_t_from = tokio::time::Instant::now();
            let deadline = attempt.registered + times.unconfirmed;
            let raced = connect_race_until(&socket, register, relay, &attempt.register, client_id, deadline).await;
            let confirmed = match raced {
                Ok((peer, conn, confirmed_at)) => {
                    if conn == "local" {
                        if let Some(sender) = local_confirmed.take() {
                            let _ = sender.send(peer.addr.ip());
                        }
                    }
                    let mut client = Self::from_udp(socket.clone(), peer.clone(), path_name(&conn));
                    let logged_in = client.login(&username, &password);
                    match tokio::time::timeout_at(confirmed_at + times.confirmed, logged_in).await {
                        Ok(Ok(_)) => return Ok(client),
                        Ok(Err(e)) => return Err(e),
                        // Dropped, the client stops receiving, acking, resending and its keepalive.
                        Err(_) => Some((peer, conn, confirmed_at, client.connection.lock().await.sent_data().await)),
                    }
                }
                Err(Error::P2pNegotiationTimeout) => None,
                Err(e) => return Err(e),
            };
            let reason = if confirmed.is_some() { "no login reply" } else { "no D2C_CFM" };
            race_note(format_args!(
                "P2P attempt {number} abandoned: {reason}, {} ms after its registration",
                attempt.registered.elapsed().as_millis()
            ));
            let confirmed_peer = confirmed.as_ref().map(|(peer, conn, at, _)| (peer, conn.as_str(), *at));
            let sent = confirmed.as_ref().map(|(.., sent)| *sent).unwrap_or_default();
            abandon(&socket, &uid, &attempt, confirmed_peer, sent, c2d_t_from, times.gap).await;
            let over = || race_over.load(std::sync::atomic::Ordering::Relaxed);
            if number == P2P_ATTEMPTS || over() {
                break;
            }
            tokio::time::sleep(times.pause).await;
            if over() {
                break;
            }
            // Bound while the old socket is still open, so the new attempt's port differs.
            let next = Arc::new(crate::transport::bind_udp_socket_with_large_rcvbuf().await?);
            race_note(format_args!("P2P attempt {}: new lookup and registration", number + 1));
            attempt = register_again(&next, &uid, &attempt).await?;
            socket = next;
            if over() {
                // Won while this one registered: its session is given up before its first `C2D_T`.
                let now = tokio::time::Instant::now();
                abandon(&socket, &uid, &attempt, None, Default::default(), now, times.gap).await;
                break;
            }
        }
        Err(Error::P2pNegotiationTimeout)
    }

    /// The LAN branch of `connect_and_login_by_uid`: `C2D_C` to `to` on a
    /// socket of its own, and for the device that answers, a UDP leg and TCP
    /// to `tcp_port` at its address. Neither opens anything until polled.
    async fn lan_legs(uid: &str, to: std::net::SocketAddr, tcp_port: u16) -> crate::Result<Vec<Leg>> {
        use futures_util::FutureExt;
        let socket = crate::transport::bind_udp_socket_with_large_rcvbuf().await?;
        socket.set_broadcast(true)?;
        let peer = crate::transport::discovery::lan_connect(&socket, uid, to).await?;
        let ip = peer.addr.ip();
        let socket = Arc::new(socket);
        let udp_turn = LanTurn::shared();
        let tcp_turn = udp_turn.clone();
        let tcp = async move {
            let mut client = Self::connect_by_ip(ip, tcp_port).await?;
            client.p2p_path = Some("TCP");
            client.lan_turn = Some(tcp_turn);
            Ok(client)
        };
        let udp = async move {
            let mut client = Self::from_udp(socket, peer, "LAN");
            client.lan_turn = Some(udp_turn);
            Ok(client)
        };
        Ok(vec![
            Leg { kind: LegKind::LanUdp, client: udp.boxed(), race_over: None },
            Leg { kind: LegKind::LanTcp, client: tcp.boxed(), race_over: None },
        ])
    }

    /// Drives the two branches and the logins of `connect_and_login_by_uid`
    /// and returns the first client whose login the device accepted. A wrong
    /// password (401) ends the race at once, except from a TCP or LAN leg:
    /// that one counts as a failed leg, never as a wrong password, because
    /// its peer may be a host other than the device. Any other refusal is
    /// kept and the race goes on. The LAN legs start as soon as the LAN
    /// discovery is answered and send their credentials one after the other
    /// (`LanTurn`). Once no leg runs the race ends, except that after a
    /// refused registration it waits for the LAN discovery's answer or
    /// silence. Legs still running after a win are logged out if they log in;
    /// after a wrong password they are dropped. With no winner the error is
    /// the last refusal, else the P2P error (the texts `connect_by_uid`
    /// gives), else a timeout: the TCP and LAN legs' own errors name an
    /// address, which a UID device's status must not show.
    async fn first_login(
        p2p: futures_util::future::BoxFuture<'_, crate::Result<Vec<Leg>>>,
        lan: futures_util::future::BoxFuture<'_, crate::Result<Vec<Leg>>>,
        username: &str,
        password: &str,
    ) -> crate::Result<Self> {
        debug_banner();
        // Each leg's progress for the negotiation debug output, under a number of its own (the device unnamed) and
        // the time since here (`race_note`).
        let race = (NEXT_DEBUG_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed), std::time::Instant::now());
        DEBUG_RACE.scope(race, Self::race_logins(p2p, lan, username, password)).await
    }

    /// `first_login`'s race itself, run inside `DEBUG_RACE`.
    async fn race_logins(
        p2p: futures_util::future::BoxFuture<'_, crate::Result<Vec<Leg>>>,
        lan: futures_util::future::BoxFuture<'_, crate::Result<Vec<Leg>>>,
        username: &str,
        password: &str,
    ) -> crate::Result<Self> {
        use futures_util::stream::FuturesUnordered;
        use futures_util::{FutureExt, StreamExt};
        let note = race_note;
        let mut branches = FuturesUnordered::new();
        branches.push(async move { (true, p2p.await) }.boxed());
        branches.push(async move { (false, lan.await) }.boxed());
        let login = |leg: Leg| {
            let (username, password) = (username.to_string(), password.to_string());
            async move {
                let logged_in = async {
                    let mut client = leg.client.await?;
                    // The P2P UDP leg logs in by itself (`p2p_attempts`).
                    if client.logged_in_at.is_none() {
                        note(format_args!("{:?} connected ([conn {}])", leg.kind, client.debug_id));
                        client.login(&username, &password).await?;
                    }
                    Ok(client)
                };
                (leg.kind, logged_in.await)
            }
            .boxed()
        };
        let mut logins = FuturesUnordered::new();
        // The legs to tell that the race is over (see `Leg::race_over`).
        let mut race_over: Vec<Arc<std::sync::atomic::AtomicBool>> = Vec::new();
        // The P2P branch and every P2P leg still running, and the LAN legs still running.
        let (mut p2p_open, mut lan_open) = (1usize, 0usize);
        // Set by a refused registration: the race then waits for the LAN discovery after the P2P legs end.
        let mut wait_for_lan = false;
        let mut refused: Option<Error> = None;
        let mut p2p_error: Option<Error> = None;
        // How the LAN discovery stood, for the decision to end and for the debug line that ends the race.
        let mut lan_answer: Option<bool> = None;
        let ended = |how: &str, lan_answer: Option<bool>| {
            let lan = match lan_answer {
                Some(true) => "answered",
                Some(false) => "not answered",
                None => "still asking",
            };
            note(format_args!("ended {how}; LAN discovery {lan}"));
        };
        loop {
            tokio::select! {
                Some((is_p2p, result)) = branches.next() => match (is_p2p, result) {
                    (true, Ok(legs)) => {
                        if crate::diag::on() {
                            note(format_args!("the P2P session gave {} leg(s): {}", legs.len(), leg_kinds(&legs)));
                        }
                        p2p_open = p2p_open + legs.len() - 1;
                        race_over.extend(legs.iter().filter_map(|leg| leg.race_over.clone()));
                        logins.extend(legs.into_iter().map(login));
                    }
                    (true, Err(e)) => {
                        if crate::diag::on() {
                            note(format_args!("no P2P session: {}", error_word(&e)));
                        }
                        p2p_open -= 1;
                        wait_for_lan = matches!(e, Error::RegisterRejected { .. });
                        p2p_error = Some(e);
                    }
                    (false, Ok(legs)) => {
                        // The LAN legs join the race here, as soon as a host answers the LAN discovery, whatever
                        // the registration does (the owner's ruling, 2026-11-17). What ties that host to the UID:
                        // it answered a C2D_C naming the UID with a D2C_C_R that echoes our random cid with rsp 0
                        // (`discovery::lan_connect` drops any other answer), and in our capture of 2026-11-17, with
                        // one character of the UID wrong, four runs of 31 broadcasts got no answer. What it does not
                        // prove: D2C_C_R names no UID, and the C2D_C is a broadcast, so any host on the LAN can
                        // read the cid and answer. Such a host gets the login; the official app logs in over BCUDP
                        // on the same answer (our captures). Its 401 is therefore never a wrong password (below).
                        let kinds = leg_kinds(&legs);
                        note(format_args!("a device answered the LAN discovery, its legs join the race: {kinds}"));
                        lan_answer = Some(true);
                        lan_open += legs.len();
                        logins.extend(legs.into_iter().map(login));
                    }
                    (false, Err(_)) => {
                        note(format_args!("no answer to the LAN discovery"));
                        lan_answer = Some(false);
                    }
                },
                Some((kind, result)) = logins.next() => {
                    if matches!(kind, LegKind::P2pUdp | LegKind::P2pTcp) {
                        p2p_open -= 1;
                    } else {
                        lan_open -= 1;
                    }
                    match result {
                        Ok(client) => {
                            note(format_args!("won by {kind:?} ([conn {}])", client.debug_id));
                            ended("with a winner", lan_answer);
                            race_over.iter().for_each(|over| over.store(true, std::sync::atomic::Ordering::Relaxed));
                            close_losers(logins);
                            return Ok(client);
                        }
                        Err(Error::LoginFailed { code }) => {
                            note(format_args!("{kind:?} refused, code {code}"));
                            let wrong_password = crate::LoginRefusal::from_code(code).forgets_saved_password();
                            if wrong_password && kind != LegKind::P2pUdp {
                                // A TCP connection or a LAN answer can come from another host: its 401 is no proof
                                // that the password is wrong, and reporting it would make the app forget a good one.
                            } else if wrong_password {
                                // 401 ends the race: a leg still pending could only send the same wrong password
                                // again.
                                ended("on a wrong password", lan_answer);
                                return Err(Error::LoginFailed { code });
                            } else {
                                refused = Some(Error::LoginFailed { code });
                            }
                        }
                        Err(e) => {
                            note(format_args!("{kind:?} failed: {}", error_word(&e)));
                            if kind == LegKind::P2pUdp {
                                // A later P2P attempt's registration refused, as the first one's would be.
                                wait_for_lan |= matches!(e, Error::RegisterRejected { .. });
                                p2p_error = Some(e);
                            }
                        }
                    }
                }
                else => break,
            }
            // Over once no leg runs, unless a refused registration leaves the LAN discovery still to answer.
            if p2p_open == 0 && lan_open == 0 && (lan_answer.is_some() || !wait_for_lan) {
                break;
            }
        }
        ended("without a winner", lan_answer);
        Err(refused.or(p2p_error).unwrap_or(Error::P2pNegotiationTimeout))
    }

    /// A UDP client on `peer`, with the direct path's keepalive when it is
    /// one; `path` names the path for the negotiation debug output.
    fn from_udp(socket: Arc<tokio::net::UdpSocket>, peer: PeerHandle, path: &'static str) -> Self {
        let connection = BcConnection::new(socket, peer);
        let keepalive = connection.spawn_direct_keepalive();
        let mut client = Self::from_connection(connection, EncryptionProtocol::Unencrypted);
        client.direct_keepalive_task = keepalive;
        client.p2p_path = Some(path);
        client
    }

    /// Connects directly to a device's Baichuan "Basic Service" TCP port
    /// (typically 9000) by IP: for a device added by IP, and the TCP legs of
    /// `connect_and_login_by_uid`. No P2P handshake, no relay, no
    /// direct-connect keepalive (`C2D_HB` has no meaning on a stable TCP
    /// connection — see `BcConnection::spawn_direct_keepalive`).
    pub async fn connect_by_ip(ip: std::net::IpAddr, port: u16) -> crate::Result<Self> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((ip, port)))
            .await
            .map_err(|_| Error::ProtocolError(format!("connecting to {ip}:{port} timed out")))?
            .map_err(|e| Error::ProtocolError(format!("could not connect to {ip}:{port}: {e}")))?;
        let connection = BcConnection::from_tcp(stream);
        Ok(Self::from_connection(connection, EncryptionProtocol::Unencrypted))
    }

    /// `connect_by_ip` under a connection number taken beforehand (`reserve_debug_id`), so that the lines of a
    /// failed attempt can carry the number too.
    pub async fn connect_by_ip_as(ip: std::net::IpAddr, port: u16, debug_id: u32) -> crate::Result<Self> {
        let mut client = Self::connect_by_ip(ip, port).await?;
        client.debug_id = debug_id;
        Ok(client)
    }

    /// A new connection number for the debug lines (see `connect_by_ip_as`).
    pub fn reserve_debug_id() -> u32 {
        NEXT_DEBUG_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Test/advanced entry point: wraps an already-established
    /// `BcConnection` (used directly in tests to skip P2P discovery, which
    /// is covered separately in `transport::discovery`'s own tests).
    pub fn from_connection(connection: BcConnection, encryption: EncryptionProtocol) -> Self {
        crate::diag::connection_started();
        debug_banner();
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
            lan_turn: None,
            try_legacy: std::env::var("REOLING_TRY_LEGACY_CIPHER").is_ok().then_some("REOLING_TRY_LEGACY_CIPHER"),
            try_policy: std::env::var("REOLING_TRY_PREVIEW_POLICY").is_ok(),
            legacy_candidate: false,
        }
    }

    /// Arms the legacy cipher step for this session, for a trial: `identity` then goes on under BCEncrypt when the
    /// version reply reads only that way.
    pub fn set_try_legacy(&mut self) {
        self.try_legacy = Some("legacy cipher");
    }

    /// Whether this session's version reply read as XML only under BCEncrypt (see `legacy_candidate`).
    pub fn legacy_candidate(&self) -> bool {
        self.legacy_candidate
    }

    /// This connection's number in the debug lines.
    pub fn debug_id(&self) -> u32 {
        self.debug_id
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
        let debug = crate::diag::on();
        let nonce = if let Some(nonce) = relay_nonce {
            if debug {
                crate::diag::line(format_args!(
                    "{}DEBUG login cipher: nonce from the UDP handshake",
                    debug_prefix(self.debug_id, None)
                ));
            }
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
            let (code, class) = (reply.meta.response_code, reply.meta.class);
            let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = reply.body else {
                return Err(Error::ProtocolError("expected an Encryption reply".to_string()));
            };
            let encryption = BcXml::from_bytes(&payload)?
                .encryption
                .ok_or_else(|| Error::ProtocolError("missing nonce in Encryption reply".to_string()))?;
            if debug {
                // The header's numbers and the type as a fixed word; never the nonce.
                crate::diag::line(format_args!(
                    "{}DEBUG login cipher: nonce from the legacy login reply, code {code:#06x} class {class:#06x}, type {}",
                    debug_prefix(self.debug_id, None),
                    cipher_type_word(&encryption.type_)
                ));
            }
            encryption.nonce
        };

        self.encryption = EncryptionProtocol::Aes {
            key: aes_key_from_password(password, &nonce),
        };

        // A LAN leg waits here for the other one's answer (see `LanTurn`), with nothing secret sent yet.
        let mut lan_turn = match &self.lan_turn {
            Some(turn) => Some(turn.clone().lock_owned().await),
            None => None,
        };
        if lan_turn.as_deref().is_some_and(|turn| *turn != LanTurn::Open) {
            // Nothing to log out of; over UDP the device is told that this session ends.
            self.connection.lock().await.disconnect().await;
            return Err(Error::ProtocolError("credentials not sent: the other LAN leg has the host's answer".into()));
        }

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
            let code = reply.meta.response_code;
            if let Some(turn) = lan_turn.as_deref_mut() {
                if crate::LoginRefusal::from_code(code).forgets_saved_password() {
                    *turn = LanTurn::Refused;
                }
            }
            return Err(Error::LoginFailed { code });
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
        if crate::diag::on() {
            let at = debug_prefix(self.debug_id, self.logged_in_at);
            if let Some(path) = self.p2p_path {
                crate::diag::line(format_args!("{at}NEGOTIATION p2p path: {path}"));
            }
            crate::diag::line(format_args!("{at}DEBUG login streams: {}", describe_streams(&self.encode_profiles)));
        }
        let device_info = BcXml::from_bytes(&payload)?.device_info.unwrap_or_default();
        // Only a login this client can use keeps the other LAN leg out.
        if let Some(turn) = lan_turn.as_deref_mut() {
            *turn = LanTurn::LoggedIn;
        }
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
                if crate::diag::on() {
                    let at = debug_prefix(self.debug_id, None);
                    crate::diag::line(format_args!(
                        "{at}DEBUG login: read past message {} under {}",
                        reply.meta.msg_id,
                        reply.meta.msg_num
                    ));
                }
                let update =
                    pushed_update(&reply, &mut image_chunks, &self.pending_images, &self.channel_uids, self.debug_id);
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
        let (name, model) = loop {
            let legacy_before = matches!(self.encryption, EncryptionProtocol::BcEncrypt);
            let asked = std::time::Instant::now();
            let (info, answer, others) = self.ask_version().await;
            let (name, model) = info.map(|i| (i.name, i.model)).unwrap_or_default();
            if crate::diag::on() {
                let at = debug_prefix(self.debug_id, self.logged_in_at);
                let version = answer.describe(asked.elapsed(), others);
                crate::diag::line(format_args!("{at}DEBUG identity version: {version}"));
                // The name stays out: it is the user's. The model tells which connection is which camera.
                crate::diag::line(format_args!(
                    "{at}DEBUG identity reply: name present={} model present={} ({} bytes)",
                    name.as_deref().is_some_and(|n: &str| !n.trim().is_empty()),
                    model.is_some(),
                    model.as_deref().map_or(0, str::len)
                ));
                match model.as_deref() {
                    // Without the learned names: a device's name often repeats its model, which tells the
                    // connections apart.
                    Some(model) => crate::diag::masked_keeping_names(format_args!(
                        "{at}DEBUG identity model: {}",
                        crate::diag::model_word(model)
                    )),
                    // Said too, so that every connection's log has a model line.
                    None => {
                        let none = "none (the version reply named no model)";
                        crate::diag::masked(format_args!("{at}DEBUG identity model: {none}"))
                    }
                }
            }
            // EXPERIMENT (`REOLING_TRY_LEGACY_CIPHER`): `ask_version` has just moved the session to BCEncrypt, so
            // both requests go out once more and the log shows what the device makes of them. Never without the
            // switch: the first pass ends the loop.
            if legacy_before || !matches!(self.encryption, EncryptionProtocol::BcEncrypt) {
                break (name, model);
            }
            self.request_for_channel(MSG_ID_SUPPORT, 0).await;
        };
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
        let debug = crate::diag::on();
        let try_legacy = self.try_legacy;
        // Debug only: whether the support table's reply (199, asked for just before) came meanwhile.
        let mut support_seen = false;
        let mut legacy_candidate = false;
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
                let (payload_offset, payload_len) = match &reply.body {
                    BcBody::Modern(ModernMsg { extension_xml, payload }) => {
                        (extension_xml.as_ref().map_or(0, Vec::len) as u32, payload.as_ref().map_or(0, Vec::len))
                    }
                    BcBody::Legacy(_) => (0, 0),
                };
                if debug && reply.meta.msg_id == MSG_ID_SUPPORT {
                    support_seen = true;
                    if let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = &reply.body {
                        if let Some(said) = support_body_line(&self.encryption, payload_offset, payload) {
                            let at = debug_prefix(self.debug_id, self.logged_in_at);
                            crate::diag::line(format_args!("{at}DEBUG support body: {said}"));
                            let cipher = Some((&self.encryption, payload_offset));
                            crate::diag::masked(format_args!("{at}{}", body_text("support", cipher, payload)));
                        }
                    }
                }
                let update =
                    pushed_update(&reply, &mut image_chunks, &self.pending_images, &self.channel_uids, self.debug_id);
                if update.is_some() || reply.meta.msg_id != MSG_ID_VERSION {
                    if debug {
                        // What the version line counts as "other messages": the header's numbers only.
                        crate::diag::line(format_args!(
                            "{}DEBUG identity other message: msg={} code={} class={:#06x} bytes={payload_len}",
                            debug_prefix(self.debug_id, self.logged_in_at),
                            reply.meta.msg_id,
                            reply.meta.response_code,
                            reply.meta.class
                        ));
                    }
                    if let Some(update) = update {
                        let _ = self.channel_updates_tx.send(update);
                    }
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
                        Err(e) => {
                            if debug {
                                let at = debug_prefix(self.debug_id, self.logged_in_at);
                                crate::diag::line(format_args!(
                                    "{at}DEBUG version body: {}",
                                    unparsed_body(&self.encryption, payload_offset, &payload, &e)
                                ));
                                let cipher = Some((&self.encryption, payload_offset));
                                crate::diag::masked(format_args!("{at}{}", body_text("version", cipher, &payload)));
                            }
                            // Kept for a trial (see `legacy_candidate`); computed on the bytes already read.
                            let (as_read, legacy) = xml_under_ciphers(&self.encryption, payload_offset, &payload);
                            legacy_candidate = legacy_cipher_wanted(true, &self.encryption, as_read, legacy);
                            VersionBody::Unparsed
                        }
                    },
                    _ => VersionBody::Empty,
                };
                answer = VersionAnswer::Reply { code: reply.meta.response_code, body };
                return;
            }
            answer = VersionAnswer::OnlyOthers;
        };
        let _ = tokio::time::timeout(IDENTITY_TIMEOUT, exchange).await;
        if debug && !support_seen {
            let at = debug_prefix(self.debug_id, self.logged_in_at);
            crate::diag::line(format_args!("{at}DEBUG support reply: none before the version request ended"));
        }
        if !matches!(self.encryption, EncryptionProtocol::BcEncrypt) {
            self.legacy_candidate = legacy_candidate;
        }
        if let (Some(switch), true) = (try_legacy, legacy_candidate) {
            // EXPERIMENT (`REOLING_TRY_LEGACY_CIPHER`, or a trial): this device's version reply reads as XML under
            // BCEncrypt and not under AES, so the rest of the session is tried under BCEncrypt. The login is over.
            self.encryption = EncryptionProtocol::BcEncrypt;
            let at = debug_prefix(self.debug_id, self.logged_in_at);
            crate::diag::always(format_args!(
                "{at}NEGOTIATION experiment {switch} active: the session goes on under BCEncrypt"
            ));
        }
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
                let update =
                    pushed_update(&bc, &mut image_chunks, &self.pending_images, &self.channel_uids, self.debug_id);
                if let Some(update) = update {
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
        // EXPERIMENT (`REOLING_TRY_PREVIEW_POLICY`, off unless set): the sub stream's Preview with the policy
        // element of the official app's (see `preview_body`). The main stream's was never captured.
        let policy = (self.try_policy && quality == StreamProfile::Sub).then_some("REOLING_TRY_PREVIEW_POLICY");
        self.start_preview(channel_id, quality, policy, trace).await
    }

    /// `start_video` in one of the request forms a trial tries (see `PreviewForm`).
    async fn start_video_form(
        &mut self,
        channel_id: u8,
        quality: StreamProfile,
        form: PreviewForm,
        gate: (&TrialGate, u64),
    ) -> crate::Result<ReceiverStream<crate::Result<VideoFrame>>> {
        self.start_preview_gated(channel_id, quality, form.policy.then_some("policy"), None, Some(gate)).await
    }

    /// Asks for live video on `channel` in the two other request forms, one after the other, after the device
    /// refused `refused` (with `code`, when known). Only when the user asks for it: each form is a Preview, which
    /// changes nothing on the device; each waits `TRIAL_STEP` at most. Stops at the first form the device accepts,
    /// whose stream then plays. Every form and answer goes to the diagnostic log. `trial` is the trial's number on
    /// `gate`: once the window has cancelled it there, no further form's send is admitted (see `TrialGate`). The
    /// caller that stops waiting for the form under way drops this future, with a send that may still be waiting,
    /// and closes the connection, which is then mid-reply.
    pub async fn try_live(
        &mut self,
        channel: u8,
        refused: StreamProfile,
        code: Option<u16>,
        gate: &TrialGate,
        trial: u64,
    ) -> TrialEnd {
        let candidate = self.legacy_candidate;
        let steps = if candidate { 3 } else { 2 };
        let code = code.map_or(String::new(), |code| format!(" code {code}"));
        self.trial_note(format_args!(
            "TRIAL started: channel {channel}, refused {}{code}, 2 forms on this connection, legacy cipher \
             candidate: {}",
            stream_word(refused),
            if candidate { "yes" } else { "no" }
        ));
        let sub = |policy| TrialForm { stream: StreamProfile::Sub, form: PreviewForm { policy } };
        let forms = if refused == StreamProfile::Sub {
            [sub(true), TrialForm { stream: StreamProfile::Main, form: PreviewForm::default() }]
        } else {
            [sub(false), sub(true)]
        };
        for (at, form) in forms.into_iter().enumerate() {
            let step = format!("{}/{steps}", at + 1);
            if let Some(end) = self.trial_step(channel, form, &step, false, (gate, trial)).await {
                return end;
            }
        }
        if gate.cancelled(trial) {
            return TrialEnd::Cancelled;
        }
        if candidate {
            self.trial_note(format_args!("TRIAL 3/3: legacy cipher, reconnecting"));
            return TrialEnd::Reconnect;
        }
        self.trial_note(format_args!(
            "TRIAL legacy cipher: not applicable (the version reply read as XML under the session cipher or under \
             neither)"
        ));
        self.trial_note(format_args!("TRIAL ended: no form accepted"));
        TrialEnd::NoneAccepted
    }

    /// The last step of a trial, on a connection made again with the legacy cipher step armed (`set_try_legacy`):
    /// the refused stream as it was asked for, then the sub stream with the policy. `gate` and `trial` as in
    /// `try_live`.
    pub async fn try_live_after_cipher(
        &mut self,
        channel: u8,
        refused: StreamProfile,
        gate: &TrialGate,
        trial: u64,
    ) -> TrialEnd {
        let legacy = matches!(self.encryption, EncryptionProtocol::BcEncrypt);
        let sub = TrialForm { stream: StreamProfile::Sub, form: PreviewForm { policy: true } };
        for form in [TrialForm { stream: refused, form: PreviewForm::default() }, sub] {
            if let Some(end) = self.trial_step(channel, form, "3/3", legacy, (gate, trial)).await {
                return end;
            }
        }
        // The legacy cipher step was armed for this connection only: the next one is made as always.
        self.trial_note(format_args!("TRIAL ended: no form accepted; connecting again as usual"));
        TrialEnd::NoneAccepted
    }

    /// One form of a trial: `None` when the device refused it, else how the trial ends.
    async fn trial_step(
        &mut self,
        channel: u8,
        form: TrialForm,
        step: &str,
        legacy: bool,
        gate: (&TrialGate, u64),
    ) -> Option<TrialEnd> {
        let cancelled = || gate.0.cancelled(gate.1);
        if cancelled() {
            return Some(TrialEnd::Cancelled);
        }
        let name = form.describe(legacy);
        self.trial_note(format_args!("TRIAL {step}: {name}"));
        match tokio::time::timeout(TRIAL_STEP, self.start_video_form(channel, form.stream, form.form, gate)).await {
            Ok(Ok(_)) if cancelled() => {
                self.trial_note(format_args!("TRIAL {step}: {name}: accepted after the trial was cancelled"));
                let _ = self.stop_video(channel).await;
                Some(TrialEnd::Cancelled)
            }
            Ok(Err(Error::StreamRefused { code, .. })) if cancelled() => {
                self.trial_note(format_args!("TRIAL {step}: refused, code {code}"));
                Some(TrialEnd::Cancelled)
            }
            Ok(Ok(frames)) => {
                self.trial_note(format_args!("TRIAL {step}: {name}: accepted"));
                self.trial_note(format_args!("TRIAL ended: accepted {name}"));
                Some(TrialEnd::Accepted { form, frames })
            }
            Ok(Err(Error::StreamRefused { code, .. })) => {
                self.trial_note(format_args!("TRIAL {step}: refused, code {code}"));
                None
            }
            // Also the form the gate held back (see `start_preview_gated`).
            Ok(Err(_)) if cancelled() => Some(TrialEnd::Cancelled),
            Ok(Err(_)) => {
                self.trial_note(format_args!("TRIAL ended: connection lost"));
                Some(TrialEnd::Lost)
            }
            Err(_) => {
                self.trial_note(format_args!("TRIAL ended: {step} timed out"));
                Some(TrialEnd::TimedOut)
            }
        }
    }

    /// A trial's line in the diagnostic log, under this connection.
    fn trial_note(&self, text: std::fmt::Arguments) {
        if crate::diag::on() {
            crate::diag::masked(format_args!("{}{text}", debug_prefix(self.debug_id, self.logged_in_at)));
        }
    }

    /// The Preview request and the stream it starts. `policy` names what asked for the policy element (an
    /// experiment's variable, or a trial), `None` for the request without it.
    async fn start_preview(
        &mut self,
        channel_id: u8,
        quality: StreamProfile,
        policy: Option<&'static str>,
        trace: Option<Arc<crate::media_trace::MediaTrace>>,
    ) -> crate::Result<ReceiverStream<crate::Result<VideoFrame>>> {
        self.start_preview_gated(channel_id, quality, policy, trace, None).await
    }

    /// `start_preview`, for a trial's form with its `gate` and number: the Preview is admitted only while the gate is
    /// open, checked at the last point before the send (see `TrialGate`). Held back, nothing is sent and an error
    /// returns.
    async fn start_preview_gated(
        &mut self,
        channel_id: u8,
        quality: StreamProfile,
        policy: Option<&'static str>,
        trace: Option<Arc<crate::media_trace::MediaTrace>>,
        gate: Option<(&TrialGate, u64)>,
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
        let before = (self.live_msg_num, self.playing.clone());
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
                payload: Some(preview_body(
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
                    policy.is_some(),
                )),
            }),
        };
        if let Some(switch) = policy {
            let at = debug_prefix(self.debug_id, self.logged_in_at);
            crate::diag::always(format_args!(
                "{at}NEGOTIATION experiment {switch} active: this Preview carries the policy element"
            ));
        }
        // DEBUG-ONLY, temporary (`REOLING_DEBUG_NEGOTIATION`): our own
        // plaintext Preview request and the camera's reply, to compare
        // against the official client's for a negotiated-parameter mismatch
        // that could explain a lower camera bitrate. No credentials here.
        let debug_negotiation = crate::diag::on();
        if debug_negotiation {
            if let BcBody::Modern(ModernMsg { payload: Some(p), .. }) = &request.body {
                crate::diag::line(format_args!(
                    "{}NEGOTIATION preview request: channel_id={} stream_type={} class={:#x} xml={}",
                    debug_prefix(self.debug_id, self.logged_in_at),
                    request.meta.channel_id,
                    request.meta.stream_type,
                    request.meta.class,
                    String::from_utf8_lossy(p)
                ));
                // The header as it goes out, in numbers: the body is the payload alone (no Extension, and both
                // ciphers keep the length), and bytes 12-13 are the channel and the stream type.
                crate::diag::line(format_args!(
                    "{}NEGOTIATION preview request header: msg_id={} body_len={} bytes12-13={} bytes14-15={} \
                     response_code={} class={:#x} payload_offset=0",
                    debug_prefix(self.debug_id, self.logged_in_at),
                    request.meta.msg_id,
                    p.len(),
                    u16::from_le_bytes([request.meta.channel_id, request.meta.stream_type]),
                    request.meta.msg_num,
                    request.meta.response_code,
                    request.meta.class
                ));
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
            // The last point before the write in the client: nothing here is awaited between this check and handing
            // the request to `send_bc`. Over UDP the send may still wait for the connection's own send lock; such a
            // send was admitted before the cancel and may go out, or be dropped with the connection (see `TrialGate`).
            if gate.is_some_and(|(gate, trial)| !gate.begin(trial)) {
                (self.live_msg_num, self.playing) = before;
                return Err(Error::ProtocolError("the trial was cancelled".to_string()));
            }
            connection.send_bc(&request, &self.encryption).await?;
        }
        // Pushes (channel lists, alarm events, ...) can come between our
        // request and the answer; they are not the answer.
        let mut image_chunks = HashMap::new();
        let ack = loop {
            let bc = self.connection.lock().await.recv_bc(&self.encryption).await?;
            let update = pushed_update(&bc, &mut image_chunks, &self.pending_images, &self.channel_uids, self.debug_id);
            if let Some(update) = update {
                let _ = self.channel_updates_tx.send(update);
            } else if bc.meta.msg_id == MSG_ID_VIDEO && bc.meta.msg_num == msg_num {
                // Same message number as our request: an earlier stream's
                // leftover frames carry theirs.
                break bc;
            }
        };
        if debug_negotiation {
            let (extension_len, payload_len) = match &ack.body {
                BcBody::Modern(ModernMsg { extension_xml, payload }) => {
                    (extension_xml.as_ref().map_or(0, Vec::len), payload.as_ref().map_or(0, Vec::len))
                }
                BcBody::Legacy(_) => (0, 0),
            };
            // -1: a header without the payload offset word.
            let payload_offset = if header_has_payload_offset(ack.meta.msg_id, ack.meta.class, ack.meta.response_code) {
                extension_len as i64
            } else {
                -1
            };
            let at = debug_prefix(self.debug_id, self.logged_in_at);
            crate::diag::line(format_args!(
                "{at}NEGOTIATION preview reply: response_code={} class={:#x} msg_id={} body_len={} bytes12-13={} \
                 bytes14-15={} payload_offset={payload_offset} payload_len={payload_len}",
                ack.meta.response_code,
                ack.meta.class,
                ack.meta.msg_id,
                extension_len + payload_len,
                u16::from_le_bytes([ack.meta.channel_id, ack.meta.stream_type]),
                ack.meta.msg_num
            ));
            if let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = &ack.body {
                if ack.meta.response_code != 200 && !payload.is_empty() {
                    let cipher = Some((&self.encryption, extension_len as u32));
                    crate::diag::masked(format_args!("{at}{}", body_text("preview", cipher, payload)));
                }
            }
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
        // Under the device's tag like the rest of its connection's lines: tokio does not carry it into the task.
        self.video_task = Some(tokio::spawn(crate::diag::with_device(async move {
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
                if let Some(update) = pushed_update(&bc, &mut image_chunks, &pending_images, &channel_uids, debug_id) {
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
                    crate::diag::line(format_args!("{}NEGOTIATION {line}", debug_prefix(debug_id, logged_in_at)));
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
                                if crate::diag::on() {
                                    let at = debug_prefix(debug_id, logged_in_at);
                                    crate::diag::line(format_args!(
                                        "{at}NEGOTIATION stream info unit: {width}x{height}"
                                    ));
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
                            if crate::diag::on() {
                                let at = debug_prefix(debug_id, logged_in_at);
                                crate::diag::line(format_args!(
                                    "{at}NEGOTIATION media resync: {e}; skipped {skip} bytes"
                                ));
                            }
                            if skip == 0 {
                                break;
                            }
                            buffer.drain(..skip);
                        }
                    }
                }
            }
        })));
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
        if crate::diag::on() {
            // The body is the device's own reply with the changes applied: its length only.
            crate::diag::line(format_args!(
                "{}DEBUG encoding write: channel={channel_id} bytes={}",
                debug_prefix(self.debug_id, self.logged_in_at),
                body.len()
            ));
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
                    let update =
                        pushed_update(&bc, &mut image_chunks, &self.pending_images, &self.channel_uids, self.debug_id);
                    if let Some(update) = update {
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

impl Drop for ReolinkClient {
    fn drop(&mut self) {
        // A losing leg of `connect_and_login_by_uid` must not keep sending `C2D_HB` beside the winner.
        if let Some(task) = &self.direct_keepalive_task {
            task.abort();
        }
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

    /// What the log says of a body that did not parse reads only its first `diag::BODY_INPUT_LIMIT` bytes: a byte
    /// past them changes nothing. A long support body is not parsed for the log at all.
    #[test]
    fn log_only_checks_of_a_body_stop_at_its_bound() {
        let mut payload = b"<?xml ".to_vec();
        payload.resize(2 * crate::diag::BODY_INPUT_LIMIT, b' ');
        let before = parse_error_kind(&payload, &Error::ConnectionLost);
        payload.push(0xff);
        assert_eq!(parse_error_kind(&payload, &Error::ConnectionLost), before);
        assert_eq!(before, "other");
        // A character cut in two at the bound is no error either.
        let mut split = vec![b' '; crate::diag::BODY_INPUT_LIMIT - 1];
        split.insert(0, b'<');
        split.extend("é>".as_bytes());
        assert_eq!(parse_error_kind(&split, &Error::ConnectionLost), "other");
        assert_eq!(parse_error_kind(b"<a>\xff</a>", &Error::ConnectionLost), "bad utf-8");
        let line = support_body_line(&EncryptionProtocol::Unencrypted, 0, &payload).expect("a line");
        assert!(line.ends_with("not parsed for the log (longer than 4096 bytes)"), "{line}");
        assert!(support_body_line(&EncryptionProtocol::Unencrypted, 0, b"<body/>").is_none());
        let line = support_body_line(&EncryptionProtocol::Unencrypted, 0, b"<body>").expect("a line");
        assert!(line.starts_with("6 bytes,") && line.contains("parse error"), "{line}");
    }

    /// A body the device sent under BCEncrypt reads as XML that way and not as an AES session read it; one sent
    /// under AES the other way round, with and without an Extension ahead of it. A short or empty body is neither.
    #[test]
    fn a_body_is_tried_as_xml_under_the_session_cipher_and_under_bcencrypt() {
        let aes = EncryptionProtocol::Aes { key: aes_key_from_password("swordfish", "TESTNONCE") };
        let body = b"<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n</body>\n";
        for offset in [0u32, 5, 77] {
            // What `read_bc` holds after decrypting each device's bytes with the session's AES.
            let from_legacy_device = aes.decrypt(offset, &bc_xor(offset, body));
            assert_eq!(xml_under_ciphers(&aes, offset, &from_legacy_device), (false, true), "offset {offset}");
            let from_aes_device = aes.decrypt(offset, &aes.encrypt(offset, body));
            assert_eq!(xml_under_ciphers(&aes, offset, &from_aes_device), (true, false), "offset {offset}");
        }
        let legacy = EncryptionProtocol::BcEncrypt;
        assert_eq!(xml_under_ciphers(&legacy, 0, body), (true, true));
        assert_eq!(xml_under_ciphers(&EncryptionProtocol::Unencrypted, 0, &bc_xor(0, body)), (false, true));
        for short in [&b""[..], b"<?x", b"<"] {
            assert_eq!(xml_under_ciphers(&aes, 0, short), (false, false));
        }
        let line = unparsed_body(&aes, 0, &aes.decrypt(0, &bc_xor(0, body)), &Error::ConnectionLost);
        let start = format!("{} bytes, xml as read under aes: no, xml under BCEncrypt: yes, parse error: ", body.len());
        assert!(line.starts_with(&start), "{line}");
    }

    /// The word for a body that did not parse comes from the kind of error, never from its text.
    #[test]
    fn a_parse_error_is_named_by_a_fixed_word() {
        let kind = |body: &[u8]| {
            let e = BcXml::from_bytes(body).expect_err("the body must not parse");
            parse_error_kind(body, &e)
        };
        assert_eq!(kind(&[0x8f, 0xff, 0x00, 0x3c]), "bad utf-8");
        assert_eq!(kind(b"k3#secret text"), "not xml");
        assert_eq!(kind(b"<body"), "truncated");
        assert_eq!(kind(b"<body><VersionInfo><channelId>300</channelId></VersionInfo></body>"), "number out of range");
        assert_eq!(kind(b"<body><VersionInfo><channelId>front</channelId></VersionInfo></body>"), "bad number");
        assert_eq!(
            kind(b"<body><VersionInfo><name>a</name><name>b</name></VersionInfo></body>"),
            "duplicate element"
        );
        assert_eq!(kind(b"<body><VersionInfo></body>"), "malformed xml");
        let words = [
            "bad utf-8", "not xml", "malformed xml", "number out of range", "bad number", "bad boolean",
            "unexpected element", "truncated", "duplicate element", "missing element", "unexpected value", "other",
        ];
        for body in [&b"<body><Preview><handle>x</handle></Preview></body>"[..], b"<a>&bogus;</a>", b"<body><Preview/></body>"] {
            if let Err(e) = BcXml::from_bytes(body) {
                assert!(words.contains(&parse_error_kind(body, &e)));
            }
        }
        assert_eq!(parse_error_kind(b"<body/>", &Error::ConnectionLost), "other");
    }

    /// Errors, cipher types and device texts reach the debug lines as fixed words and numbers.
    #[test]
    fn debug_words_hold_no_address_and_no_device_text() {
        assert_eq!(error_word(&Error::ProtocolError("connecting to 192.0.2.7:9000 timed out".into())), "connect timed out");
        assert_eq!(
            error_word(&Error::ProtocolError("could not connect to 192.0.2.7:9000: refused".into())),
            "connect failed"
        );
        assert_eq!(error_word(&Error::ProtocolError("<uid>X</uid>".into())), "protocol error");
        assert_eq!(error_word(&Error::RegisterRejected { rsp: -1 }), "register rejected with rsp=-1");
        assert_eq!(error_word(&Error::LoginFailed { code: 409 }), "refused, code 409");
        assert_eq!(error_word(&Error::P2pNegotiationTimeout), "timed out");
        assert_eq!(error_word(&Error::ReplyTimeout), "timed out");
        assert_eq!(error_word(&Error::UidResolutionFailed), "lookup failed");
        assert_eq!(error_word(&Error::ConnectionLost), "connection lost");
        assert_eq!(error_word(&Error::StreamRefused { stream: StreamProfile::Sub, code: 400 }), "other");
        assert_eq!(cipher_type_word("md5"), "md5");
        assert_eq!(cipher_type_word("aes"), "aes");
        assert_eq!(cipher_type_word("none"), "none");
        assert_eq!(cipher_type_word(""), "empty");
        assert_eq!(cipher_type_word("user@example.com"), "other (16 bytes)");
        assert_eq!(stream_type_word("subStream"), "subStream");
        assert_eq!(stream_type_word("externStream"), "externStream");
        assert_eq!(stream_type_word(""), "empty");
        assert_eq!(stream_type_word("0123456789ABCDEF"), "other (16 bytes)");
        assert_eq!(cipher_word(&EncryptionProtocol::BcEncrypt), "BCEncrypt");
    }

    /// The Preview body is the same bytes as ever without the experiment's switch; with it, the one element is
    /// added after `streamType` and the body still reads as a Preview.
    #[test]
    fn the_preview_policy_experiment_changes_nothing_unless_switched_on() {
        let body = BcXml {
            preview: Some(Preview {
                version: XML_VERSION.to_string(),
                channel_id: 0,
                handle: 256,
                stream_type: Some("subStream".to_string()),
            }),
            ..Default::default()
        }
        .to_bytes();
        let plain = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><body><Preview version=\"1.1\"><channelId>0</channelId>\
                     <handle>256</handle><streamType>subStream</streamType></Preview></body>";
        assert_eq!(String::from_utf8_lossy(&body), plain);
        assert_eq!(preview_body(body.clone(), false), body);
        let with_policy = preview_body(body.clone(), true);
        let expected = plain.replace("</Preview>", "<policy>newestIFrame</policy></Preview>");
        assert_eq!(String::from_utf8_lossy(&with_policy), expected);
        assert_eq!(with_policy.len(), body.len() + 29);
        let preview = BcXml::from_bytes(&with_policy).unwrap().preview.expect("a Preview");
        assert_eq!((preview.handle, preview.stream_type.as_deref()), (256, Some("subStream")));
        // A body without a Preview is left alone.
        assert_eq!(preview_body(b"<body/>".to_vec(), true), b"<body/>");
    }

    /// The legacy-cipher experiment needs its switch, a session under AES, and a version reply that reads as XML
    /// only under BCEncrypt.
    #[test]
    fn the_legacy_cipher_experiment_needs_its_switch_and_an_unreadable_aes_reply() {
        let aes = EncryptionProtocol::Aes { key: [7u8; 16] };
        assert!(legacy_cipher_wanted(true, &aes, false, true));
        for (switch, as_read, legacy) in
            [(false, false, true), (true, true, true), (true, true, false), (true, false, false), (false, true, false)]
        {
            assert!(!legacy_cipher_wanted(switch, &aes, as_read, legacy), "{switch} {as_read} {legacy}");
        }
        assert!(!legacy_cipher_wanted(true, &EncryptionProtocol::BcEncrypt, false, true));
        assert!(!legacy_cipher_wanted(true, &EncryptionProtocol::Unencrypted, false, true));
    }

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
    use tokio::net::{TcpListener, UdpSocket};
    use tokio::task::JoinHandle;
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

    #[tokio::test]
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

    #[tokio::test]
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
    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
    async fn nothing_is_sent_while_a_playback_runs() {
        let (mut client, device) = client_and_device().await;
        let playback = tokio::spawn(std::future::pending::<()>());
        let end = playback.abort_handle();
        client.replay_task = Some(playback);
        quiet_until_ended(client, device, end).await;
    }

    #[tokio::test]
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
    #[tokio::test]
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

    /// A device that answers the version request under BCEncrypt while the session is under AES gives a body that
    /// does not parse, and without the experiment's switch the session stays under AES.
    #[tokio::test]
    async fn a_bcencrypt_version_reply_leaves_an_aes_session_as_it_is() {
        let aes = EncryptionProtocol::Aes { key: aes_key_from_password("swordfish", "TESTNONCE") };
        let (mut client, mut device) = client_and_device_with(aes.clone()).await;
        client.try_legacy = None;
        let fake = tokio::spawn(async move {
            let request = device.recv_bc(&aes).await.unwrap();
            assert_eq!(request.meta.msg_id, MSG_ID_VERSION);
            let version = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<VersionInfo version=\"1.1\">\n\
                           <name>Test camera</name>\n<type>TESTMODEL</type>\n</VersionInfo>\n</body>\n";
            let reply = device_message(MSG_ID_VERSION, request.meta.msg_num, 200, None, Some(version.into()));
            device.send_bc(&reply, &EncryptionProtocol::BcEncrypt).await.unwrap();
        });
        let (info, answer, _) = client.ask_version().await;
        assert!(info.is_none());
        assert!(matches!(answer, VersionAnswer::Reply { code: 200, body: VersionBody::Unparsed }), "{answer:?}");
        assert!(matches!(client.encryption, EncryptionProtocol::Aes { .. }));
        assert!(client.legacy_candidate(), "the reply reads as XML under BCEncrypt only");
        fake.await.unwrap();
    }

    /// One scripted session against a fake device: a login (the nonce from a legacy reply), `identity` with a
    /// version reply that does not parse, then a sub Preview refused with a body and a main Preview refused without
    /// one. Returns every message the device received, as read, and the client's connection number.
    async fn scripted_session() -> (Vec<String>, u32) {
        let (mut client, mut device) = client_and_device().await;
        let debug_id = client.debug_id();
        let fake = tokio::spawn(async move {
            let aes = EncryptionProtocol::Aes { key: aes_key_from_password("swordfish", "TESTNONCE") };
            let mut seen = Vec::new();
            let upgrade = device.recv_bc(&PLAIN).await.unwrap();
            seen.push(format!("{upgrade:?}"));
            let nonce = nonce_reply(upgrade.meta.msg_num, "TESTNONCE");
            device.send_bc(&nonce, &EncryptionProtocol::BcEncrypt).await.unwrap();
            let login = device.recv_bc(&aes).await.unwrap();
            seen.push(format!("{login:?}"));
            device.send_bc(&login_reply(login.meta.msg_num), &aes).await.unwrap();
            loop {
                let request = device.recv_bc(&aes).await.unwrap();
                seen.push(format!("{request:?}"));
                if request.meta.msg_id == MSG_ID_VERSION {
                    // Placeholders only; the padding takes the text past its bound.
                    let version = format!(
                        "<body><VersionInfo version=\"1.1\"><name>Owner test@example.com</name>\
                         <serialNumber>TESTCAMUID000001</serialNumber><ip>192.168.1.50</ip>\
                         <firmwareVersion>v3.0.0.2356_23062000</firmwareVersion>{}<broken></body>",
                        "<pad>z</pad>".repeat(300)
                    );
                    let reply = device_message(MSG_ID_VERSION, request.meta.msg_num, 200, None, Some(version.into()));
                    device.send_bc(&reply, &aes).await.unwrap();
                    break;
                }
            }
            for body in [Some(b"<body><Error><reason>TESTCAMUID000001</reason></Error></body>".to_vec()), None] {
                let request = device.recv_bc(&aes).await.unwrap();
                seen.push(format!("{request:?}"));
                let refusal = device_message(MSG_ID_VIDEO, request.meta.msg_num, 400, None, body);
                device.send_bc(&refusal, &aes).await.unwrap();
            }
            seen
        });
        client.login("admin", "swordfish").await.unwrap();
        client.identity().await;
        for stream in [StreamProfile::Sub, StreamProfile::Main] {
            assert!(matches!(client.start_video(0, stream).await, Err(Error::StreamRefused { code: 400, .. })));
        }
        (fake.await.unwrap(), debug_id)
    }

    /// With the diagnostic log off or on, the device receives the same messages; with it on, the file holds the
    /// session's lines under its connection, and the version reply's text bounded and masked.
    #[tokio::test]
    async fn the_diagnostic_log_changes_nothing_on_the_wire_and_records_the_session() {
        let _held = crate::diag::TEST_FILE.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("diag-tests").join("session");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("reoling-diagnostic.log");
        crate::diag::set_file(None);
        let (off, _) = scripted_session().await;
        assert!(!dir.exists(), "no file while the switch is off");
        crate::diag::set_file(Some(path.clone()));
        let (on, debug_id) = scripted_session().await;
        crate::diag::set_file(None);
        assert_eq!(off, on);

        let file = std::fs::read_to_string(&path).unwrap();
        let ours: Vec<&str> = file.lines().filter(|l| l.contains(&format!("[conn {debug_id}"))).collect();
        for wanted in [
            "DEBUG login cipher: nonce from the legacy login reply",
            "DEBUG login streams:",
            "DEBUG identity version: version reply code 200",
            "DEBUG version body: ",
            "DEBUG identity model: none (the version reply named no model)",
            "DEBUG version body text (as read): <body><VersionInfo version=\"1.1\"><name><",
            "<firmwareVersion>v3.0.0.2356_23062000</firmwareVersion>",
            "NEGOTIATION preview request: channel_id=0 stream_type=1",
            "NEGOTIATION preview reply: response_code=400",
            "DEBUG preview body text (as read): <body><Error><reason><16 bytes></reason></Error></body>",
            "NEGOTIATION preview request: channel_id=0 stream_type=0",
        ] {
            assert!(ours.iter().any(|l| l.contains(wanted)), "no line with {wanted:?} in {ours:#?}");
        }
        let text = ours.iter().find(|l| l.contains("DEBUG version body text")).unwrap();
        assert!(text.contains(" more bytes)") && text.len() <= crate::diag::LINE_LIMIT, "{text}");
        for leak in ["TESTCAMUID", "example.com", "192.168", "Owner", "TESTNONCE", "swordfish"] {
            assert!(!file.contains(leak), "{leak} in the file");
        }
        assert_eq!(ours.iter().filter(|l| l.contains("DEBUG preview body text")).count(), 1, "{ours:#?}");
        assert!(ours.iter().all(|l| l.starts_with('+')), "{ours:#?}");
    }

    /// What a Preview request asks for, as the device reads it: (msg id, header stream type, stream, policy).
    fn asked_form(request: &Bc) -> (u32, u8, String, bool) {
        let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = &request.body else {
            return (request.meta.msg_id, request.meta.stream_type, String::new(), false);
        };
        let text = String::from_utf8_lossy(payload);
        let stream = BcXml::from_bytes(payload).ok().and_then(|x| x.preview).and_then(|p| p.stream_type);
        let policy = text.contains("<policy>newestIFrame</policy>");
        (request.meta.msg_id, request.meta.stream_type, stream.unwrap_or_default(), policy)
    }

    /// One Iframe of live video under `msg_num`, its picture `[0, 0, 0, 1, 0x67]`.
    fn iframe(msg_num: u16) -> Bc {
        let frame_data = [0u8, 0, 0, 1, 0x67];
        let mut media = Vec::new();
        media.extend_from_slice(&0x63643030u32.to_le_bytes());
        media.extend_from_slice(b"H264");
        media.extend_from_slice(&(frame_data.len() as u32).to_le_bytes());
        media.extend_from_slice(&0u32.to_le_bytes());
        media.extend_from_slice(&999u32.to_le_bytes());
        media.extend_from_slice(&0u32.to_le_bytes());
        media.extend_from_slice(&frame_data);
        media.extend_from_slice(&[0u8; 3]);
        let extension = Extension {
            version: XML_VERSION.to_string(),
            binary_data: Some(1),
            channel_id: Some(0),
            encrypt_len: Some(media.len() as u32),
            ..Default::default()
        };
        device_message(MSG_ID_VIDEO, msg_num, 0, Some(extension.to_bytes()), Some(media))
    }

    /// A huge reply body is cut to the mask's input bound before it is checked as text or deciphered: an invalid
    /// byte past the bound changes nothing, and the rest is only counted.
    #[test]
    fn a_huge_reply_body_is_cut_before_it_is_read() {
        let mut payload = b"<body>".to_vec();
        payload.resize(4_000_000, b'a');
        payload.push(0xff);
        let cipher = EncryptionProtocol::BcEncrypt;
        let started = std::time::Instant::now();
        let text = body_text("support", Some((&cipher, 0)), &payload);
        assert!(text.starts_with("DEBUG support body text (as read): <body>"), "{}", &text[..text.len().min(80)]);
        let unread = payload.len() - crate::diag::BODY_INPUT_LIMIT;
        assert!(text.ends_with(&format!("({unread} bytes not read)")), "{text}");
        assert!(started.elapsed() < std::time::Duration::from_millis(500), "{:?}", started.elapsed());
    }

    /// A device that refuses every Preview gets the sub stream with the policy, then the main stream, and nothing
    /// else; the log says each form and its answer, and how the trial ended.
    #[tokio::test]
    async fn a_trial_asks_each_form_in_turn_and_records_the_answers() {
        let _held = crate::diag::TEST_FILE.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("diag-tests").join("trial");
        let _ = std::fs::remove_dir_all(&dir);
        crate::diag::set_file(Some(dir.join("reoling-diagnostic.log")));
        let (mut client, mut device) = client_and_device().await;
        let debug_id = client.debug_id();
        let fake = tokio::spawn(async move {
            let mut asked = Vec::new();
            while let Ok(Ok(request)) = tokio::time::timeout(Duration::from_millis(500), device.recv_bc(&PLAIN)).await {
                asked.push(asked_form(&request));
                let refusal = device_message(MSG_ID_VIDEO, request.meta.msg_num, 400, None, None);
                device.send_bc(&refusal, &PLAIN).await.unwrap();
            }
            asked
        });
        let end = client.try_live(0, StreamProfile::Sub, Some(400), &TrialGate::default(), 0).await;
        assert!(matches!(end, TrialEnd::NoneAccepted), "{end:?}");
        let asked = fake.await.unwrap();
        crate::diag::set_file(None);
        assert_eq!(
            asked,
            [(MSG_ID_VIDEO, 1, "subStream".to_string(), true), (MSG_ID_VIDEO, 0, "mainStream".to_string(), false)]
        );
        let file = std::fs::read_to_string(dir.join("reoling-diagnostic.log")).unwrap();
        let trial: Vec<&str> = file
            .lines()
            .filter(|l| l.contains(&format!("[conn {debug_id}")) && l.contains("TRIAL"))
            .map(|l| &l[l.find("TRIAL").unwrap()..])
            .collect();
        assert_eq!(
            trial,
            [
                "TRIAL started: channel 0, refused subStream code 400, 2 forms on this connection, legacy cipher \
                 candidate: no",
                "TRIAL 1/2: subStream with policy",
                "TRIAL 1/2: refused, code 400",
                "TRIAL 2/2: mainStream",
                "TRIAL 2/2: refused, code 400",
                "TRIAL legacy cipher: not applicable (the version reply read as XML under the session cipher or under \
                 neither)",
                "TRIAL ended: no form accepted",
            ]
        );
    }

    /// The trial stops at the first form the device accepts, and that stream plays.
    #[tokio::test]
    async fn a_trial_stops_at_the_first_accepted_form() {
        let (mut client, mut device) = client_and_device().await;
        let fake = tokio::spawn(async move {
            let mut asked = Vec::new();
            for code in [400, 200] {
                let request = device.recv_bc(&PLAIN).await.unwrap();
                asked.push(asked_form(&request));
                let answer = device_message(MSG_ID_VIDEO, request.meta.msg_num, code, None, None);
                device.send_bc(&answer, &PLAIN).await.unwrap();
                if code == 200 {
                    device.send_bc(&iframe(request.meta.msg_num), &PLAIN).await.unwrap();
                }
            }
            let more = tokio::time::timeout(Duration::from_millis(300), device.recv_bc(&PLAIN)).await;
            (asked, more.is_ok())
        });
        let end = client.try_live(0, StreamProfile::Main, Some(400), &TrialGate::default(), 0).await;
        let TrialEnd::Accepted { form, mut frames } = end else {
            panic!("the second form was accepted");
        };
        assert_eq!(form, TrialForm { stream: StreamProfile::Sub, form: PreviewForm { policy: true } });
        let frame = frames.next().await.unwrap().unwrap();
        assert_eq!(frame.data, vec![0, 0, 0, 1, 0x67]);
        let (asked, more) = fake.await.unwrap();
        assert_eq!(
            asked,
            [(MSG_ID_VIDEO, 1, "subStream".to_string(), false), (MSG_ID_VIDEO, 1, "subStream".to_string(), true)]
        );
        assert!(!more, "nothing is sent after the accepted form");
    }

    /// Cancelled while its first form is under way (Stop, another channel, the log turned off): the trial waits
    /// for that answer and sends no other form.
    #[tokio::test]
    async fn a_cancelled_trial_sends_no_further_form() {
        let (mut client, mut device) = client_and_device().await;
        let gate = TrialGate::default();
        let set = gate.clone();
        let fake = tokio::spawn(async move {
            let mut asked = Vec::new();
            while let Ok(Ok(request)) = tokio::time::timeout(Duration::from_millis(500), device.recv_bc(&PLAIN)).await {
                asked.push(asked_form(&request));
                set.cancel();
                let refusal = device_message(MSG_ID_VIDEO, request.meta.msg_num, 400, None, None);
                device.send_bc(&refusal, &PLAIN).await.unwrap();
            }
            asked
        });
        let end = client.try_live(0, StreamProfile::Main, Some(400), &gate, 0).await;
        assert!(matches!(end, TrialEnd::Cancelled), "{end:?}");
        assert_eq!(fake.await.unwrap(), [(MSG_ID_VIDEO, 1, "subStream".to_string(), false)]);
    }

    /// The window cancels a trial while its first form waits for the connection (another task holds it): once the
    /// cancel has returned, no Preview of the trial goes out, and the trial ends as cancelled.
    #[tokio::test]
    async fn a_trial_cancelled_while_its_form_waits_for_the_connection_sends_no_preview() {
        let (mut client, mut device) = client_and_device().await;
        let held = client.connection.clone().lock_owned().await;
        let gate = TrialGate::default();
        let window_gate = gate.clone();
        let fake = tokio::spawn(async move {
            let mut asked = Vec::new();
            while let Ok(Ok(request)) = tokio::time::timeout(Duration::from_millis(300), device.recv_bc(&PLAIN)).await {
                asked.push(request.meta.msg_id);
                let refusal = device_message(MSG_ID_VIDEO, request.meta.msg_num, 400, None, None);
                device.send_bc(&refusal, &PLAIN).await.unwrap();
            }
            asked
        });
        let window = async move {
            // The trial has checked its number and waits for the connection by now.
            tokio::time::sleep(Duration::from_millis(50)).await;
            window_gate.cancel();
            drop(held);
        };
        let (end, ()) = tokio::join!(client.try_live(0, StreamProfile::Main, Some(400), &gate, 0), window);
        assert!(matches!(end, TrialEnd::Cancelled), "{end:?}");
        assert_eq!(fake.await.unwrap(), Vec::<u32>::new(), "no Preview after the cancel returned");
    }

    /// The cancel lands after the trial's number check before its form and before the send: `join!` polls the trial
    /// first, so it has checked its number and waits for the connection (another task holds it) when the window
    /// cancels. A check made anywhere before that wait lets the Preview out; the one at the last point before the
    /// write (`TrialGate::begin`) holds it back.
    #[tokio::test]
    async fn a_cancel_between_the_trial_s_check_and_its_send_stops_the_send() {
        for _ in 0..20 {
            let (mut client, mut device) = client_and_device().await;
            let held = client.connection.clone().lock_owned().await;
            let gate = TrialGate::default();
            let window_gate = gate.clone();
            let fake = tokio::spawn(async move {
                let mut asked = Vec::new();
                let wait = Duration::from_millis(300);
                while let Ok(Ok(request)) = tokio::time::timeout(wait, device.recv_bc(&PLAIN)).await {
                    asked.push(request.meta.msg_id);
                }
                asked
            });
            let window = async move {
                assert!(!window_gate.cancelled(0), "the trial checked an open number");
                window_gate.cancel();
                drop(held);
            };
            let (end, ()) = tokio::join!(client.try_live(0, StreamProfile::Main, Some(400), &gate, 0), window);
            assert!(matches!(end, TrialEnd::Cancelled), "{end:?}");
            assert_eq!(fake.await.unwrap(), Vec::<u32>::new(), "no Preview after the cancel returned");
        }
    }

    /// A form the device never answers ends the trial after `TRIAL_STEP` as a timeout, and the log says so (not
    /// that every form was refused).
    #[tokio::test(start_paused = true)]
    async fn an_unanswered_form_ends_the_trial_as_a_timeout() {
        let _held = crate::diag::TEST_FILE.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("diag-tests").join("timeout");
        let _ = std::fs::remove_dir_all(&dir);
        crate::diag::set_file(Some(dir.join("reoling-diagnostic.log")));
        let (mut client, mut device) = client_and_device().await;
        let debug_id = client.debug_id();
        let fake = tokio::spawn(async move {
            let request = device.recv_bc(&PLAIN).await.unwrap();
            let _ = device.recv_bc(&PLAIN).await; // nothing more comes; the connection goes with the test
            request.meta.msg_id
        });
        let end = client.try_live(0, StreamProfile::Sub, None, &TrialGate::default(), 0).await;
        crate::diag::set_file(None);
        assert!(matches!(end, TrialEnd::TimedOut), "{end:?}");
        fake.abort();
        let file = std::fs::read_to_string(dir.join("reoling-diagnostic.log")).unwrap();
        let ours: Vec<&str> =
            file.lines().filter(|l| l.contains(&format!("[conn {debug_id}")) && l.contains("TRIAL")).collect();
        assert!(ours.last().is_some_and(|l| l.ends_with("TRIAL ended: 1/2 timed out")), "{ours:#?}");
        assert!(!file.contains("no form accepted"), "{ours:#?}");
    }

    /// A multi-channel device's abilities reach the file whole, one line per channel.
    #[test]
    fn the_abilities_reach_the_file_one_line_per_channel() {
        let _held = crate::diag::TEST_FILE.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("diag-tests").join("abilities");
        let _ = std::fs::remove_dir_all(&dir);
        crate::diag::set_file(Some(dir.join("reoling-diagnostic.log")));
        let items: String = (0..32).map(|c| format!("<item><chnID>{c}</chnID><ptzType>5</ptzType></item>")).collect();
        let body = format!("<?xml version=\"1.0\" ?>\n<body>\n<Support version=\"1.1\">{items}</Support>\n</body>\n");
        let reply = device_message(MSG_ID_SUPPORT, 9, 200, None, Some(body.into_bytes()));
        let pending = std::sync::Mutex::new(HashMap::new());
        let uids = std::sync::Mutex::new(HashMap::new());
        let update = pushed_update(&reply, &mut HashMap::new(), &pending, &uids, 999_101);
        crate::diag::set_file(None);
        assert!(matches!(update, Some(DeviceUpdate::Abilities(ref a)) if a.len() == 32), "{update:?}");
        let file = std::fs::read_to_string(dir.join("reoling-diagnostic.log")).unwrap();
        let lines: Vec<&str> =
            file.lines().filter(|l| l.contains("[conn 999101") && l.contains("DEBUG abilities")).collect();
        assert_eq!(lines.len(), 32, "{lines:#?}");
        assert!(lines.iter().all(|l| !l.contains(" more bytes)") && l.contains("pan: true")), "{lines:#?}");
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
    #[tokio::test]
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

    /// `first_login` with these branches, as "admin" / "swordfish".
    async fn race(
        p2p: impl std::future::Future<Output = crate::Result<Vec<Leg>>> + Send + 'static,
        lan: impl std::future::Future<Output = crate::Result<Vec<Leg>>> + Send + 'static,
    ) -> crate::Result<ReolinkClient> {
        ReolinkClient::first_login(Box::pin(p2p), Box::pin(lan), "admin", "swordfish").await
    }

    fn leg(
        kind: LegKind,
        client: impl std::future::Future<Output = crate::Result<ReolinkClient>> + Send + 'static,
    ) -> Leg {
        Leg { kind, client: Box::pin(client), race_over: None }
    }

    /// A P2P UDP leg whose device (the nonce came with the handshake) answers
    /// the login with `reply`, `delay` after the request. Gives back the
    /// device's address and, once it has answered, its end of the connection.
    async fn udp_leg(delay: Duration, reply: fn(u16) -> Bc) -> (Leg, std::net::SocketAddr, JoinHandle<BcConnection>) {
        let (client, mut device) = client_and_device_with_nonce(PLAIN, Some("TESTNONCE")).await;
        let device_addr = client.peer_addr().await;
        let fake = tokio::spawn(async move {
            let aes = EncryptionProtocol::Aes { key: aes_key_from_password("swordfish", "TESTNONCE") };
            let login = device.recv_bc(&aes).await.unwrap();
            tokio::time::sleep(delay).await;
            device.send_bc(&reply(login.meta.msg_num), &aes).await.unwrap();
            device
        });
        (leg(LegKind::P2pUdp, async move { Ok(client) }), device_addr, fake)
    }

    /// A device on TCP that serves one login, the nonce step included.
    async fn tcp_camera() -> (std::net::SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fake = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            fake_login_with_nonce_step(BcConnection::from_tcp(stream), Vec::new(), Vec::new(), login_reply).await;
        });
        (addr, fake)
    }

    /// Listeners on the same port of two addresses. Another test may hold the port on the second address, so this
    /// tries until both bind.
    async fn same_port_on(first: &str, second: &str) -> (TcpListener, TcpListener, u16) {
        loop {
            let one = TcpListener::bind((first, 0)).await.unwrap();
            let port = one.local_addr().unwrap().port();
            if let Ok(other) = TcpListener::bind((second, port)).await {
                return (one, other, port);
            }
        }
    }

    /// Whether something connects to `listener` within 300 ms.
    async fn connects(listener: &TcpListener) -> bool {
        tokio::time::timeout(Duration::from_millis(300), listener.accept()).await.is_ok()
    }

    #[track_caller]
    fn assert_refused(result: crate::Result<ReolinkClient>, expected: u16) {
        match result {
            Err(Error::LoginFailed { code }) => assert_eq!(code, expected),
            Err(other) => panic!("expected code {expected}, got {other}"),
            Ok(_) => panic!("expected code {expected}, got a login"),
        }
    }

    fn refusal_401(msg_num: u16) -> Bc {
        device_message(MSG_ID_LOGIN, msg_num, 401, None, None)
    }

    fn refusal_409(msg_num: u16) -> Bc {
        device_message(MSG_ID_LOGIN, msg_num, 409, None, None)
    }

    /// Like the official app's in our captures: TCP logs in first and wins at
    /// once, without waiting for the LAN discovery, and the UDP leg is logged
    /// out once its own login is accepted.
    #[tokio::test]
    async fn a_tcp_login_that_finishes_first_wins_and_the_udp_leg_is_logged_out() {
        let (udp, _, fake_udp) = udp_leg(Duration::from_millis(300), login_reply).await;
        let (tcp_addr, fake_tcp) = tcp_camera().await;
        let tcp = leg(LegKind::P2pTcp, ReolinkClient::connect_by_ip(tcp_addr.ip(), tcp_addr.port()));
        let won = race(async move { Ok(vec![udp, tcp]) }, std::future::pending());
        let client = tokio::time::timeout(Duration::from_secs(5), won).await.expect("no wait for the LAN").unwrap();
        assert_eq!(client.peer_addr().await, tcp_addr);
        fake_tcp.await.unwrap();
        let mut device = fake_udp.await.unwrap();
        let aes = EncryptionProtocol::Aes { key: aes_key_from_password("swordfish", "TESTNONCE") };
        let logout = async {
            while device.recv_bc(&aes).await.unwrap().meta.msg_id != MSG_ID_LOGOUT {}
        };
        tokio::time::timeout(Duration::from_secs(2), logout).await.expect("the UDP leg is logged out");
    }

    /// Answers the first `C2D_T` on `socket`, `delay` later, with a
    /// `D2C_CFM` of `conn`, did 2 and the nonce "TESTNONCE"; gives back where
    /// it came from.
    async fn confirm_c2d_t(socket: &UdpSocket, conn: &str, delay: Duration) -> std::net::SocketAddr {
        use crate::protocol::bcudp::codec::{read_bcudp, write_bcudp};
        use crate::protocol::bcudp::model::{BcUdp, UdpDiscovery};
        use crate::protocol::bcudp::xml::{D2cCfm, UdpXml};
        let mut buf = [0u8; 2048];
        loop {
            let (n, from) = socket.recv_from(&mut buf).await.unwrap();
            let Ok(Some((BcUdp::Discovery(disc), _))) = read_bcudp(&buf[..n]) else { continue };
            let UdpXml::C2dT(request) = disc.payload else { continue };
            tokio::time::sleep(delay).await;
            let (sid, cid, nc) = (request.sid, request.cid, Some("TESTNONCE".to_string()));
            let cfm = D2cCfm { sid, conn: conn.to_string(), rsp: 0, cid, did: 2, nc, pl: String::new(), lver: 3 };
            let reply = BcUdp::Discovery(UdpDiscovery { tid: disc.tid, payload: UdpXml::D2cCfm(cfm) });
            socket.send_to(&write_bcudp(&reply), from).await.unwrap();
            return from;
        }
    }

    /// A registered session whose register reply named `dev` (or no device
    /// address) and `relay`, with TCP to `tcp_port` beside it.
    async fn session_legs(
        dev: Option<std::net::SocketAddr>,
        relay: std::net::SocketAddr,
        tcp_port: u16,
    ) -> Vec<Leg> {
        let register = UdpSocket::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let offer = crate::transport::discovery::RegisterResult { sid: 555, dev, map: None, relay: Some(relay) };
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let now = tokio::time::Instant::now();
        let first = crate::transport::discovery::Registration {
            lookup: crate::transport::discovery::UidLookupResult { reg: register, relay, debug: None },
            register: offer,
            client_id: 1,
            lookup_server: register,
            started: now,
            answered: now,
            registered: now,
        };
        let login = ("9527000EXAMPLE01".to_string(), "admin".to_string(), "swordfish".to_string());
        ReolinkClient::p2p_session_legs(socket, first, login, tcp_port, P2P_ATTEMPT_TIMES)
    }

    /// The device end of a P2P session on `dev`: confirms the first `C2D_T`
    /// as `conn` and answers the login.
    fn p2p_device(dev: UdpSocket, conn: &'static str) -> JoinHandle<()> {
        tokio::spawn(async move {
            let addr = confirm_c2d_t(&dev, conn, Duration::ZERO).await;
            let peer =
                PeerHandle { addr, local_connection_id: 2, remote_connection_id: 1, nonce: None, is_direct: false };
            fake_login_step(BcConnection::new(Arc::new(dev), peer), Vec::new(), login_reply).await;
        })
    }

    /// Once the device's local `D2C_CFM` came from the register reply's `dev` address, the TCP leg logs in there:
    /// here the device never answers the login over UDP, and TCP wins.
    #[tokio::test]
    async fn the_p2p_tcp_leg_logs_in_after_a_local_cfm_from_the_announced_address() {
        let dev = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay = UdpSocket::bind("127.0.0.2:0").await.unwrap();
        let (dev_addr, relay_addr) = (dev.local_addr().unwrap(), relay.local_addr().unwrap());
        let (tcp_addr, fake_tcp) = tcp_camera().await;
        let fake_dev = tokio::spawn(async move {
            confirm_c2d_t(&dev, "local", Duration::ZERO).await;
            dev
        });
        let legs = session_legs(Some(dev_addr), relay_addr, tcp_addr.port()).await;
        assert_eq!(legs.iter().map(|l| l.kind).collect::<Vec<_>>(), [LegKind::P2pUdp, LegKind::P2pTcp]);
        let won = race(async move { Ok(legs) }, std::future::pending());
        let client = tokio::time::timeout(Duration::from_secs(5), won).await.expect("TCP wins").unwrap();
        assert_eq!(client.peer_addr().await, tcp_addr);
        fake_tcp.await.unwrap();
        drop(fake_dev.await.unwrap());
    }

    /// A host at the register reply's `dev` address that never sends a `D2C_CFM` is not the device: it gets no TCP
    /// connection, so no login, and the relay's session wins.
    #[tokio::test]
    async fn a_host_at_the_announced_address_without_a_local_cfm_gets_no_login() {
        let dev = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay = UdpSocket::bind("127.0.0.2:0").await.unwrap();
        let (dev_addr, relay_addr) = (dev.local_addr().unwrap(), relay.local_addr().unwrap());
        let other_host = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _fake_relay = p2p_device(relay, "relay");
        let legs = session_legs(Some(dev_addr), relay_addr, other_host.local_addr().unwrap().port()).await;
        let won = race(async move { Ok(legs) }, std::future::pending());
        let client = tokio::time::timeout(Duration::from_secs(5), won).await.expect("the relay wins").unwrap();
        assert_eq!(client.peer_addr().await, relay_addr);
        assert!(!connects(&other_host).await, "the host at the announced address got a TCP connection");
        drop(dev);
    }

    /// A local `D2C_CFM` from another address than the announced one does not open TCP: neither to the announced
    /// address nor to the one the confirmation came from.
    #[tokio::test]
    async fn a_local_cfm_from_another_address_opens_no_tcp() {
        use crate::protocol::bcudp::codec::{read_bcudp, write_bcudp};
        use crate::protocol::bcudp::model::{BcUdp, UdpDiscovery};
        use crate::protocol::bcudp::xml::{D2cCfm, UdpXml};
        let dev = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let other = UdpSocket::bind("127.0.0.3:0").await.unwrap();
        let relay = UdpSocket::bind("127.0.0.2:0").await.unwrap();
        let (dev_addr, relay_addr) = (dev.local_addr().unwrap(), relay.local_addr().unwrap());
        let (other_host, announced_host, port) = same_port_on("127.0.0.3", "127.0.0.1").await;
        let legs = session_legs(Some(dev_addr), relay_addr, port).await;
        let racing = tokio::spawn(race(async move { Ok(legs) }, std::future::pending()));
        // The C2D_T reaches the announced address; the D2C_CFM that echoes it comes from another one.
        let mut buf = [0u8; 2048];
        loop {
            let (n, from) = dev.recv_from(&mut buf).await.unwrap();
            let Ok(Some((BcUdp::Discovery(disc), _))) = read_bcudp(&buf[..n]) else { continue };
            let UdpXml::C2dT(request) = disc.payload else { continue };
            let (sid, cid) = (request.sid, request.cid);
            let cfm = D2cCfm { sid, conn: "local".into(), rsp: 0, cid, did: 2, nc: None, pl: String::new(), lver: 3 };
            let reply = BcUdp::Discovery(UdpDiscovery { tid: disc.tid, payload: UdpXml::D2cCfm(cfm) });
            other.send_to(&write_bcudp(&reply), from).await.unwrap();
            break;
        }
        assert!(!connects(&other_host).await, "TCP went to the address the D2C_CFM came from");
        assert!(!connects(&announced_host).await, "TCP went to the announced address");
        racing.abort();
    }

    /// The UDP session that logs in first wins, and the TCP leg, whose login
    /// the device accepts later, is logged out.
    #[tokio::test]
    async fn a_udp_login_that_finishes_first_wins_and_the_tcp_leg_is_logged_out() {
        let dev = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay = UdpSocket::bind("127.0.0.2:0").await.unwrap();
        let (dev_addr, relay_addr) = (dev.local_addr().unwrap(), relay.local_addr().unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_port = listener.local_addr().unwrap().port();
        let fake_tcp = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut device = BcConnection::from_tcp(stream);
            let upgrade = device.recv_bc(&PLAIN).await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            let nonce = nonce_reply(upgrade.meta.msg_num, "TESTNONCE");
            device.send_bc(&nonce, &EncryptionProtocol::BcEncrypt).await.unwrap();
            let aes = EncryptionProtocol::Aes { key: aes_key_from_password("swordfish", "TESTNONCE") };
            let login = device.recv_bc(&aes).await.unwrap();
            device.send_bc(&login_reply(login.meta.msg_num), &aes).await.unwrap();
            while device.recv_bc(&aes).await.unwrap().meta.msg_id != MSG_ID_LOGOUT {}
        });
        let _fake_dev = p2p_device(dev, "local");
        let legs = session_legs(Some(dev_addr), relay_addr, tcp_port).await;
        let client = race(async move { Ok(legs) }, std::future::pending()).await.unwrap();
        assert_eq!(client.peer_addr().await, dev_addr);
        tokio::time::timeout(Duration::from_secs(2), fake_tcp).await.expect("the TCP leg is logged out").unwrap();
    }

    /// A register reply without a device address gives no TCP leg: nothing
    /// connects to the port, and the relay's session is the only leg.
    #[tokio::test]
    async fn no_p2p_tcp_leg_without_an_announced_address() {
        let relay = UdpSocket::bind("127.0.0.2:0").await.unwrap();
        let relay_addr = relay.local_addr().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _fake_relay = p2p_device(relay, "relay");
        let legs = session_legs(None, relay_addr, listener.local_addr().unwrap().port()).await;
        assert_eq!(legs.iter().map(|l| l.kind).collect::<Vec<_>>(), [LegKind::P2pUdp]);
        let client = race(async move { Ok(legs) }, std::future::pending()).await.unwrap();
        assert_eq!(client.peer_addr().await, relay_addr);
        assert!(!connects(&listener).await, "something connected over TCP");
    }

    /// A 401 from the P2P TCP leg, whose peer is proven only by a sid and cid
    /// echo, does not end the race and is never the result (the app would forget
    /// the saved password): the UDP session still wins, or its own refusal
    /// or error stands.
    #[tokio::test]
    async fn a_401_from_the_p2p_tcp_leg_does_not_end_the_race() {
        let tcp_401 = || leg(LegKind::P2pTcp, async { Err(Error::LoginFailed { code: 401 }) });
        let (udp, device_addr, _fake_udp) = udp_leg(Duration::from_millis(100), login_reply).await;
        let client = race(async move { Ok(vec![udp, tcp_401()]) }, std::future::pending()).await.unwrap();
        assert_eq!(client.peer_addr().await, device_addr);

        let (udp, _, _fake_udp) = udp_leg(Duration::from_millis(100), refusal_409).await;
        assert_refused(race(async move { Ok(vec![udp, tcp_401()]) }, std::future::pending()).await, 409);

        let udp = leg(LegKind::P2pUdp, async { Err(Error::P2pNegotiationTimeout) });
        let ended = race(async move { Ok(vec![udp, tcp_401()]) }, std::future::pending()).await;
        assert!(matches!(ended, Err(Error::P2pNegotiationTimeout)));

        // Whichever leg ends first, the UDP leg's own error stands.
        let udp = leg(LegKind::P2pUdp, async { Err(Error::ReplyTimeout) });
        let ended = race(async move { Ok(vec![tcp_401(), udp]) }, std::future::pending()).await;
        assert!(matches!(ended, Err(Error::ReplyTimeout)));
    }

    /// A losing leg that takes the whole negotiation and then logs in, well
    /// after `LOGIN_REPLY_TIMEOUT`, is still logged out.
    #[tokio::test]
    async fn a_loser_that_logs_in_late_is_still_logged_out() {
        let (client, mut device) = client_and_device_with(PLAIN).await;
        let late = async move {
            tokio::time::sleep(LOGIN_REPLY_TIMEOUT * 2 - Duration::from_secs(1)).await;
            (LegKind::P2pUdp, Ok(client))
        };
        close_losers(Box::pin(futures_util::stream::once(late)));
        tokio::time::sleep(LOGIN_REPLY_TIMEOUT * 2).await;
        let logout = async {
            while device.recv_bc(&PLAIN).await.unwrap().meta.msg_id != MSG_ID_LOGOUT {}
        };
        tokio::time::timeout(Duration::from_secs(2), logout).await.expect("the late leg is logged out");
    }

    /// A fake device on the LAN that answers every `C2D_C` with `answer(cid of the request)`.
    async fn lan_device(
        answer: fn(i32) -> crate::protocol::bcudp::xml::D2cCr,
    ) -> (std::net::SocketAddr, JoinHandle<()>) {
        use crate::protocol::bcudp::codec::{read_bcudp, write_bcudp};
        use crate::protocol::bcudp::model::{BcUdp, UdpDiscovery};
        use crate::protocol::bcudp::xml::UdpXml;
        let device = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = device.local_addr().unwrap();
        let fake = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while let Ok((n, from)) = device.recv_from(&mut buf).await {
                let Ok(Some((BcUdp::Discovery(disc), _))) = read_bcudp(&buf[..n]) else { continue };
                let UdpXml::C2dC(request) = disc.payload else { continue };
                let payload = UdpXml::D2cCr(answer(request.cid));
                let reply = BcUdp::Discovery(UdpDiscovery { tid: disc.tid, payload });
                let _ = device.send_to(&write_bcudp(&reply), from).await;
            }
        });
        (addr, fake)
    }

    /// A device on TCP that answers the nonce step `delay` late, accepts the login and waits for the logout.
    async fn slow_tcp_camera(delay: Duration) -> (std::net::SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fake = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut device = BcConnection::from_tcp(stream);
            let upgrade = device.recv_bc(&PLAIN).await.unwrap();
            tokio::time::sleep(delay).await;
            let nonce = nonce_reply(upgrade.meta.msg_num, "TESTNONCE");
            device.send_bc(&nonce, &EncryptionProtocol::BcEncrypt).await.unwrap();
            let aes = EncryptionProtocol::Aes { key: aes_key_from_password("swordfish", "TESTNONCE") };
            let login = device.recv_bc(&aes).await.unwrap();
            device.send_bc(&login_reply(login.meta.msg_num), &aes).await.unwrap();
            while device.recv_bc(&aes).await.unwrap().meta.msg_id != MSG_ID_LOGOUT {}
        });
        (addr, fake)
    }

    /// A device that answers the LAN discovery for our cid logs in over its LAN legs at once, while the P2P
    /// registration is still under way: LAN TCP wins.
    #[tokio::test]
    async fn a_lan_answer_wins_while_the_registration_is_slow() {
        use crate::protocol::bcudp::xml::D2cCr;
        let (device_addr, _fake_device) = lan_device(|cid| D2cCr { rsp: 0, cid, did: 7 }).await;
        let (tcp_addr, fake_tcp) = tcp_camera().await;
        let (udp, _, _fake_udp) = udp_leg(Duration::ZERO, login_reply).await;
        let p2p = async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok(vec![udp])
        };
        let lan = ReolinkClient::lan_legs("9527000EXAMPLE01", device_addr, tcp_addr.port());
        let client = tokio::time::timeout(Duration::from_secs(5), race(p2p, lan)).await.expect("a winner").unwrap();
        assert_eq!(client.peer_addr().await, tcp_addr);
        fake_tcp.await.unwrap();
    }

    /// The P2P leg already logging in loses to the LAN TCP leg and is logged out once the device accepts it.
    #[tokio::test]
    async fn a_p2p_leg_that_loses_to_a_lan_leg_is_logged_out() {
        use crate::protocol::bcudp::xml::D2cCr;
        let (device_addr, _fake_device) = lan_device(|cid| D2cCr { rsp: 0, cid, did: 7 }).await;
        let (tcp_addr, fake_tcp) = tcp_camera().await;
        let (udp, _, fake_udp) = udp_leg(Duration::from_millis(300), login_reply).await;
        let lan = ReolinkClient::lan_legs("9527000EXAMPLE01", device_addr, tcp_addr.port());
        let client = race(async move { Ok(vec![udp]) }, lan).await.unwrap();
        assert_eq!(client.peer_addr().await, tcp_addr);
        fake_tcp.await.unwrap();
        let mut device = fake_udp.await.unwrap();
        let aes = EncryptionProtocol::Aes { key: aes_key_from_password("swordfish", "TESTNONCE") };
        let logout = async {
            while device.recv_bc(&aes).await.unwrap().meta.msg_id != MSG_ID_LOGOUT {}
        };
        tokio::time::timeout(Duration::from_secs(2), logout).await.expect("the P2P leg is logged out");
    }

    /// The P2P legs ending without a refused registration do not end the race while a LAN leg still logs in.
    #[tokio::test]
    async fn a_lan_leg_still_logging_in_outlasts_the_p2p_legs() {
        let (tcp_addr, _fake_tcp) = slow_tcp_camera(Duration::from_millis(300)).await;
        // The P2P leg fails after the LAN answer is in, while the LAN TCP login is still under way.
        let failed = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Err(Error::P2pNegotiationTimeout)
        };
        let p2p = async move { Ok(vec![leg(LegKind::P2pUdp, failed)]) };
        let lan = vec![leg(LegKind::LanTcp, ReolinkClient::connect_by_ip(tcp_addr.ip(), tcp_addr.port()))];
        let client = race(p2p, async move { Ok(lan) }).await.unwrap();
        assert_eq!(client.peer_addr().await, tcp_addr);
    }

    /// An answer that does not echo our cid, or one with a non-zero rsp, releases no LAN leg: nothing connects
    /// to the answering host, and the P2P leg wins.
    #[tokio::test]
    async fn a_lan_answer_with_another_cid_or_rsp_releases_nothing() {
        use crate::protocol::bcudp::xml::D2cCr;
        let answers: [fn(i32) -> D2cCr; 2] =
            [|cid| D2cCr { rsp: 0, cid: cid.wrapping_add(1), did: 7 }, |cid| D2cCr { rsp: -1, cid, did: 7 }];
        for answer in answers {
            let (device_addr, _fake_device) = lan_device(answer).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (udp, udp_addr, _fake_udp) = udp_leg(Duration::from_millis(300), login_reply).await;
            let lan = ReolinkClient::lan_legs("9527000EXAMPLE01", device_addr, listener.local_addr().unwrap().port());
            let client = race(async move { Ok(vec![udp]) }, lan).await.unwrap();
            assert_eq!(client.peer_addr().await, udp_addr);
            assert!(!connects(&listener).await, "the LAN leg connected");
        }
    }

    /// Without an answer to the LAN discovery the P2P legs race as before: the P2P leg wins, its refusal stands,
    /// and a P2P error that is not a refused registration ends the race without waiting for the LAN discovery.
    #[tokio::test]
    async fn without_a_lan_answer_the_p2p_legs_race_as_before() {
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent_addr = silent.local_addr().unwrap();
        let (udp, device_addr, _fake_udp) = udp_leg(Duration::from_millis(300), login_reply).await;
        let lan = ReolinkClient::lan_legs("9527000EXAMPLE01", silent_addr, 9);
        let client = race(async move { Ok(vec![udp]) }, lan).await.unwrap();
        assert_eq!(client.peer_addr().await, device_addr);

        let (udp, _, _fake_udp) = udp_leg(Duration::from_millis(300), refusal_409).await;
        let ended = race(async move { Ok(vec![udp]) }, std::future::pending());
        assert_refused(tokio::time::timeout(Duration::from_secs(2), ended).await.expect("no wait for the LAN"), 409);

        let ended = race(async { Err(Error::UidResolutionFailed) }, std::future::pending());
        let ended = tokio::time::timeout(Duration::from_secs(1), ended).await.expect("no wait for the LAN");
        assert!(matches!(ended, Err(Error::UidResolutionFailed)));
        drop(silent);
    }

    /// Issue #4: a refused registration ends only the P2P branch; the device
    /// that answers the LAN discovery logs in over TCP (its UDP leg gets no
    /// answer).
    #[tokio::test]
    async fn register_rejected_and_a_lan_answer_still_logs_in() {
        use crate::protocol::bcudp::codec::{read_bcudp, write_bcudp};
        use crate::protocol::bcudp::model::{BcUdp, UdpDiscovery};
        use crate::protocol::bcudp::xml::{D2cCr, UdpXml};
        let device = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let device_addr = device.local_addr().unwrap();
        let fake_device = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                let (n, from) = device.recv_from(&mut buf).await.unwrap();
                let Ok(Some((BcUdp::Discovery(disc), _))) = read_bcudp(&buf[..n]) else { continue };
                let UdpXml::C2dC(request) = disc.payload else { continue };
                let reply = UdpXml::D2cCr(D2cCr { rsp: 0, cid: request.cid, did: 7 });
                let reply = BcUdp::Discovery(UdpDiscovery { tid: 1, payload: reply });
                device.send_to(&write_bcudp(&reply), from).await.unwrap();
                return device;
            }
        });
        let (tcp_addr, fake_tcp) = tcp_camera().await;
        let p2p = async { Err(Error::RegisterRejected { rsp: -1 }) };
        let lan = ReolinkClient::lan_legs("9527000EXAMPLE01", device_addr, tcp_addr.port());
        let client = race(p2p, lan).await.unwrap();
        assert_eq!(client.peer_addr().await, tcp_addr);
        fake_tcp.await.unwrap();
        drop(fake_device.await.unwrap());

        // The answer comes before the refusal: its leg logs in all the same.
        let (tcp_addr, fake_tcp) = tcp_camera().await;
        let p2p = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Err(Error::RegisterRejected { rsp: -1 })
        };
        let lan = vec![leg(LegKind::LanTcp, ReolinkClient::connect_by_ip(tcp_addr.ip(), tcp_addr.port()))];
        let client = race(p2p, async move { Ok(lan) }).await.unwrap();
        assert_eq!(client.peer_addr().await, tcp_addr);
        fake_tcp.await.unwrap();
    }

    /// A 401 from a LAN leg, whose answer may come from another host, never ends the race as a wrong password:
    /// the P2P leg still logs in, and with no winner the refused registration is the result. A refusal that is
    /// not a 401 leaves the other LAN leg running.
    #[tokio::test]
    async fn a_401_from_a_lan_leg_is_not_a_wrong_password() {
        let rejected = || async { Err(Error::RegisterRejected { rsp: -1 }) };
        let lan_401 = |kind| leg(kind, async { Err(Error::LoginFailed { code: 401 }) });
        let (udp, udp_addr, _fake_udp) = udp_leg(Duration::from_millis(100), login_reply).await;
        let lan = vec![lan_401(LegKind::LanTcp)];
        let client = race(async move { Ok(vec![udp]) }, async move { Ok(lan) }).await.unwrap();
        assert_eq!(client.peer_addr().await, udp_addr);

        let (tcp_addr, fake_tcp) = tcp_camera().await;
        let tcp = leg(LegKind::LanTcp, ReolinkClient::connect_by_ip(tcp_addr.ip(), tcp_addr.port()));
        let lan = vec![leg(LegKind::LanUdp, async { Err(Error::LoginFailed { code: 409 }) }), tcp];
        let client = race(rejected(), async move { Ok(lan) }).await.unwrap();
        assert_eq!(client.peer_addr().await, tcp_addr);
        fake_tcp.await.unwrap();

        let lan = vec![lan_401(LegKind::LanUdp), lan_401(LegKind::LanTcp)];
        let ended = race(rejected(), async move { Ok(lan) }).await;
        assert!(matches!(ended, Err(Error::RegisterRejected { rsp: -1 })), "{:?}", ended.err());
    }

    /// 401 ends the race at once: neither the pending P2P TCP leg, nor a LAN leg still logging in, nor the LAN
    /// discovery still asking holds it up. A LAN leg that has not sent its credentials yet never sends them.
    #[tokio::test]
    async fn a_wrong_password_ends_the_race_at_once() {
        // (a) beside a P2P TCP leg that never connects, with the LAN discovery still asking.
        let (udp, _, _fake_udp) = udp_leg(Duration::ZERO, refusal_401).await;
        let pending = leg(LegKind::P2pTcp, std::future::pending());
        let ended = race(async move { Ok(vec![udp, pending]) }, std::future::pending());
        assert_refused(tokio::time::timeout(Duration::from_secs(1), ended).await.expect("ended at once"), 401);

        // (b) beside a LAN TCP leg whose device answers the nonce step after the 401.
        let (udp, _, _fake_udp) = udp_leg(Duration::from_millis(100), refusal_401).await;
        let (tcp_addr, fake_tcp) = late_nonce_camera(Duration::from_millis(300)).await;
        let lan = vec![leg(LegKind::LanTcp, ReolinkClient::connect_by_ip(tcp_addr.ip(), tcp_addr.port()))];
        let ended = race(async move { Ok(vec![udp]) }, async move { Ok(lan) });
        assert_refused(tokio::time::timeout(Duration::from_secs(1), ended).await.expect("ended at once"), 401);
        let login = tokio::time::timeout(Duration::from_secs(2), fake_tcp).await.expect("the LAN leg is closed");
        assert!(!login.unwrap(), "the LAN leg sent its credentials after the 401");
    }

    /// A device on TCP that answers the nonce step `delay` late and then tells whether the credentials came.
    async fn late_nonce_camera(delay: Duration) -> (std::net::SocketAddr, JoinHandle<bool>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fake = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut device = BcConnection::from_tcp(stream);
            let upgrade = device.recv_bc(&PLAIN).await.unwrap();
            tokio::time::sleep(delay).await;
            let nonce = nonce_reply(upgrade.meta.msg_num, "TESTNONCE");
            let _ = device.send_bc(&nonce, &EncryptionProtocol::BcEncrypt).await;
            let aes = EncryptionProtocol::Aes { key: aes_key_from_password("swordfish", "TESTNONCE") };
            device.recv_bc(&aes).await.is_ok()
        });
        (addr, fake)
    }

    /// A device on TCP that answers the nonce step at once and the credentials `wait` later with `reply`, then
    /// reads until the connection ends.
    async fn answering_camera(wait: Duration, reply: fn(u16) -> Bc) -> (std::net::SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fake = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut device = BcConnection::from_tcp(stream);
            let upgrade = device.recv_bc(&PLAIN).await.unwrap();
            let nonce = nonce_reply(upgrade.meta.msg_num, "TESTNONCE");
            device.send_bc(&nonce, &EncryptionProtocol::BcEncrypt).await.unwrap();
            let aes = EncryptionProtocol::Aes { key: aes_key_from_password("swordfish", "TESTNONCE") };
            let login = device.recv_bc(&aes).await.unwrap();
            tokio::time::sleep(wait).await;
            let _ = device.send_bc(&reply(login.meta.msg_num), &aes).await;
            while device.recv_bc(&aes).await.is_ok() {}
        });
        (addr, fake)
    }

    /// Two LAN legs over TCP that take turns with their credentials, as the two of `lan_legs` do.
    fn lan_pair(first: std::net::SocketAddr, second: std::net::SocketAddr) -> Vec<Leg> {
        let turn = LanTurn::shared();
        let on = |kind, addr: std::net::SocketAddr, turn: Arc<tokio::sync::Mutex<LanTurn>>| {
            leg(kind, async move {
                let mut client = ReolinkClient::connect_by_ip(addr.ip(), addr.port()).await?;
                client.lan_turn = Some(turn);
                Ok(client)
            })
        };
        vec![on(LegKind::LanTcp, first, turn.clone()), on(LegKind::LanUdp, second, turn)]
    }

    /// The two LAN legs go to the same host and send it their credentials one after the other. A 401 to the
    /// first keeps the second from sending the same password there: the host sees one failed login from the LAN
    /// legs, not two. The 401 is still no wrong password: with the registration refused, that refusal stands.
    #[tokio::test]
    async fn after_a_401_the_other_lan_leg_does_not_send_its_credentials() {
        let (refusing, _fake_refusing) = answering_camera(Duration::from_millis(200), refusal_401).await;
        // Has its nonce while the first leg still waits for its answer.
        let (other, fake_other) = late_nonce_camera(Duration::from_millis(100)).await;
        let rejected = async { Err(Error::RegisterRejected { rsp: -1 }) };
        let ended = tokio::spawn(race(rejected, async move { Ok(lan_pair(refusing, other)) }));
        let login = tokio::time::timeout(Duration::from_secs(2), fake_other).await.expect("the LAN leg is closed");
        assert!(!login.unwrap(), "the other LAN leg sent its credentials after the 401");
        let ended = tokio::time::timeout(Duration::from_secs(2), ended).await.expect("the race ended").unwrap();
        assert!(matches!(ended, Err(Error::RegisterRejected { rsp: -1 })), "{:?}", ended.err());
    }

    /// With the right password the first LAN leg's login wins and the other, waiting for its turn, sends no
    /// credentials: the device sees one login from the LAN, not a second one that is logged out at once.
    #[tokio::test]
    async fn once_a_lan_leg_is_logged_in_the_other_sends_no_credentials() {
        let (accepting, _fake_accepting) = answering_camera(Duration::from_millis(200), login_reply).await;
        let (other, fake_other) = late_nonce_camera(Duration::from_millis(100)).await;
        let client = race(std::future::pending(), async move { Ok(lan_pair(accepting, other)) }).await.unwrap();
        assert_eq!(client.peer_addr().await, accepting);
        let login = tokio::time::timeout(Duration::from_secs(2), fake_other).await.expect("the LAN leg is closed");
        assert!(!login.unwrap(), "the other LAN leg sent its credentials after the first one's login");
    }

    /// A refusal that is not a 401 says nothing about the password: the other LAN leg then sends its credentials
    /// and logs in.
    #[tokio::test]
    async fn after_another_refusal_the_other_lan_leg_still_logs_in() {
        let (refusing, _fake_refusing) = answering_camera(Duration::from_millis(100), refusal_409).await;
        let (other, _fake_other) = slow_tcp_camera(Duration::from_millis(50)).await;
        let client = race(std::future::pending(), async move { Ok(lan_pair(refusing, other)) }).await.unwrap();
        assert_eq!(client.peer_addr().await, other);
    }

    /// A 200 the client cannot use (no device information in it) does not keep the other LAN leg out: it sends
    /// its own credentials and logs in.
    #[tokio::test]
    async fn after_an_unusable_200_the_other_lan_leg_still_logs_in() {
        let empty_200: fn(u16) -> Bc = |msg_num| device_message(MSG_ID_LOGIN, msg_num, 200, None, None);
        let (unusable, _fake_unusable) = answering_camera(Duration::from_millis(100), empty_200).await;
        let (other, _fake_other) = slow_tcp_camera(Duration::from_millis(50)).await;
        let rejected = async { Err(Error::RegisterRejected { rsp: -1 }) };
        let raced = race(rejected, async move { Ok(lan_pair(unusable, other)) });
        let client = tokio::time::timeout(Duration::from_secs(5), raced).await.expect("a winner").unwrap();
        assert_eq!(client.peer_addr().await, other);
    }

    /// The real pair of `lan_legs`: once TCP is logged in, the UDP leg, which has its nonce and waits for its
    /// turn, sends no credentials and tells the device that it leaves (`C2D_DISC`).
    #[tokio::test]
    async fn a_lan_udp_leg_that_gives_up_its_turn_says_goodbye() {
        use crate::protocol::bc::codec::{read_bc, write_bc};
        use crate::protocol::bcudp::codec::{read_bcudp, write_bcudp};
        use crate::protocol::bcudp::model::{BcUdp, UdpData, UdpDiscovery};
        use crate::protocol::bcudp::xml::{D2cCr, UdpXml};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_port = tcp.local_addr().unwrap().port();
        let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let udp_addr = udp.local_addr().unwrap();
        // TCP answers its credentials only once UDP has its nonce, and UDP gets its nonce only once TCP's
        // credentials are in: UDP then waits for the turn with nothing secret sent.
        let (tcp_has_credentials, tcp_credentials_in) = tokio::sync::oneshot::channel();
        let (udp_has_nonce, udp_nonce_out) = tokio::sync::oneshot::channel();
        let _fake_tcp = tokio::spawn(async move {
            let (stream, _) = tcp.accept().await.unwrap();
            let mut device = BcConnection::from_tcp(stream);
            let upgrade = device.recv_bc(&PLAIN).await.unwrap();
            let nonce = nonce_reply(upgrade.meta.msg_num, "TESTNONCE");
            device.send_bc(&nonce, &EncryptionProtocol::BcEncrypt).await.unwrap();
            let aes = EncryptionProtocol::Aes { key: aes_key_from_password("swordfish", "TESTNONCE") };
            let login = device.recv_bc(&aes).await.unwrap();
            tcp_has_credentials.send(()).unwrap();
            udp_nonce_out.await.unwrap();
            device.send_bc(&login_reply(login.meta.msg_num), &aes).await.unwrap();
            while device.recv_bc(&aes).await.is_ok() {}
        });
        let (goodbyes, credentials) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (said, sent) = (goodbyes.clone(), credentials.clone());
        let _fake_udp = tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let mut cid = 0;
            let (mut tcp_credentials_in, mut udp_has_nonce) = (Some(tcp_credentials_in), Some(udp_has_nonce));
            while let Ok((n, from)) = udp.recv_from(&mut buf).await {
                let Ok(Some((packet, _))) = read_bcudp(&buf[..n]) else { continue };
                match packet {
                    BcUdp::Discovery(UdpDiscovery { tid, payload: UdpXml::C2dC(request) }) => {
                        cid = request.cid;
                        let payload = UdpXml::D2cCr(D2cCr { rsp: 0, cid, did: 7 });
                        let reply = BcUdp::Discovery(UdpDiscovery { tid, payload });
                        udp.send_to(&write_bcudp(&reply), from).await.unwrap();
                    }
                    BcUdp::Discovery(UdpDiscovery { payload: UdpXml::C2dDisc(_), .. }) => {
                        said.fetch_add(1, Ordering::Relaxed);
                    }
                    BcUdp::Data(data) => {
                        let mut binary = std::collections::HashSet::new();
                        let Ok(Some((bc, _))) = read_bc(&data.payload, &PLAIN, &mut binary) else { continue };
                        if matches!(bc.body, BcBody::Legacy(LegacyMsg::LoginUpgrade)) {
                            if let Some(ready) = tcp_credentials_in.take() {
                                ready.await.unwrap();
                            }
                            let nonce = nonce_reply(bc.meta.msg_num, "TESTNONCE");
                            let payload = write_bc(&nonce, &EncryptionProtocol::BcEncrypt);
                            let reply = BcUdp::Data(UdpData { connection_id: cid, packet_id: 0, payload });
                            udp.send_to(&write_bcudp(&reply), from).await.unwrap();
                            if let Some(sent) = udp_has_nonce.take() {
                                sent.send(()).unwrap();
                            }
                        } else if bc.meta.msg_id == MSG_ID_LOGIN {
                            sent.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    _ => {}
                }
            }
        });
        let lan = ReolinkClient::lan_legs("9527000EXAMPLE01", udp_addr, tcp_port);
        let raced = race(std::future::pending(), lan);
        let client = tokio::time::timeout(Duration::from_secs(5), raced).await.expect("a winner").unwrap();
        assert_eq!(client.peer_addr().await.port(), tcp_port);
        let said_goodbye = async {
            while goodbyes.load(Ordering::Relaxed) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(2), said_goodbye).await.expect("the UDP leg sent no C2D_DISC");
        assert_eq!(credentials.load(Ordering::Relaxed), 0, "the UDP leg sent its credentials");
    }

    /// The two legs `lan_legs` gives take turns with their credentials: they share one `LanTurn`.
    #[tokio::test]
    async fn the_two_lan_legs_share_their_turn() {
        use crate::protocol::bcudp::xml::D2cCr;
        let (device_addr, _fake_device) = lan_device(|cid| D2cCr { rsp: 0, cid, did: 7 }).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_port = listener.local_addr().unwrap().port();
        let legs = ReolinkClient::lan_legs("9527000EXAMPLE01", device_addr, tcp_port).await.unwrap();
        let mut turns = Vec::new();
        for leg in legs {
            turns.push(leg.client.await.unwrap().lan_turn.clone().expect("a LAN leg without a turn"));
        }
        assert_eq!(turns.len(), 2);
        assert!(Arc::ptr_eq(&turns[0], &turns[1]), "each LAN leg has a turn of its own");
    }

    /// Both LAN legs go to the host that answered the discovery, not to the address the request went to: a
    /// broadcast is answered from the device's own address, as here. TCP connects there and UDP sends there.
    #[tokio::test]
    async fn the_lan_legs_go_to_the_address_that_answered() {
        use crate::protocol::bcudp::codec::{read_bcudp, write_bcudp};
        use crate::protocol::bcudp::model::{BcUdp, UdpDiscovery};
        use crate::protocol::bcudp::xml::{D2cCr, UdpXml};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let asked = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let asked_addr = asked.local_addr().unwrap();
        let answering = Arc::new(UdpSocket::bind("127.0.0.2:0").await.unwrap());
        let (tcp, elsewhere, tcp_port) = same_port_on("127.0.0.2", "127.0.0.1").await;
        // What reached the asked address besides the request, and what reached the answering one.
        let (stray, reached) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (answerer, strays) = (answering.clone(), stray.clone());
        let _asked = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while let Ok((n, from)) = asked.recv_from(&mut buf).await {
                let Ok(Some((BcUdp::Discovery(UdpDiscovery { tid, payload: UdpXml::C2dC(request) }), _))) =
                    read_bcudp(&buf[..n])
                else {
                    strays.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                let payload = UdpXml::D2cCr(D2cCr { rsp: 0, cid: request.cid, did: 7 });
                let reply = BcUdp::Discovery(UdpDiscovery { tid, payload });
                let _ = answerer.send_to(&write_bcudp(&reply), from).await;
            }
        });
        let heard = reached.clone();
        let _answering = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while answering.recv_from(&mut buf).await.is_ok() {
                heard.fetch_add(1, Ordering::Relaxed);
            }
        });
        let lan = ReolinkClient::lan_legs("9527000EXAMPLE01", asked_addr, tcp_port);
        let unanswered = tokio::time::timeout(Duration::from_millis(500), race(std::future::pending(), lan)).await;
        assert!(unanswered.is_err(), "nothing answers the logins here");
        assert!(connects(&tcp).await, "LAN TCP did not connect to the address that answered");
        assert!(!connects(&elsewhere).await, "LAN TCP connected to the address the discovery was sent to");
        assert!(reached.load(Ordering::Relaxed) > 0, "LAN UDP sent nothing to the address that answered");
        assert_eq!(stray.load(Ordering::Relaxed), 0, "LAN UDP sent to the address that was asked");
    }

    /// With no leg left, the P2P error is reported, as `connect_by_uid` gives it.
    #[tokio::test]
    async fn the_p2p_error_is_reported_when_every_leg_fails() {
        let p2p = async { Err(Error::RegisterRejected { rsp: -1 }) };
        let lan = async { Err(Error::P2pNegotiationTimeout) };
        match race(p2p, lan).await {
            Err(e) => assert_eq!(e.to_string(), "protocol error: register rejected with rsp=-1"),
            Ok(_) => panic!("expected the P2P error"),
        }
    }

    /// The P2P attempt times of the tests: the app's 13 s / 12 s / 0.5 s / 3 s scaled down. The tests run on the wall
    /// clock, with loose upper bounds: tokio's paused clock jumps past these timers before a loopback reply lands.
    const SHORT_TIMES: AttemptTimes = AttemptTimes {
        unconfirmed: Duration::from_millis(600),
        confirmed: Duration::from_millis(600),
        gap: Duration::from_millis(100),
        pause: Duration::from_millis(300),
    };

    /// What a fake server heard: when, from where, and the discovery message (`None` for anything else).
    type Heard =
        Arc<std::sync::Mutex<Vec<(std::time::Instant, std::net::SocketAddr, Option<crate::protocol::bcudp::xml::UdpXml>)>>>;

    /// A fake P2P server on `socket` that notes everything it hears and answers what `answer` answers.
    fn fake_server(
        socket: UdpSocket,
        mut answer: impl FnMut(&crate::protocol::bcudp::xml::UdpXml) -> Option<crate::protocol::bcudp::xml::UdpXml>
            + Send
            + 'static,
    ) -> Heard {
        use crate::protocol::bcudp::codec::{read_bcudp, write_bcudp};
        use crate::protocol::bcudp::model::{BcUdp, UdpDiscovery};
        let heard = Heard::default();
        let notes = heard.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while let Ok((n, from)) = socket.recv_from(&mut buf).await {
                let message = match read_bcudp(&buf[..n]) {
                    Ok(Some((BcUdp::Discovery(disc), _))) => Some(disc),
                    _ => None,
                };
                let payload = message.as_ref().map(|disc| disc.payload.clone());
                notes.lock().unwrap().push((std::time::Instant::now(), from, payload));
                if let Some(reply) = message.as_ref().and_then(|disc| answer(&disc.payload).map(|r| (disc.tid, r))) {
                    let reply = BcUdp::Discovery(UdpDiscovery { tid: reply.0, payload: reply.1 });
                    let _ = socket.send_to(&write_bcudp(&reply), from).await;
                }
            }
        });
        heard
    }

    /// The messages `heard` names `pick` gives something for, with when and where from.
    fn picked<T>(
        heard: &Heard,
        pick: impl Fn(&crate::protocol::bcudp::xml::UdpXml) -> Option<T>,
    ) -> Vec<(std::time::Instant, std::net::SocketAddr, T)> {
        let heard = heard.lock().unwrap();
        heard.iter().filter_map(|(at, from, m)| Some((*at, *from, pick(m.as_ref()?)?))).collect()
    }

    /// A lookup server that names `register` and `relay`, and a register server that gives the n-th `C2R_C` (from
    /// 1) sid 1000 + n and the n-th of `relays`, its reply changed by `shape(n, ..)`. Gives back what each heard and
    /// the first registration, made on `socket` under client id 41.
    async fn p2p_servers(
        socket: &UdpSocket,
        relays: Vec<std::net::SocketAddr>,
        mut shape: impl FnMut(usize, &mut crate::protocol::bcudp::xml::R2cCr) + Send + 'static,
    ) -> (Heard, Heard, crate::transport::discovery::Registration) {
        use crate::protocol::bcudp::xml::{IpPort, M2cQr, R2cCr, UdpXml};
        let ip_port = |a: std::net::SocketAddr| IpPort { ip: a.ip().to_string(), port: a.port() };
        let register = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let register_addr = register.local_addr().unwrap();
        let lookup = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let lookup_addr = lookup.local_addr().unwrap();
        let first_relay = relays[0];
        let looked_up = fake_server(lookup, move |m| {
            let UdpXml::C2mQ(_) = m else { return None };
            let reply = M2cQr {
                reg: Some(ip_port(register_addr)),
                relay: Some(ip_port(first_relay)),
                debug: Some("251658240".to_string()),
            };
            Some(UdpXml::M2cQr(reply))
        });
        let mut registrations = 0usize;
        let registered = fake_server(register, move |m| {
            let UdpXml::C2rC(_) = m else { return None };
            let relay = relays[registrations.min(relays.len() - 1)];
            registrations += 1;
            let sid = Some(1000 + registrations as u32);
            let relay = Some(ip_port(relay));
            let mut reply = R2cCr { dev: None, dmap: None, relay, nat: "NULL".into(), sid, rsp: 0 };
            shape(registrations, &mut reply);
            Some(UdpXml::R2cCr(reply))
        });
        let started = tokio::time::Instant::now();
        let found = crate::transport::discovery::resolve_uid(socket, "9527000EXAMPLE01", lookup_addr).await.unwrap();
        let answered = tokio::time::Instant::now();
        let port = socket.local_addr().unwrap().port();
        let offer = crate::transport::discovery::register(socket, "9527000EXAMPLE01", 41, port, &found).await.unwrap();
        let first = crate::transport::discovery::Registration {
            lookup: found,
            register: offer,
            client_id: 41,
            lookup_server: lookup_addr,
            started,
            answered,
            registered: answered,
        };
        (looked_up, registered, first)
    }

    /// `p2p_attempts` on `socket` from `first`, as "admin" / "swordfish", with `SHORT_TIMES`.
    async fn attempts(
        socket: UdpSocket,
        first: crate::transport::discovery::Registration,
    ) -> crate::Result<ReolinkClient> {
        let over = Arc::new(std::sync::atomic::AtomicBool::new(false));
        attempts_with(socket, first, SHORT_TIMES, over).await
    }

    /// `attempts` with these `times` and this `race_over`.
    async fn attempts_with(
        socket: UdpSocket,
        first: crate::transport::discovery::Registration,
        times: AttemptTimes,
        over: Arc<std::sync::atomic::AtomicBool>,
    ) -> crate::Result<ReolinkClient> {
        let login = ("9527000EXAMPLE01".to_string(), "admin".to_string(), "swordfish".to_string());
        ReolinkClient::p2p_attempts(Arc::new(socket), first, login, times, over, None).await
    }

    fn c2r_c(m: &crate::protocol::bcudp::xml::UdpXml) -> Option<crate::protocol::bcudp::xml::C2rC> {
        match m {
            crate::protocol::bcudp::xml::UdpXml::C2rC(c) => Some(c.clone()),
            _ => None,
        }
    }

    fn c2r_disc(m: &crate::protocol::bcudp::xml::UdpXml) -> Option<crate::protocol::bcudp::xml::C2rDisc> {
        match m {
            crate::protocol::bcudp::xml::UdpXml::C2rDisc(c) => Some(c.clone()),
            _ => None,
        }
    }

    fn c2d_disc(m: &crate::protocol::bcudp::xml::UdpXml) -> Option<crate::protocol::bcudp::xml::C2dDisc> {
        match m {
            crate::protocol::bcudp::xml::UdpXml::C2dDisc(c) => Some(c.clone()),
            _ => None,
        }
    }

    /// Asserts three abandon messages, `SHORT_TIMES.gap` apart.
    #[track_caller]
    fn assert_three_rounds<T>(sent: &[(std::time::Instant, std::net::SocketAddr, T)]) {
        assert_eq!(sent.len(), 3, "abandon messages");
        for pair in sent.windows(2) {
            let gap = pair[1].0 - pair[0].0;
            assert!(gap >= Duration::from_millis(80) && gap < Duration::from_millis(600), "{gap:?}");
        }
    }

    /// Like the official app in our captures: an attempt that gets no `D2C_CFM` is abandoned with three
    /// `C2R_DISC` to the register server, and after the pause a new lookup and registration go out from a new
    /// port under the next client id; the new session, with its new sid, logs in.
    #[tokio::test]
    async fn a_silent_attempt_is_abandoned_and_a_fresh_one_logs_in() {
        use crate::protocol::bcudp::xml::UdpXml;
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let answering = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (silent_addr, answering_addr) = (silent.local_addr().unwrap(), answering.local_addr().unwrap());
        let silent_heard = fake_server(silent, |_| None);
        let _device = p2p_device(answering, "relay");
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let first_port = socket.local_addr().unwrap().port();
        let (looked_up, registered, first) = p2p_servers(&socket, vec![silent_addr, answering_addr], |_, _| {}).await;

        let client = attempts(socket, first).await.unwrap();
        assert_eq!(client.peer_addr().await, answering_addr);

        let registrations = picked(&registered, c2r_c);
        assert_eq!(registrations.len(), 2);
        let (first_at, first_from, first_c) = &registrations[0];
        let (second_at, second_from, second_c) = &registrations[1];
        assert_eq!(first_from.port(), first_port);
        assert_ne!(second_from.port(), first_port, "the new attempt's own socket");
        assert_eq!(u32::from(second_c.cli.port), u32::from(second_from.port()));
        assert_eq!((first_c.cid, second_c.cid), (41, 42));

        let abandoned = picked(&registered, c2r_disc);
        assert_three_rounds(&abandoned);
        for (_, from, disc) in &abandoned {
            assert_eq!(*from, *first_from, "from the old socket");
            assert_eq!((disc.sid, disc.cid, disc.rsp, disc.conn.as_str()), (1001, 41, -1, "unknown"));
            assert_eq!((disc.uid.as_str(), disc.debug), ("9527000EXAMPLE01", 251658240));
        }
        let waited = abandoned[0].0 - *first_at;
        assert!(waited >= Duration::from_millis(550) && waited < Duration::from_millis(1500), "{waited:?}");
        assert!(picked(&silent_heard, c2d_disc).is_empty(), "C2D_DISC without a D2C_CFM");

        let lookups = picked(&looked_up, |m| matches!(m, UdpXml::C2mQ(_)).then_some(()));
        assert_eq!(lookups.len(), 2);
        assert_eq!(lookups[1].1, *second_from, "the new lookup from the new socket");
        let paused = lookups[1].0 - abandoned[2].0;
        assert!(paused >= Duration::from_millis(280) && *second_at > lookups[1].0, "{paused:?}");

        let confirmed = picked(&registered, |m| match m {
            UdpXml::C2rCfm(c) => Some(c.sid),
            _ => None,
        });
        assert_eq!(confirmed.iter().map(|c| c.2).collect::<Vec<_>>(), [1002], "the new sid");
    }

    /// An attempt whose relay confirms but never answers the login is abandoned on both ends, three paired rounds:
    /// `C2D_DISC` with the old ids to the relay, `C2R_DISC` with `rsp` 0 and `conn` "relay" to the register server.
    /// The old session sends nothing else after that.
    #[tokio::test]
    async fn an_attempt_silent_after_its_d2c_cfm_is_abandoned_on_both_ends() {
        use crate::protocol::bcudp::xml::{D2cCfm, UdpXml};
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let answering = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (silent_addr, answering_addr) = (silent.local_addr().unwrap(), answering.local_addr().unwrap());
        let silent_heard = fake_server(silent, |m| {
            let UdpXml::C2dT(t) = m else { return None };
            let nc = Some("TESTNONCE".to_string());
            let (sid, conn, cid) = (t.sid, t.conn.clone(), t.cid);
            Some(UdpXml::D2cCfm(D2cCfm { sid, conn, rsp: 0, cid, did: 2, nc, pl: String::new(), lver: 3 }))
        });
        let _device = p2p_device(answering, "relay");
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (_, registered, first) = p2p_servers(&socket, vec![silent_addr, answering_addr], |_, _| {}).await;

        let client = attempts(socket, first).await.unwrap();
        assert_eq!(client.peer_addr().await, answering_addr);

        let to_relay = picked(&silent_heard, c2d_disc);
        assert_three_rounds(&to_relay);
        assert!(to_relay.iter().all(|(_, _, d)| (d.cid, d.did) == (41, 2)));
        let to_register = picked(&registered, c2r_disc);
        assert_three_rounds(&to_register);
        for ((relay_at, _, _), (register_at, _, disc)) in to_relay.iter().zip(&to_register) {
            assert!(register_at >= relay_at && *register_at - *relay_at < Duration::from_millis(80));
            assert_eq!((disc.sid, disc.cid, disc.rsp, disc.conn.as_str()), (1001, 41, 0, "relay"));
            assert!(disc.time.conn >= 550, "{}", disc.time.conn);
            // The unanswered login request, and any resends of it.
            let send = &disc.send;
            assert!(send.cnt >= 1 && send.size > 0 && send.cntr < send.cnt && send.sizer < send.size, "{send:?}");
        }
        let heard = silent_heard.lock().unwrap();
        let after: Vec<_> = heard.iter().filter(|(at, _, _)| *at > to_relay[0].0).collect();
        assert!(after.iter().all(|(_, _, m)| matches!(m, Some(UdpXml::C2dDisc(_)))), "the old session went on");
    }

    /// An attempt that logs in in time is kept: no abandon message, no second registration.
    #[tokio::test]
    async fn an_attempt_that_answers_in_time_is_not_abandoned() {
        let answering = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let answering_addr = answering.local_addr().unwrap();
        let _device = p2p_device(answering, "relay");
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (_, registered, first) = p2p_servers(&socket, vec![answering_addr], |_, _| {}).await;
        let client = attempts(socket, first).await.unwrap();
        assert_eq!(client.peer_addr().await, answering_addr);
        tokio::time::sleep(SHORT_TIMES.unconfirmed + SHORT_TIMES.confirmed).await;
        assert!(picked(&registered, c2r_disc).is_empty());
        assert_eq!(picked(&registered, c2r_c).len(), 1);
    }

    /// Three silent attempts end with `P2pNegotiationTimeout` after three registrations, each abandoned, and no
    /// fourth lookup.
    #[tokio::test]
    async fn three_silent_attempts_end_in_the_error_of_one() {
        use crate::protocol::bcudp::xml::UdpXml;
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent_addr = silent.local_addr().unwrap();
        let _silent_heard = fake_server(silent, |_| None);
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (looked_up, registered, first) = p2p_servers(&socket, vec![silent_addr], |_, _| {}).await;
        let ended = attempts(socket, first).await;
        assert!(matches!(ended, Err(Error::P2pNegotiationTimeout)), "{:?}", ended.err());
        tokio::time::sleep(SHORT_TIMES.pause * 2).await;
        let registrations = picked(&registered, c2r_c);
        assert_eq!(registrations.iter().map(|r| r.2.cid).collect::<Vec<_>>(), [41, 42, 43]);
        assert_eq!(picked(&registered, |m| c2r_disc(m).map(|d| d.sid)).iter().map(|d| d.2).collect::<Vec<_>>(), [
            1001, 1001, 1001, 1002, 1002, 1002, 1003, 1003, 1003
        ]);
        assert_eq!(picked(&looked_up, |m| matches!(m, UdpXml::C2mQ(_)).then_some(())).len(), 3);
    }

    /// A login won by another leg stops the P2P attempts: the silent one is abandoned, and no new registration
    /// follows.
    #[tokio::test]
    async fn another_leg_s_login_stops_the_p2p_attempts() {
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent_addr = silent.local_addr().unwrap();
        let _silent_heard = fake_server(silent, |_| None);
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (_, registered, first) = p2p_servers(&socket, vec![silent_addr], |_, _| {}).await;
        let (tcp_addr, fake_tcp) = tcp_camera().await;
        let login = ("9527000EXAMPLE01".to_string(), "admin".to_string(), "swordfish".to_string());
        let mut legs = ReolinkClient::p2p_session_legs(Arc::new(socket), first, login, tcp_addr.port(), SHORT_TIMES);
        // The register reply named no device: the TCP leg of the test stands in for it.
        legs.push(leg(LegKind::P2pTcp, ReolinkClient::connect_by_ip(tcp_addr.ip(), tcp_addr.port())));
        let client = race(async move { Ok(legs) }, std::future::pending()).await.unwrap();
        assert_eq!(client.peer_addr().await, tcp_addr);
        fake_tcp.await.unwrap();
        tokio::time::sleep(SHORT_TIMES.unconfirmed + SHORT_TIMES.gap * 3 + SHORT_TIMES.pause * 3).await;
        assert_eq!(picked(&registered, c2r_disc).len(), 3, "the silent attempt is still abandoned");
        assert_eq!(picked(&registered, c2r_c).len(), 1, "no registration after the race was won");
    }

    /// A login won while a retry looks up and registers stops that retry before its first `C2D_T`: its new session
    /// is abandoned at once (three `C2R_DISC`), and no further registration follows.
    #[tokio::test]
    async fn a_win_during_a_retry_registration_starts_no_new_session() {
        use crate::protocol::bcudp::xml::UdpXml;
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent_addr = silent.local_addr().unwrap();
        let silent_heard = fake_server(silent, |_| None);
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let over = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let won = over.clone();
        let (_, registered, first) = p2p_servers(&socket, vec![silent_addr], move |n, _| {
            if n == 2 {
                won.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        })
        .await;
        let ended = attempts_with(socket, first, SHORT_TIMES, over).await;
        assert!(matches!(ended, Err(Error::P2pNegotiationTimeout)), "{:?}", ended.err());
        // The last abandon round is still on its way to the fake server.
        tokio::time::sleep(SHORT_TIMES.pause * 2).await;
        let registrations = picked(&registered, c2r_c);
        assert_eq!(registrations.len(), 2, "no registration after the second");
        let new_port = registrations[1].1;
        let c2d_t = picked(&silent_heard, |m| matches!(m, UdpXml::C2dT(_)).then_some(()));
        assert!(c2d_t.iter().all(|(_, from, _)| *from != new_port), "a C2D_T of the new session");
        let abandoned = picked(&registered, |m| c2r_disc(m).map(|d| d.sid));
        assert_eq!(abandoned.iter().map(|d| d.2).collect::<Vec<_>>(), [1001, 1001, 1001, 1002, 1002, 1002]);
    }

    /// A retry's registration answered with the retired session's sid is not taken: the attempt waits for the
    /// answer to the resent `C2R_C` and runs, and logs in, under that fresh sid.
    #[tokio::test]
    async fn a_retry_never_takes_the_retired_sid() {
        use crate::protocol::bcudp::xml::UdpXml;
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let answering = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (silent_addr, answering_addr) = (silent.local_addr().unwrap(), answering.local_addr().unwrap());
        let _silent_heard = fake_server(silent, |_| None);
        let _device = p2p_device(answering, "relay");
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (_, registered, first) = p2p_servers(&socket, vec![silent_addr, answering_addr], |n, reply| {
            if n == 2 {
                reply.sid = Some(1001);
            }
        })
        .await;
        // Room for the `C2R_C` resend, which comes `RESEND_INTERVAL` after the first.
        let times = AttemptTimes { unconfirmed: Duration::from_millis(1500), ..SHORT_TIMES };
        let over = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let client = attempts_with(socket, first, times, over).await.unwrap();
        assert_eq!(client.peer_addr().await, answering_addr);
        let confirmed = picked(&registered, |m| match m {
            UdpXml::C2rCfm(c) => Some(c.sid),
            _ => None,
        });
        assert_eq!(confirmed.iter().map(|c| c.2).collect::<Vec<_>>(), [1003], "the fresh sid");
    }

    /// After a retry's refused registration the race waits for the LAN discovery, as after the first
    /// registration's refusal: the answer that comes later still logs in over LAN TCP.
    #[tokio::test]
    async fn after_a_retry_s_refused_registration_the_race_waits_for_the_lan() {
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent_addr = silent.local_addr().unwrap();
        let _silent_heard = fake_server(silent, |_| None);
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (_, registered, first) = p2p_servers(&socket, vec![silent_addr], |n, reply| {
            if n == 2 {
                reply.rsp = -1;
            }
        })
        .await;
        let (tcp_addr, fake_tcp) = tcp_camera().await;
        let login = ("9527000EXAMPLE01".to_string(), "admin".to_string(), "swordfish".to_string());
        let legs = ReolinkClient::p2p_session_legs(Arc::new(socket), first, login, 9, SHORT_TIMES);
        let lan = vec![leg(LegKind::LanTcp, ReolinkClient::connect_by_ip(tcp_addr.ip(), tcp_addr.port()))];
        let lan = async move {
            // Past the first attempt's abandon and the retry's refusal (about 1.2 s with `SHORT_TIMES`).
            tokio::time::sleep(Duration::from_secs(3)).await;
            Ok(lan)
        };
        let client = race(async move { Ok(legs) }, lan).await.unwrap();
        assert_eq!(client.peer_addr().await, tcp_addr);
        fake_tcp.await.unwrap();
        assert_eq!(picked(&registered, c2r_c).len(), 2, "the refusal came from the retry");
    }

    /// A dropped client on a direct UDP path sends nothing more: neither
    /// `C2D_HB` nor (its connection gone too) an ack.
    #[tokio::test]
    async fn a_dropped_direct_client_stops_its_keepalive() {
        use crate::protocol::bcudp::codec::read_bcudp;
        use crate::protocol::bcudp::model::BcUdp;
        use crate::protocol::bcudp::xml::UdpXml;
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let device = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = device.local_addr().unwrap();
        let peer = PeerHandle { addr, local_connection_id: 1, remote_connection_id: 2, nonce: None, is_direct: true };
        let client = ReolinkClient::from_udp(socket, peer, "local");
        let mut buf = [0u8; 2048];
        let heartbeat = async {
            loop {
                let (n, _) = device.recv_from(&mut buf).await.unwrap();
                if let Ok(Some((BcUdp::Discovery(disc), _))) = read_bcudp(&buf[..n]) {
                    if matches!(disc.payload, UdpXml::C2dHb(_)) {
                        break;
                    }
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(3), heartbeat).await.expect("a C2D_HB");
        drop(client);
        while tokio::time::timeout(Duration::from_millis(100), device.recv_from(&mut buf)).await.is_ok() {}
        let quiet = tokio::time::timeout(Duration::from_millis(1500), device.recv_from(&mut buf)).await;
        assert!(quiet.is_err(), "still sending after the drop");
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
