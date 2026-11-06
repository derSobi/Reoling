use crate::ui::download_job::DownloadJob;
use crate::ui::playback_driver::{PlaybackCommand, PlaybackDriver, PLAYBACK_TICK};
use crate::ui::video_view::VideoSink;
use reoling::{
    ChannelInfo, CivilTime, DeviceIdentity, DeviceKind, DeviceUpdate, RecordStream, ReolinkClient, ReplaySpeed,
    SettingsGates, SettingsPage, SettingsTarget, SettingsWrite, StreamProfile, VideoFrame, VideoType, WriteOutcome,
};
use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio_stream::StreamExt;

pub use crate::ui::download_job::{DownloadEvent, DownloadOutcome, DownloadRequest};
pub use crate::ui::playback_driver::{FileSpan, PlaybackEvent, PlayerPhase, PlayerState};

/// What a device's connection tells the UI.
pub enum DeviceEvent {
    /// Logged in; carries the device's own name and model.
    Connected(DeviceIdentity),
    /// The device (NVR / Home Hub) described its channels.
    Channels(Vec<ChannelInfo>),
    /// The device's answer to a siren or spotlight command (`code` 200 is
    /// success).
    ControlReply { msg_id: u32, code: u16 },
    /// A control command could not even be sent.
    ControlFailed(String),
    /// What each channel's camera can do.
    Abilities(Vec<(u8, reoling::ChannelAbilities)>),
    /// A camera's zoom and focus as (min, max, current).
    ZoomFocus { channel_id: u8, zoom: Option<(u32, u32, u32)>, focus: Option<(u32, u32, u32)> },
    /// The camera's saved presets.
    Presets { channel_id: u8, presets: Vec<reoling::PtzPreset> },
    /// The camera's Monitor Point (PTZ Guard) state.
    MonitorPoint { channel_id: u8, state: reoling::MonitorPoint },
    /// Monitor Point's saved thumbnail (JPEG bytes), fully reassembled.
    MonitorPointImage { channel_id: u8, jpeg: Vec<u8> },
    /// A preset's saved thumbnail (JPEG bytes), fully reassembled.
    PresetImage { channel_id: u8, preset_id: u8, jpeg: Vec<u8> },
    ImageNotFound { preset_id: Option<u8> },
    Snapshot { preset_id: u8, jpeg: Vec<u8> },
    /// A channel's name, asked for because the channel list had none.
    ChannelName { channel_id: u8, name: String },
    /// The requested stream's first frame reached GStreamer.
    Playing,
    /// The stream of `channel` could not be started or broke off. The
    /// device itself is still connected.
    PlayFailed { channel: u8, reason: String },
    /// The requested stream waits for the device's download: no failure,
    /// it starts once the download is over.
    PlayDeferred(String),
    /// The encoder values the device allows (from its login reply).
    EncodeProfiles(Vec<reoling::encoding::EncodeProfile>),
    /// A channel's encoder settings (`None`: the device refused to say).
    Encoding { channel_id: u8, encoding: Option<reoling::encoding::Encoding> },
    /// The encoder values one channel allows, asked for separately.
    ChannelEncodeProfiles { channel_id: u8, profiles: Vec<reoling::encoding::EncodeProfile> },
    /// The camera on a channel: its name, model and firmware.
    ChannelVersion { channel_id: u8, model: Option<String>, firmware: Option<String> },
    /// The device refused the login because the user name or password is
    /// wrong. Any other refusal comes as `Lost`.
    LoginRejected,
    /// The device could not be reached, or the connection
    /// died. Nothing more will come.
    Lost(String),
    /// Recorded-video playback (never sent for live video).
    Playback(PlaybackEvent),
    /// A recording download (see `download_job`).
    Download(DownloadEvent),
    /// One settings page, complete, with the number its read was sent under
    /// (answer to `query_settings`).
    Settings { page: SettingsPage, read: u64 },
    /// A download runs; the settings page is read (or written) once it is
    /// over.
    SettingsDeferred(SettingsTarget),
    /// The device's answers to one group of settings writes, with the
    /// number the group was sent under (answer to `write_settings`).
    SettingsWritten { target: SettingsTarget, group: u64, results: Vec<WriteOutcome> },
}

enum Command {
    Play { channel: u8, profile: StreamProfile },
    Siren { channel: u8 },
    Spotlight { channel: u8, on: bool },
    Ptz { channel: u8, command: &'static str, speed: u8 },
    QueryZoomFocus { channel: u8 },
    Zoom { channel: u8, position: u32 },
    Focus { channel: u8, position: u32 },
    QueryPresets { channel: u8 },
    SetPreset { channel: u8, id: u8, name: String },
    GotoPreset { channel: u8, id: u8 },
    DeletePreset { channel: u8, id: u8 },
    TalkStart { channel: u8 },
    TalkBlock { channel: u8, block: Vec<u8> },
    TalkStop { channel: u8 },
    Calibrate { channel: u8 },
    QueryMonitorPoint { channel: u8 },
    QueryMonitorPointImage { channel: u8 },
    QueryPresetImage { channel: u8, preset_id: u8 },
    CapturePresetImage { channel: u8, preset_id: u8 },
    UploadPresetImage { channel: u8, preset_id: u8, jpeg: Vec<u8> },
    SavePresetWithImage { channel: u8, id: u8, name: String },
    ModifyPreset { channel: u8, id: u8, name: String, with_image: bool },
    SetMonitorPointConfig { channel: u8, enabled: bool, timeout_seconds: u32 },
    SetMonitorPointHere { channel: u8, enabled: bool, timeout_seconds: u32 },
    GoToMonitorPoint { channel: u8, timeout_seconds: u32 },
    QueryEncoding { channel: u8 },
    QueryEncodeProfiles { channel: u8 },
    QueryChannelVersion { channel: u8 },
    SetEncoding { channel: u8, body: String },
    QuerySettings { target: SettingsTarget, gates: SettingsGates, read: u64 },
    WriteSettings { target: SettingsTarget, kind: DeviceKind, group: u64, writes: Vec<SettingsWrite> },
    Playback(PlaybackCommand),
    Download(DownloadRequest),
    DownloadCancel,
    Stop,
    Shutdown,
}

/// What the command branch of the device loop got: a command whose turn
/// has come after a download, or one from the link.
enum Incoming {
    Deferred,
    Received(Option<Command>),
}

impl Incoming {
    fn command(self, deferred: &mut VecDeque<Command>) -> Option<Command> {
        match self {
            Incoming::Deferred => deferred.pop_front(),
            Incoming::Received(command) => command,
        }
    }
}

/// Tells the user why `command` waits for the download: live video says
/// so on its page; on the Playback page a day change always says so, and
/// the first other playback command of a download.
async fn deferral_notice(
    command: &Command,
    job: &mut DownloadJob,
    driver: &PlaybackDriver,
    deferred: &VecDeque<Command>,
    tx: &async_channel::Sender<DeviceEvent>,
) {
    match command {
        Command::Play { .. } => {
            let _ = tx.send(DeviceEvent::PlayDeferred("Live video starts after the download".to_string())).await;
        }
        Command::Playback(PlaybackCommand::Close { .. }) | Command::Stop => {}
        Command::Playback(playback) => {
            let day = matches!(playback, PlaybackCommand::Day { .. });
            if !day && !job.first_wait() {
                return;
            }
            // The page's session: the one of the newest day change that
            // waits, else the driver's.
            let session = match playback {
                PlaybackCommand::Day { session, .. } => *session,
                _ => deferred
                    .iter()
                    .rev()
                    .find_map(|c| match c {
                        Command::Playback(PlaybackCommand::Day { session, .. }) => Some(*session),
                        _ => None,
                    })
                    .unwrap_or_else(|| driver.session()),
            };
            let event = PlaybackEvent::Notice { session, message: "Playback continues after the download".to_string() };
            let _ = tx.send(DeviceEvent::Playback(event)).await;
        }
        Command::QuerySettings { target, .. } | Command::WriteSettings { target, .. } => {
            let _ = tx.send(DeviceEvent::SettingsDeferred(*target)).await;
        }
        _ => {}
    }
}

/// The UI's handle on one device's connection (which lives on its own
/// thread). Dropping it disconnects.
pub struct DeviceLink {
    pub events: async_channel::Receiver<DeviceEvent>,
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    /// Bumped by every playback command except `Month`: lets the device
    /// thread skip the device work of commands a newer one supersedes (only
    /// the newest opens anything; every intent is still recorded).
    playback_gen: Arc<AtomicU64>,
}

/// Recorded-video playback. Every command first stops live video on the
/// device; live `play` ends the playback. The answers come as
/// `DeviceEvent::Playback`. Times are device-local, as the device lists
/// them.
impl DeviceLink {
    fn next_playback_gen(&self) -> u64 {
        self.playback_gen.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn send_playback(&self, command: PlaybackCommand) {
        let _ = self.commands.send(Command::Playback(command));
    }

    /// Asks for the recorded days of a month (answered as `Month`).
    pub fn playback_month(&self, channel: u8, year: u16, month: u8) {
        self.send_playback(PlaybackCommand::Month { channel, year, month });
    }

    /// Starts a session: lists the day's files for `stream` (answered as
    /// `Day`); nothing plays.
    pub fn playback_day(&self, session: u64, channel: u8, day: CivilTime, stream: RecordStream) {
        let gen = self.next_playback_gen();
        self.send_playback(PlaybackCommand::Day { gen, session, channel, day, stream });
    }

    /// Plays from `at` (device-local). Returns the command's generation:
    /// `PlayerState::gen` reaches it once the device thread has taken the
    /// command up (the same for Play and Pause).
    pub fn playback_seek(&self, at: CivilTime) -> u64 {
        let gen = self.next_playback_gen();
        self.send_playback(PlaybackCommand::Seek { gen, at });
        gen
    }

    pub fn playback_play(&self) -> u64 {
        let gen = self.next_playback_gen();
        self.send_playback(PlaybackCommand::Play { gen });
        gen
    }

    pub fn playback_pause(&self) -> u64 {
        let gen = self.next_playback_gen();
        self.send_playback(PlaybackCommand::Pause { gen });
        gen
    }

    pub fn playback_speed(&self, speed: ReplaySpeed) {
        let gen = self.next_playback_gen();
        self.send_playback(PlaybackCommand::Speed { gen, speed });
    }

    /// Main or sub stream: other files, so the day is listed again (and a
    /// `Day` for that stream follows).
    pub fn playback_stream(&self, stream: RecordStream) {
        let gen = self.next_playback_gen();
        self.send_playback(PlaybackCommand::Stream { gen, stream });
    }

    /// Ends the session: closes the playback on the device, drops the sink.
    pub fn playback_close(&self) {
        let gen = self.next_playback_gen();
        self.send_playback(PlaybackCommand::Close { gen });
    }

    /// Downloads a range of recordings to a file (see `download_job`):
    /// the running playback is paused first, and playback (or live video)
    /// asked for meanwhile waits until the download is over. Answered as
    /// `Download` events.
    pub fn download(&self, request: DownloadRequest) {
        let _ = self.commands.send(Command::Download(request));
    }

    /// Stops the running download; its partial file is removed.
    pub fn download_cancel(&self) {
        let _ = self.commands.send(Command::DownloadCancel);
    }
}

impl DeviceLink {
    /// Asks for a channel's encoder settings (answered as `Encoding`).
    pub fn query_encoding(&self, channel: u8) {
        let _ = self.commands.send(Command::QueryEncoding { channel });
    }

    /// Asks for the encoder values one channel allows (answered as
    /// `ChannelEncodeProfiles`).
    pub fn query_encode_profiles(&self, channel: u8) {
        let _ = self.commands.send(Command::QueryEncodeProfiles { channel });
    }

    /// Asks which camera sits on a channel (answered as `ChannelVersion`).
    pub fn query_channel_version(&self, channel: u8) {
        let _ = self.commands.send(Command::QueryChannelVersion { channel });
    }

    /// Writes a channel's encoder settings (answered as a `ControlReply`
    /// for message 57).
    pub fn set_encoding(&self, channel: u8, body: String) {
        let _ = self.commands.send(Command::SetEncoding { channel, body });
    }

    /// Reads one settings page (answered as `Settings` with the same `read`,
    /// the caller's number for this read; `SettingsDeferred` first when a
    /// download has the connection).
    pub fn query_settings(&self, target: SettingsTarget, gates: SettingsGates, read: u64) {
        let _ = self.commands.send(Command::QuerySettings { target, gates, read });
    }

    /// Writes one group of settings on a device of `kind` (answered as
    /// `SettingsWritten` with the same `group`; `SettingsDeferred` first
    /// when a download has the connection, the writes going out once it is
    /// over).
    pub fn write_settings(&self, target: SettingsTarget, kind: DeviceKind, group: u64, writes: Vec<SettingsWrite>) {
        let _ = self.commands.send(Command::WriteSettings { target, kind, group, writes });
    }

    /// Streams the channel (replacing whatever this device was streaming).
    pub fn play(&self, channel: u8, profile: StreamProfile) {
        let _ = self.commands.send(Command::Play { channel, profile });
    }

    /// Plays the camera's siren once.
    pub fn siren(&self, channel: u8) {
        let _ = self.commands.send(Command::Siren { channel });
    }

    /// Spotlight on or off.
    pub fn spotlight(&self, channel: u8, on: bool) {
        let _ = self.commands.send(Command::Spotlight { channel, on });
    }

    /// Pan/tilt: moves until a `stop` (speed 0) follows.
    pub fn ptz(&self, channel: u8, command: &'static str, speed: u8) {
        let _ = self.commands.send(Command::Ptz { channel, command, speed });
    }

    /// Asks where the zoom is and how far it goes (answered as `ZoomFocus`).
    pub fn query_zoom_focus(&self, channel: u8) {
        let _ = self.commands.send(Command::QueryZoomFocus { channel });
    }

    pub fn set_focus(&self, channel: u8, position: u32) {
        let _ = self.commands.send(Command::Focus { channel, position });
    }

    pub fn set_zoom(&self, channel: u8, position: u32) {
        let _ = self.commands.send(Command::Zoom { channel, position });
    }

    pub fn query_presets(&self, channel: u8) {
        let _ = self.commands.send(Command::QueryPresets { channel });
    }

    pub fn set_preset(&self, channel: u8, id: u8, name: String) {
        let _ = self.commands.send(Command::SetPreset { channel, id, name });
    }

    pub fn goto_preset(&self, channel: u8, id: u8) {
        let _ = self.commands.send(Command::GotoPreset { channel, id });
    }

    pub fn delete_preset(&self, channel: u8, id: u8) {
        let _ = self.commands.send(Command::DeletePreset { channel, id });
    }

    /// Re-calibrates the pan/tilt mechanism.
    pub fn calibrate(&self, channel: u8) {
        let _ = self.commands.send(Command::Calibrate { channel });
    }

    /// Asks for Monitor Point's current state (answered as `MonitorPoint`).
    pub fn query_monitor_point(&self, channel: u8) {
        let _ = self.commands.send(Command::QueryMonitorPoint { channel });
    }

    /// Asks for Monitor Point's saved thumbnail (answered as
    /// `MonitorPointImage`).
    pub fn query_monitor_point_image(&self, channel: u8) {
        let _ = self.commands.send(Command::QueryMonitorPointImage { channel });
    }

    /// Asks for a preset's saved thumbnail (answered as `PresetImage`).
    /// Takes a fresh live picture for a preset (answered as `Snapshot`).
    pub fn capture_preset_image(&self, channel: u8, preset_id: u8) {
        let _ = self.commands.send(Command::CapturePresetImage { channel, preset_id });
    }

    pub fn upload_preset_image(&self, channel: u8, preset_id: u8, jpeg: Vec<u8>) {
        let _ = self.commands.send(Command::UploadPresetImage { channel, preset_id, jpeg });
    }

    pub fn save_preset_with_image(&self, channel: u8, id: u8, name: String) {
        let _ = self.commands.send(Command::SavePresetWithImage { channel, id, name });
    }

    pub fn modify_preset(&self, channel: u8, id: u8, name: String, with_image: bool) {
        let _ = self.commands.send(Command::ModifyPreset { channel, id, name, with_image });
    }

    pub fn query_preset_image(&self, channel: u8, preset_id: u8) {
        let _ = self.commands.send(Command::QueryPresetImage { channel, preset_id });
    }

    /// Auto Return on/off and its timeout, without moving the point.
    pub fn set_monitor_point_config(&self, channel: u8, enabled: bool, timeout_seconds: u32) {
        let _ = self.commands.send(Command::SetMonitorPointConfig { channel, enabled, timeout_seconds });
    }

    /// Saves the camera's current position as Monitor Point.
    pub fn set_monitor_point_here(&self, channel: u8, enabled: bool, timeout_seconds: u32) {
        let _ = self.commands.send(Command::SetMonitorPointHere { channel, enabled, timeout_seconds });
    }

    /// Moves the camera to Monitor Point now.
    pub fn go_to_monitor_point(&self, channel: u8, timeout_seconds: u32) {
        let _ = self.commands.send(Command::GoToMonitorPoint { channel, timeout_seconds });
    }

    /// Opens a talk session; the camera's answer comes as a `ControlReply`
    /// for message 201.
    pub fn talk_start(&self, channel: u8) {
        let _ = self.commands.send(Command::TalkStart { channel });
    }

    pub fn talk_stop(&self, channel: u8) {
        let _ = self.commands.send(Command::TalkStop { channel });
    }

    /// Where the microphone's blocks go: usable from any thread.
    pub fn talk_sink(&self, channel: u8) -> Box<dyn Fn(Vec<u8>) + Send> {
        let commands = self.commands.clone();
        Box::new(move |block| {
            let _ = commands.send(Command::TalkBlock { channel, block });
        })
    }

    /// Stops the stream; the connection stays.
    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }

    /// Tells the device we are leaving.
    pub fn shutdown(&self) {
        let _ = self.commands.send(Command::Shutdown);
    }
}

impl Drop for DeviceLink {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One connection's request for a `VideoSink`, sent to `main.rs`'s
/// GTK-main-thread responder task the first time a connection knows its
/// codec (from the first video frame) — see `spawn_connection`'s doc
/// comment. Answered with the sink, or with why this system cannot build
/// one (see `requirements`).
pub type SinkRequest = (VideoType, tokio::sync::oneshot::Sender<Result<VideoSink, String>>);

/// A device thread's request for a replay `VideoSink`: as `SinkRequest`,
/// with the playback session whose footage it is for. The window serves only
/// the Playback page's current session; a request for any other (frames of a
/// day, device or page left meanwhile) gets no answer, and that frame is
/// dropped.
pub type ReplaySinkRequest = (u64, VideoType, tokio::sync::oneshot::Sender<Result<VideoSink, String>>);

/// What a connection does with the live frame that made it ask for a sink
/// (see `request_sink`).
enum SinkAnswer {
    /// Pushes it into this sink, and every frame of the stream after it.
    Built(VideoSink),
    /// This system cannot show the stream (see `requirements`): the stream
    /// stops, the connection stays (PTZ, pushes, Playback, another stream),
    /// and the window says why.
    Refused(String),
    /// The window is gone: the connection ends.
    Gone,
}

/// Asks the window for a live sink for `video_type`.
async fn request_sink(requests: &tokio::sync::mpsc::Sender<SinkRequest>, video_type: VideoType) -> SinkAnswer {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if requests.send((video_type, reply_tx)).await.is_err() {
        return SinkAnswer::Gone;
    }
    match reply_rx.await {
        Ok(Ok(sink)) => SinkAnswer::Built(sink),
        Ok(Err(reason)) => SinkAnswer::Refused(reason),
        Err(_) => SinkAnswer::Gone,
    }
}

/// A connection's live stream, as `on_live_frame` finds and leaves it: the
/// device's frames (`None`: no stream), the sink they go into (asked for
/// with the stream's first frame) and whether `Playing` was said.
struct LiveStream<'a, Frames> {
    frames: &'a mut Option<Frames>,
    sink: &'a mut Option<VideoSink>,
    announced: &'a mut bool,
}

/// What the connection does after a live frame (see `on_live_frame`).
#[derive(Debug, PartialEq, Eq)]
enum AfterFrame {
    /// Goes on with the stream: the frame went into its sink.
    Shown,
    /// Goes on without the stream, which this system cannot show.
    Stopped,
    /// Ends: the window is gone.
    End,
}

/// One live frame of the stream on `channel`: the stream's first asks the
/// window for a sink (`request_sink`), the frame goes into it, and the first
/// one in says `Playing`. A refused sink stops the stream: its frames are let
/// go first, then `stop_video` tells the device, and the window hears why
/// (`PlayFailed`); the connection goes on (PTZ, pushes, Playback, another
/// stream).
async fn on_live_frame<Frames, Stop: std::future::Future>(
    frame: &VideoFrame,
    channel: u8,
    live: LiveStream<'_, Frames>,
    requests: &tokio::sync::mpsc::Sender<SinkRequest>,
    tx: &async_channel::Sender<DeviceEvent>,
    stop_video: impl FnOnce() -> Stop,
) -> AfterFrame {
    if live.sink.is_none() {
        match request_sink(requests, frame.video_type).await {
            SinkAnswer::Built(built) => *live.sink = Some(built),
            SinkAnswer::Refused(reason) => {
                *live.frames = None;
                let _ = stop_video().await;
                let _ = tx.send(DeviceEvent::PlayFailed { channel, reason }).await;
                return AfterFrame::Stopped;
            }
            SinkAnswer::Gone => return AfterFrame::End, // GTK side gone
        }
    }
    live.sink.as_ref().expect("just built").push_frame(frame);
    if !*live.announced {
        *live.announced = true;
        if tx.send(DeviceEvent::Playing).await.is_err() {
            return AfterFrame::End;
        }
    }
    AfterFrame::Shown
}

/// How the user chose to reach the device — set by the explicit UID/IP
/// toggle in the connect dialog, never inferred or auto-detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectTarget {
    Uid(String),
    Ip { addr: IpAddr, port: u16 },
}

/// Which transport a UID connection should try. `Udp` (plain UDP/P2P, no
/// TCP fallback or preference) is the default and the only path the real
/// app targets — the official Reolink client never uses TCP for the BC
/// protocol. `PreferTcp` is diagnostic-only, opt-in via `main.rs`'s
/// `--prefer-tcp` flag; not exposed in the connect dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UidTransport {
    PreferTcp,
    Udp,
}

/// What the UI hears of a refused login: `LoginRejected` (the saved password
/// is forgotten) only for a wrong user name or password, `Lost` with the
/// device's reason, the password kept, for every other code.
fn login_refused(code: u16) -> DeviceEvent {
    let refusal = reoling::LoginRefusal::from_code(code);
    if refusal.forgets_saved_password() {
        DeviceEvent::LoginRejected
    } else {
        DeviceEvent::Lost(refusal.reason())
    }
}

/// Connects and logs in to a device on a dedicated thread with its own
/// tokio runtime, and keeps the connection up: it listens for pushes (the
/// channel list) and streams a channel on request. GTK is never touched from
/// there — everything reaches the UI through `DeviceLink::events`.
///
/// **Video frames bypass the GTK main loop entirely** — see
/// `.plans/reoling-baichuan-p2p-audit-2026-09-16.md` section 34. Routing
/// every frame through a `glib` task coupled live video to whatever else the
/// GTK thread was doing. `gstreamer::Pipeline`/`AppSrc` are thread-safe, so
/// this thread asks `main_window`'s responder (via `sink_request_tx`) for a
/// `VideoSink` once per stream, when the first frame shows its codec, then
/// pushes every frame itself. The UI only hears `Playing`.
///
/// Recorded footage goes the same way to its own view, through
/// `replay_sink_request_tx`; the playback logic runs here too, in a
/// `PlaybackDriver`, as the replay sink must be driven from the thread
/// that pushes its frames.
pub fn spawn_device(
    target: ConnectTarget,
    username: String,
    password: String,
    uid_transport: UidTransport,
    sink_request_tx: tokio::sync::mpsc::Sender<SinkRequest>,
    replay_sink_request_tx: tokio::sync::mpsc::Sender<ReplaySinkRequest>,
    audio_out: std::sync::Arc<crate::ui::audio::AudioOutput>,
) -> DeviceLink {
    let (tx, rx) = async_channel::unbounded();
    let (commands, mut command_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
    let playback_gen = Arc::new(AtomicU64::new(0));
    let driver_gen = Arc::clone(&playback_gen);

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        runtime.block_on(async move {
            let connect_result = match target {
                // `Udp` is the plain UDP/P2P session, the only path the real
                // app targets. `PreferTcp` is diagnostic-only
                // (`--prefer-tcp`, see `main.rs`); the official Reolink app
                // never uses TCP for the BC protocol.
                ConnectTarget::Uid(uid) => match uid_transport {
                    UidTransport::PreferTcp => ReolinkClient::connect_by_uid_prefer_tcp(&uid).await,
                    UidTransport::Udp => ReolinkClient::connect_by_uid(&uid).await,
                },
                ConnectTarget::Ip { addr, port } => ReolinkClient::connect_by_ip(addr, port).await,
            };
            let mut client = match connect_result {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(DeviceEvent::Lost(e.to_string())).await;
                    return;
                }
            };
            match client.login(&username, &password).await {
                Ok(_) => {}
                Err(reoling::Error::LoginFailed { code }) => {
                    eprintln!("login refused by the device: code {code}");
                    let _ = tx.send(login_refused(code)).await;
                    return;
                }
                Err(e) => {
                    let _ = tx.send(DeviceEvent::Lost(e.to_string())).await;
                    return;
                }
            }
            let mut channel_updates =
                client.take_channel_updates().expect("taken once per client");
            let mut audio = client.take_audio().expect("taken once per client");
            let identity = client.identity().await;
            if std::env::var("REOLING_PROBE_PUSH").is_ok() {
                client.probe_pushes(&username).await;
            }
            // Diagnostic-only: a scripted download session (see
            // `download_probe`), only with its variable set.
            if let Some(channel_id) = crate::ui::download_probe::probe_channel() {
                crate::ui::download_probe::run(&mut client, channel_id, &identity).await;
            }
            // Diagnostic-only: every settings page, read once (see
            // `client::settings::probe_settings`).
            if let Some(channel_id) = reoling::settings_probe_channel() {
                client.probe_settings(channel_id, &identity, &username).await;
            }
            let _ = tx.send(DeviceEvent::Connected(identity)).await;
            let _ = tx.send(DeviceEvent::EncodeProfiles(client.encode_profiles())).await;

            let mut frames: Option<tokio_stream::wrappers::ReceiverStream<reoling::Result<VideoFrame>>> =
                None;
            let mut sink: Option<VideoSink> = None;
            let mut announced = false;
            let mut channel = 0u8;
            let mut asked_names: std::collections::HashSet<u8> = std::collections::HashSet::new();
            let mut driver = PlaybackDriver::new(driver_gen, replay_sink_request_tx);
            let mut playback_tick = tokio::time::interval(PLAYBACK_TICK);
            playback_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // A running download, and the commands that wait for it: live
            // video and playback need the connection's reader, which the
            // download has (as a search would). They run, in order, once
            // it is over.
            let mut download: Option<DownloadJob> = None;
            let mut deferred: VecDeque<Command> = VecDeque::new();

            loop {
                let replay_active = driver.is_active();
                let downloading = download.is_some();
                let has_deferred = !downloading && !deferred.is_empty();
                tokio::select! {
                    incoming = async {
                        if has_deferred { Incoming::Deferred } else { Incoming::Received(command_rx.recv().await) }
                    } => match incoming.command(&mut deferred) {
                        Some(
                            command @ (Command::Play { .. }
                            | Command::Stop
                            | Command::Playback(_)
                            | Command::QuerySettings { .. }
                            | Command::WriteSettings { .. }),
                        ) if downloading =>
                        {
                            if let Some(job) = download.as_mut() {
                                deferral_notice(&command, job, &driver, &deferred, &tx).await;
                            }
                            deferred.push_back(command);
                        }
                        Some(Command::Download(request)) => {
                            if download.is_some() {
                                let outcome =
                                    DownloadOutcome::Failed("A download is already running on this device".to_string());
                                let event = DownloadEvent::Finished { session: request.session, outcome };
                                let _ = tx.send(DeviceEvent::Download(event)).await;
                            } else {
                                // Live video and a download never share
                                // the connection's reader.
                                if frames.take().is_some() {
                                    let _ = client.stop_video(channel).await;
                                    sink = None;
                                    announced = false;
                                }
                                download = DownloadJob::start(request, &mut driver, &mut client, &tx).await;
                            }
                        }
                        Some(Command::DownloadCancel) => {
                            if let Some(job) = download.as_ref() {
                                job.cancel();
                            }
                        }
                        Some(Command::Play { channel: wanted, profile }) => {
                            // Live and playback never share the connection.
                            // Nothing happens here while no playback runs.
                            driver.end(&mut client).await;
                            if frames.take().is_some() {
                                let _ = client.stop_video(channel).await;
                            }
                            channel = wanted;
                            sink = None;
                            announced = false;
                            match client.start_video(channel, profile).await {
                                Ok(f) => frames = Some(f),
                                Err(e) => {
                                    let event = DeviceEvent::PlayFailed { channel, reason: e.to_string() };
                                    let _ = tx.send(event).await;
                                }
                            }
                        }
                        Some(Command::QueryEncoding { channel }) => client.query_encoding(channel).await,
                        Some(Command::QueryEncodeProfiles { channel }) => client.query_encode_profiles(channel).await,
                        Some(Command::QueryChannelVersion { channel }) => client.query_channel_version(channel).await,
                        Some(Command::QuerySettings { target, gates, read }) => {
                            client.query_settings(target, &gates, read).await
                        }
                        Some(Command::WriteSettings { target, kind, group, writes }) => {
                            client.write_settings(target, kind, group, writes).await
                        }
                        Some(Command::SetEncoding { channel, body }) => {
                            if let Err(e) = client.set_encoding(channel, body).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::Siren { channel }) => {
                            if let Err(e) = client.play_siren(channel).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::Spotlight { channel, on }) => {
                            if let Err(e) = client.set_spotlight(channel, on).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::Ptz { channel, command, speed }) => {
                            if let Err(e) = client.ptz(channel, command, speed).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::QueryZoomFocus { channel }) => {
                            client.query_zoom_focus(channel).await;
                        }
                        Some(Command::Zoom { channel, position }) => {
                            if let Err(e) = client.set_zoom(channel, position).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::QueryPresets { channel }) => {
                            client.query_presets(channel).await;
                        }
                        Some(Command::SetPreset { channel, id, name }) => {
                            if let Err(e) = client.set_preset(channel, id, &name).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::GotoPreset { channel, id }) => {
                            if let Err(e) = client.goto_preset(channel, id).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::DeletePreset { channel, id }) => {
                            if let Err(e) = client.delete_preset(channel, id).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::Calibrate { channel }) => {
                            if let Err(e) = client.calibrate_ptz(channel).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::QueryMonitorPoint { channel }) => {
                            client.query_monitor_point(channel).await;
                        }
                        Some(Command::QueryMonitorPointImage { channel }) => {
                            client.query_monitor_point_image(channel).await;
                        }
                        Some(Command::CapturePresetImage { channel, preset_id }) => {
                            client.request_snapshot(channel, preset_id).await;
                        }
                        Some(Command::UploadPresetImage { channel, preset_id, jpeg }) => {
                            client.upload_preset_image(channel, preset_id, jpeg).await;
                        }
                        Some(Command::SavePresetWithImage { channel, id, name }) => {
                            if let Err(e) = client.save_preset_with_image(channel, id, &name).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::ModifyPreset { channel, id, name, with_image }) => {
                            if let Err(e) = client.modify_preset(channel, id, &name, with_image).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::QueryPresetImage { channel, preset_id }) => {
                            client.query_preset_image(channel, preset_id).await;
                        }
                        Some(Command::SetMonitorPointConfig { channel, enabled, timeout_seconds }) => {
                            if let Err(e) = client.set_monitor_point_config(channel, enabled, timeout_seconds).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::SetMonitorPointHere { channel, enabled, timeout_seconds }) => {
                            if let Err(e) =
                                client.set_current_position_as_monitor_point(channel, enabled, timeout_seconds).await
                            {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::GoToMonitorPoint { channel, timeout_seconds }) => {
                            if let Err(e) = client.go_to_monitor_point(channel, timeout_seconds).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::Focus { channel, position }) => {
                            if let Err(e) = client.set_focus(channel, position).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::TalkStart { channel }) => {
                            if let Err(e) = client.talk_start(channel).await {
                                let _ = tx.send(DeviceEvent::ControlFailed(e.to_string())).await;
                            }
                        }
                        Some(Command::TalkBlock { channel, block }) => {
                            let _ = client.talk_block(channel, &block).await;
                        }
                        Some(Command::TalkStop { channel }) => {
                            let _ = client.talk_stop(channel).await;
                        }
                        Some(Command::Playback(command)) => {
                            // Live and playback never share the connection's
                            // reader.
                            if frames.take().is_some() {
                                let _ = client.stop_video(channel).await;
                                sink = None;
                                announced = false;
                            }
                            driver.command(command, &mut client, &tx).await;
                        }
                        Some(Command::Stop) => {
                            if frames.take().is_some() {
                                let _ = client.stop_video(channel).await;
                            }
                        }
                        Some(Command::Shutdown) | None => break,
                    },
                    // Live sound only: a playback's audio travels with its
                    // video (`driver.units()`) into the replay sink.
                    Some(frame) = audio.recv() => audio_out.push(&frame),
                    Some(update) = channel_updates.recv() => {
                        let event = match update {
                            DeviceUpdate::Channels(channels) => {
                                // An NVR's list carries no names; ask for the
                                // ones we do not have (once per channel).
                                let unnamed: Vec<u8> = channels
                                    .iter()
                                    .filter(|c| c.online && c.name.is_empty())
                                    .map(|c| c.channel_id)
                                    .filter(|id| asked_names.insert(*id))
                                    .collect();
                                client.request_channel_names(&unnamed).await;
                                DeviceEvent::Channels(channels)
                            }
                            DeviceUpdate::ChannelName { channel_id, name } => {
                                DeviceEvent::ChannelName { channel_id, name }
                            }
                            DeviceUpdate::Abilities(abilities) => DeviceEvent::Abilities(abilities),
                            DeviceUpdate::ZoomFocus { channel_id, zoom, focus } => {
                                DeviceEvent::ZoomFocus { channel_id, zoom, focus }
                            }
                            DeviceUpdate::Presets { channel_id, presets } => {
                                DeviceEvent::Presets { channel_id, presets }
                            }
                            DeviceUpdate::MonitorPoint { channel_id, state } => {
                                DeviceEvent::MonitorPoint { channel_id, state }
                            }
                            DeviceUpdate::MonitorPointImage { channel_id, jpeg } => {
                                DeviceEvent::MonitorPointImage { channel_id, jpeg }
                            }
                            DeviceUpdate::Snapshot { preset_id, jpeg, .. } => {
                                DeviceEvent::Snapshot { preset_id, jpeg }
                            }
                            DeviceUpdate::ImageNotFound { preset_id, .. } => {
                                DeviceEvent::ImageNotFound { preset_id }
                            }
                            DeviceUpdate::PresetImage { channel_id, preset_id, jpeg } => {
                                DeviceEvent::PresetImage { channel_id, preset_id, jpeg }
                            }
                            DeviceUpdate::ControlReply { msg_id, code } => {
                                DeviceEvent::ControlReply { msg_id, code }
                            }
                            DeviceUpdate::Encoding { channel_id, encoding } => {
                                DeviceEvent::Encoding { channel_id, encoding }
                            }
                            DeviceUpdate::EncodeProfiles { channel_id, profiles } => {
                                DeviceEvent::ChannelEncodeProfiles { channel_id, profiles }
                            }
                            DeviceUpdate::ChannelVersion { channel_id, model, firmware, .. } => {
                                DeviceEvent::ChannelVersion { channel_id, model, firmware }
                            }
                            DeviceUpdate::Settings { page, read } => DeviceEvent::Settings { page, read },
                            DeviceUpdate::SettingsWritten { target, group, results } => {
                                DeviceEvent::SettingsWritten { target, group, results }
                            }
                        };
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    frame = async {
                        match frames.as_mut() {
                            Some(f) => f.next().await,
                            None => std::future::pending().await,
                        }
                    } => match frame {
                        Some(Ok(frame)) => {
                            let live = LiveStream { frames: &mut frames, sink: &mut sink, announced: &mut announced };
                            let next = on_live_frame(&frame, channel, live, &sink_request_tx, &tx, || {
                                client.stop_video(channel)
                            });
                            match next.await {
                                AfterFrame::Shown | AfterFrame::Stopped => {}
                                AfterFrame::End => break,
                            }
                        }
                        Some(Err(e)) => {
                            frames = None;
                            let _ = tx.send(DeviceEvent::PlayFailed { channel, reason: e.to_string() }).await;
                        }
                        None => frames = None,
                    },
                    item = async {
                        match driver.units() {
                            Some(units) => units.next().await,
                            None => std::future::pending().await,
                        }
                    } => driver.on_unit(item, &tx).await,
                    unit = async {
                        match download.as_mut() {
                            Some(job) => job.next_unit().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        let ended = match download.as_mut() {
                            Some(job) => job.on_unit(unit, &tx).await,
                            None => None,
                        };
                        if let Some(ended) = ended {
                            if let Some(job) = download.take() {
                                job.finish(ended, &mut client, &tx).await;
                            }
                        }
                    },
                    _ = playback_tick.tick(), if replay_active => driver.tick(&mut client, &tx).await,
                    error = client.wait_for_pushes(), if frames.is_none() => {
                        let _ = tx.send(DeviceEvent::Lost(error.to_string())).await;
                        break;
                    }
                }
            }
            // Every exit path ends the session — tell the device before
            // this task (and the client with it) goes away. A download
            // under way goes with it (its partial file is removed).
            if let Some(job) = download.take() {
                job.abandon();
            }
            client.disconnect().await;
        });
    });

    DeviceLink { events: rx, commands, playback_gen }
}

#[cfg(test)]
mod tests {
    use super::{
        login_refused, on_live_frame, request_sink, AfterFrame, DeviceEvent, LiveStream, SinkAnswer, SinkRequest,
    };
    use crate::ui::video_view::fixtures::{self, View};
    use reoling::{VideoFrame, VideoType};
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn only_a_wrong_password_is_a_rejected_login() {
        assert!(matches!(login_refused(401), DeviceEvent::LoginRejected));
        for (code, reason) in [
            (400, "Login refused by the device (code 400)"),
            (409, "Too many users are signed in to the device"),
            (419, "The device is busy with another client"),
            (0, "Login refused by the device (code 0)"),
            (0xffff, "Login refused by the device (code 65535)"),
        ] {
            match login_refused(code) {
                DeviceEvent::Lost(r) => assert_eq!(r, reason, "code {code}"),
                _ => panic!("code {code} must keep the saved password"),
            }
        }
    }

    const CHANNEL: u8 = 3;

    /// A live frame, as the device's frames come.
    fn frame(video_type: VideoType, microseconds: u32, is_keyframe: bool) -> VideoFrame {
        VideoFrame { video_type, is_keyframe, microseconds, data: vec![0u8; 2_000] }
    }

    /// Stands for a stream's frames from the device, and says in `log` when
    /// the connection lets go of them.
    struct Frames(Rc<RefCell<Vec<&'static str>>>);

    impl Drop for Frames {
        fn drop(&mut self) {
            self.0.borrow_mut().push("frames let go");
        }
    }

    /// The events the window has had since last asked, as read here.
    fn heard(events: &async_channel::Receiver<DeviceEvent>) -> Vec<String> {
        std::iter::from_fn(|| events.try_recv().ok())
            .map(|event| match event {
                DeviceEvent::PlayFailed { channel, reason } => format!("PlayFailed {channel}: {reason}"),
                DeviceEvent::Playing => "Playing".to_string(),
                _ => "another event".to_string(),
            })
            .collect()
    }

    #[tokio::test]
    async fn a_refused_live_sink_stops_the_stream_and_the_connection_goes_on() {
        // The window's live responder (main_window.rs) on a thread of its
        // own, as on GTK's: a live view on a system without h265parse.
        let (requests, mut incoming) = tokio::sync::mpsc::channel::<SinkRequest>(8);
        let window = std::thread::spawn(move || {
            let view = View::live(Some(VideoType::H265));
            while let Some((video_type, reply)) = incoming.blocking_recv() {
                let _ = reply.send(view.ensure_sink(video_type));
            }
            view.reset();
        });
        let (tx, events) = async_channel::unbounded();
        let log = Rc::new(RefCell::new(Vec::new()));
        let (mut sink, mut announced) = (None, false);
        // Play, twice (the window still answers after a refusal; in 1.0.4
        // its responder died with the panic): a stream this system cannot
        // show.
        let h265 = frame(VideoType::H265, 0, true);
        for played in 1..=2 {
            let mut frames = Some(Frames(Rc::clone(&log)));
            let live = LiveStream { frames: &mut frames, sink: &mut sink, announced: &mut announced };
            let stopped = Rc::clone(&log);
            let next = on_live_frame(&h265, CHANNEL, live, &requests, &tx, || async move {
                stopped.borrow_mut().push("stop_video");
            });
            // The connection goes on without the stream. Its frames are let
            // go before the device is told to stop sending them (a reader
            // whose frames no one takes would hold up the reply), once.
            assert_eq!(next.await, AfterFrame::Stopped);
            assert!(frames.is_none() && sink.is_none() && !announced);
            assert_eq!(*log.borrow(), ["frames let go", "stop_video"].repeat(played));
            let failed = format!("PlayFailed {CHANNEL}: Cannot show this stream: h265parse is missing.");
            assert_eq!(heard(&events), [failed]);
        }
        // Play again, a stream it can show: into its sink, `Playing` once,
        // not stopped.
        let mut frames = Some(Frames(Rc::clone(&log)));
        for i in 0..3u32 {
            let h264 = frame(VideoType::H264, i * 40_000, i == 0);
            let live = LiveStream { frames: &mut frames, sink: &mut sink, announced: &mut announced };
            let next = on_live_frame(&h264, CHANNEL, live, &requests, &tx, || async {
                unreachable!("a stream that is shown was stopped")
            });
            assert_eq!(next.await, AfterFrame::Shown);
        }
        assert!(frames.is_some() && announced);
        assert_eq!(log.borrow().len(), 4);
        assert_eq!(heard(&events), ["Playing"]);
        assert!(fixtures::queued_bytes(sink.as_ref().expect("a sink")) > 0, "the frames did not go into the sink");
        drop(sink.take());
        drop(requests);
        window.join().expect("the window's thread");
    }

    #[tokio::test]
    async fn without_the_window_a_live_frame_ends_the_connection() {
        let (tx, events) = async_channel::unbounded();
        let (mut frames, mut sink, mut announced) = (Some(()), None, false);
        let h264 = frame(VideoType::H264, 0, true);
        // Gone before the stream's first frame...
        let (requests, incoming) = tokio::sync::mpsc::channel::<SinkRequest>(1);
        drop(incoming);
        let live = LiveStream { frames: &mut frames, sink: &mut sink, announced: &mut announced };
        let next = on_live_frame(&h264, CHANNEL, live, &requests, &tx, || async {
            unreachable!("the stream was stopped instead of the connection ended")
        });
        assert_eq!(next.await, AfterFrame::End);
        assert!(heard(&events).is_empty());
        // ...or before `Playing` is said.
        let view = View::live(None);
        sink = Some(view.ensure_sink(VideoType::H264).expect("a sink"));
        drop(events);
        let live = LiveStream { frames: &mut frames, sink: &mut sink, announced: &mut announced };
        let next = on_live_frame(&h264, CHANNEL, live, &requests, &tx, || async {
            unreachable!("the stream was stopped instead of the connection ended")
        });
        assert_eq!(next.await, AfterFrame::End);
        drop(sink);
        view.reset();
    }

    #[tokio::test]
    async fn without_the_window_the_connection_ends() {
        // Gone before the request...
        let (requests, incoming) = tokio::sync::mpsc::channel::<SinkRequest>(1);
        drop(incoming);
        assert!(matches!(request_sink(&requests, VideoType::H264).await, SinkAnswer::Gone));
        // ...or before its answer.
        let (requests, mut incoming) = tokio::sync::mpsc::channel::<SinkRequest>(1);
        let window = tokio::spawn(async move {
            while let Some((_, reply)) = incoming.recv().await {
                drop(reply);
            }
        });
        assert!(matches!(request_sink(&requests, VideoType::H264).await, SinkAnswer::Gone));
        drop(requests);
        window.await.expect("the window's task");
    }
}
