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
    /// device itself is still connected. `refused`: the device refused the
    /// stream (a trial may try other request forms).
    PlayFailed { channel: u8, reason: String, refused: bool },
    /// A trial (see `DeviceLink::try_live`) ended this connection, which
    /// has been closed on the device: the device is to be connected again,
    /// with the legacy cipher step armed when `legacy`, after which
    /// `try_live_after_cipher` follows (the window keeps the trial's
    /// channel and stream). `message`: how the trial ended, for
    /// the user, when it is over. `carried`: the commands this connection
    /// did not serve, for the next one (see `spawn_device`).
    TrialReconnect { legacy: bool, message: Option<&'static str>, carried: Carried },
    /// A trial's form was accepted: `stream` of `channel` plays now.
    TrialAccepted { channel: u8, stream: StreamProfile },
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
    /// `gen`: the trial's number (see `DeviceLink::try_live`).
    TryLive { channel: u8, refused: StreamProfile, gen: u64 },
    TryLiveAfterCipher { channel: u8, refused: StreamProfile, gen: u64 },
    /// Wakes a connection whose trial the window cancelled; nothing else.
    CancelTrial,
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

/// The commands a connection did not serve before a trial ended it (see `DeviceEvent::TrialReconnect`): the next
/// connection to the device serves them first, in order, so that a command the user gave meanwhile is not lost.
/// This is the one place that decides what is carried: never a settings page's write, which the camera window
/// reports as dropped when the connection makes way (`CameraSettings::reconnecting`); for live view only what the
/// user asked for last, from the trial on until the next connection begins to serve (`keep_last_live`); and nothing
/// at all once `CARRY_LIMIT` has passed when that connection begins to serve them. An encoding save is carried: the
/// window keeps waiting for its answer and is not told it was dropped.
#[derive(Default)]
pub struct Carried {
    deferred: VecDeque<Command>,
    /// The old link's queue: what the window sent it after the connection stopped reading.
    rest: Option<tokio::sync::mpsc::UnboundedReceiver<Command>>,
    /// When the connection that left them ended.
    since: Option<std::time::Instant>,
}

/// How long carried commands wait for the next connection to serve them (see `Carried`). The window makes it at once,
/// so this only cuts off commands that would run long after the user gave them.
const CARRY_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

impl Carried {
    /// What the next connection serves first, taken as it begins to serve (`serve`, after its login): every carried
    /// command in order, none once `CARRY_LIMIT` has passed by `now`; then the commands the window sent the new link
    /// meanwhile (`pending`), so that live view follows what the user asked for last across both. With them, the
    /// carried ones at their head, which `serve` drops if they still wait when the limit passes (`LateCarried`).
    /// Nothing is taken after an ordinary connection. The old link must be gone by then.
    fn into_commands_at(
        mut self,
        pending: &mut tokio::sync::mpsc::UnboundedReceiver<Command>,
        now: std::time::Instant,
    ) -> (VecDeque<Command>, LateCarried) {
        let Some(since) = self.since else { return (self.deferred, LateCarried { left: 0, since: None }) };
        self.settle();
        if now.saturating_duration_since(since) > CARRY_LIMIT {
            self.deferred.clear();
        }
        let carried = self.deferred.len();
        while let Ok(command) = pending.try_recv() {
            self.deferred.push_back(command);
        }
        let left = keep_last_live(&mut self.deferred, carried);
        (self.deferred, LateCarried { left, since: Some(since) })
    }

    /// Moves the old link's queue into `deferred` and leaves out what is never carried: a wake-up for the old
    /// connection's trial, which has nothing left to wake, the `Shutdown` the old link sent as it was dropped, and
    /// the settings pages' writes, which the window has reported as dropped.
    fn settle(&mut self) {
        if let Some(mut rest) = self.rest.take() {
            while let Ok(command) = rest.try_recv() {
                self.deferred.push_back(command);
            }
        }
        self.deferred.retain(|c| {
            !matches!(c, Command::CancelTrial | Command::Shutdown | Command::WriteSettings { .. })
        });
    }

    /// What the user now wants to watch on the device while it has no connection: `Some` a channel's stream,
    /// `None` nothing (Stop, the Playback page, another device). It replaces every carried Play, Stop and trial.
    pub fn set_live(&mut self, play: Option<(u8, StreamProfile)>) {
        self.settle();
        self.deferred.push_back(play.map_or(Command::Stop, |(channel, profile)| Command::Play { channel, profile }));
        keep_last_live(&mut self.deferred, 0);
    }
}

/// The carried commands still at the head of a connection's `deferred` (`left`), and when the connection that left
/// them ended: a carried command is served within `CARRY_LIMIT` or not at all, also when an earlier one (a carried
/// download) keeps it waiting. `deferred` only grows at its back and is served from its front, so they stay at its
/// head.
struct LateCarried {
    left: usize,
    since: Option<std::time::Instant>,
}

impl LateCarried {
    /// Drops the carried commands still waiting at the head of `deferred` once the limit has passed by `now`.
    fn prune(&mut self, deferred: &mut VecDeque<Command>, now: std::time::Instant) {
        if self.left > 0 && self.since.is_some_and(|since| now.saturating_duration_since(since) > CARRY_LIMIT) {
            deferred.drain(..self.left.min(deferred.len()));
            self.left = 0;
        }
    }

    /// A command is about to be taken: one from the head of `deferred` is no longer waiting.
    fn took(&mut self, incoming: &Incoming) {
        if matches!(incoming, Incoming::Deferred) {
            self.left = self.left.saturating_sub(1);
        }
    }
}

/// The one rule for carried live view: of the live view commands in `commands` (Play, a trial's, Stop) only the last
/// stays, and none when that is a Stop, which has nothing to stop on a new connection. How many of the first
/// `head` commands stay.
fn keep_last_live(commands: &mut VecDeque<Command>, head: usize) -> usize {
    let live = |c: &Command| {
        matches!(c, Command::Play { .. } | Command::TryLive { .. } | Command::TryLiveAfterCipher { .. } | Command::Stop)
    };
    let last = commands.iter().rposition(live).filter(|&at| !matches!(commands[at], Command::Stop));
    let (mut at, mut kept) = (0, 0);
    commands.retain(|c| {
        let keep = !live(c) || Some(at) == last;
        kept += usize::from(keep && at < head);
        at += 1;
        keep
    });
    kept
}

/// What the command branch of the device loop got: a command whose turn
/// has come after a download, or one from the link.
enum Incoming {
    Deferred,
    Received(Option<Command>),
}

impl Command {
    /// Whether the command ends a running trial: it needs the connection's
    /// reader or ends the connection, or it asks for exactly that.
    fn ends_trial(&self) -> bool {
        matches!(
            self,
            Command::Play { .. }
                | Command::TryLive { .. }
                | Command::TryLiveAfterCipher { .. }
                | Command::CancelTrial
                | Command::Playback(_)
                | Command::Download(_)
                | Command::Stop
                | Command::Shutdown
        )
    }
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

    /// After the device refused the `refused` stream of `channel`: asks for
    /// live video in the other request forms Reoling knows, one after the
    /// other, and plays the first one the device accepts (see
    /// `ReolinkClient::try_live`). `gen`: the trial's number. A form of the
    /// trial is admitted for sending only while the device's trial number
    /// (`spawn_device`) still equals it, so the window cancels it by moving
    /// that number on (see `TrialGate`).
    pub fn try_live(&self, channel: u8, refused: StreamProfile, gen: u64) {
        let _ = self.commands.send(Command::TryLive { channel, refused, gen });
    }

    /// The last step of a trial, once the device has connected again with
    /// the legacy cipher step armed; `gen` as for `try_live`.
    pub fn try_live_after_cipher(&self, channel: u8, refused: StreamProfile, gen: u64) {
        let _ = self.commands.send(Command::TryLiveAfterCipher { channel, refused, gen });
    }

    /// Wakes the connection after the window moved the trial number on: a
    /// form under way is no longer waited for. Stop, Play and Shutdown do
    /// that too.
    pub fn cancel_trial(&self) {
        let _ = self.commands.send(Command::CancelTrial);
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
                let _ = tx.send(DeviceEvent::PlayFailed { channel, reason, refused: false }).await;
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

/// What stderr says of a failed connect or login: a fixed text per kind, as
/// some texts quote the device's XML or its address.
fn error_kind(e: &reoling::Error) -> &'static str {
    use reoling::Error;
    match e {
        Error::UidResolutionFailed => "could not resolve the device's UID against any P2P relay",
        Error::P2pNegotiationTimeout => "P2P negotiation timed out",
        Error::LoginFailed { .. } => "login refused",
        Error::ProtocolError(_) | Error::RegisterRejected { .. } => "protocol error",
        Error::StreamRefused { .. } => "stream refused",
        Error::ReplyTimeout => "no reply in time",
        Error::ConnectionLost => "connection lost",
        Error::Io(_) => "network or socket error",
        Error::Xml(_) => "unreadable XML reply",
        Error::AddrParse(_) => "invalid IP address",
    }
}

/// What the user sees over the picture when a trial found no form the device accepts.
pub const NO_FORM_ACCEPTED: &str =
    "The camera refused every request form Reoling knows. The diagnostic log has the details.";
/// The same when a form went unanswered.
pub const TRIAL_TIMED_OUT: &str =
    "The camera did not answer one of the request forms in time. The diagnostic log has the details.";
/// The same when the connection broke during a form.
pub const TRIAL_LOST: &str =
    "The connection broke while Reoling tried another request form. The diagnostic log has the details.";

/// Connects and logs in to a device on a dedicated thread with its own
/// tokio runtime, and keeps the connection up: it listens for pushes (the
/// channel list) and streams a channel on request. GTK is never touched from
/// there — everything reaches the UI through `DeviceLink::events`.
///
/// **Video frames bypass the GTK main loop entirely**. Routing
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
///
/// `diag_device` is the device's 1-based place in the list, for the
/// diagnostic log's `[dev D]`; `legacy_cipher` arms the legacy cipher step
/// of a trial for this one connection (see `ReolinkClient::set_try_legacy`).
/// `trial_epoch`: the device's trial number, which the window owns and moves
/// on to cancel a trial (see `DeviceLink::try_live`). `carried`: what the
/// connection a trial ended left unserved, served here first.
#[allow(clippy::too_many_arguments)]
pub fn spawn_device(
    target: ConnectTarget,
    username: String,
    password: String,
    sink_request_tx: tokio::sync::mpsc::Sender<SinkRequest>,
    replay_sink_request_tx: tokio::sync::mpsc::Sender<ReplaySinkRequest>,
    audio_out: std::sync::Arc<crate::ui::audio::AudioOutput>,
    diag_device: u32,
    legacy_cipher: bool,
    trial_epoch: reoling::TrialGate,
    carried: Carried,
) -> DeviceLink {
    let (tx, rx) = async_channel::unbounded();
    let (commands, command_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
    let playback_gen = Arc::new(AtomicU64::new(0));
    let driver_gen = Arc::clone(&playback_gen);

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        runtime.block_on(reoling::diag::DEVICE.scope(diag_device, async move {
            if reoling::diag::on() {
                let how = if matches!(target, ConnectTarget::Uid(_)) { "UID" } else { "address" };
                reoling::diag::masked(format_args!("NEGOTIATION device: added by {how}"));
            }
            // A device added by address has its connection number before it connects, so that a failed attempt's
            // lines carry it; one added by UID is numbered in its race.
            let attempt = matches!(target, ConnectTarget::Ip { .. }).then(ReolinkClient::reserve_debug_id);
            let tag = attempt.map_or(String::new(), |id| format!("[conn {id}] "));
            // A UID device races its paths and comes back logged in; one added by IP logs in below.
            let connect_result = match target {
                ConnectTarget::Uid(uid) => {
                    ReolinkClient::connect_and_login_by_uid(&uid, &username, &password).await.map(|c| (c, true))
                }
                ConnectTarget::Ip { addr, port } => {
                    let id = attempt.unwrap_or_else(ReolinkClient::reserve_debug_id);
                    ReolinkClient::connect_by_ip_as(addr, port, id).await.map(|c| (c, false))
                }
            };
            // A connection made for a trial's legacy cipher step that fails ends the trial: the log says so.
            let trial_failed = || {
                if legacy_cipher {
                    let ended = "TRIAL ended: the connection for the legacy cipher step failed";
                    reoling::diag::masked(format_args!("{tag}{ended}"));
                }
            };
            let (mut client, logged_in) = match connect_result {
                Ok(connected) => connected,
                Err(reoling::Error::LoginFailed { code }) => {
                    reoling::diag::always_under(&tag, format_args!("login refused by the device: code {code}"));
                    reoling::diag::masked(format_args!("{tag}NEGOTIATION device: login refused, code {code}"));
                    trial_failed();
                    let _ = tx.send(login_refused(code)).await;
                    return;
                }
                Err(e) => {
                    let failed = format_args!("connecting to the device failed: {}", error_kind(&e));
                    reoling::diag::always_under(&tag, failed);
                    reoling::diag::masked(format_args!("{tag}NEGOTIATION device: connect failed: {}", error_kind(&e)));
                    trial_failed();
                    let _ = tx.send(DeviceEvent::Lost(e.to_string())).await;
                    return;
                }
            };
            if !logged_in {
                match client.login(&username, &password).await {
                    Ok(_) => {}
                    Err(reoling::Error::LoginFailed { code }) => {
                        reoling::diag::always_under(&tag, format_args!("login refused by the device: code {code}"));
                        reoling::diag::masked(format_args!("{tag}NEGOTIATION device: login refused, code {code}"));
                        trial_failed();
                        let _ = tx.send(login_refused(code)).await;
                        return;
                    }
                    Err(e) => {
                        let failed = format_args!("logging in to the device failed: {}", error_kind(&e));
                        reoling::diag::always_under(&tag, failed);
                        let kind = error_kind(&e);
                        reoling::diag::masked(format_args!("{tag}NEGOTIATION device: connect failed: {kind}"));
                        trial_failed();
                        let _ = tx.send(DeviceEvent::Lost(e.to_string())).await;
                        return;
                    }
                }
            }
            reoling::diag::masked(format_args!("NEGOTIATION device: logged in on [conn {}]", client.debug_id()));
            if legacy_cipher {
                client.set_try_legacy();
            }
            let channel_updates =
                client.take_channel_updates().expect("taken once per client");
            let audio = client.take_audio().expect("taken once per client");
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

            serve(
                client,
                command_rx,
                tx,
                channel_updates,
                audio,
                driver_gen,
                sink_request_tx,
                replay_sink_request_tx,
                audio_out,
                legacy_cipher,
                trial_epoch,
                carried,
            )
            .await;
        }));
    });

    DeviceLink { events: rx, commands, playback_gen }
}

/// Runs `trial` while the commands keep coming in; every command but
/// `CancelTrial` waits in `deferred` for its turn, in order. `None` when a
/// command that ends a trial (`Command::ends_trial`) or the window's going
/// (then also `true`) abandoned it: the form under way is not waited for,
/// so the connection is mid-reply and has to be closed.
async fn run_trial(
    trial: impl std::future::Future<Output = reoling::TrialEnd>,
    command_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Command>,
    deferred: &mut VecDeque<Command>,
) -> (Option<reoling::TrialEnd>, bool) {
    tokio::pin!(trial);
    loop {
        tokio::select! {
            // The trial first: one the window cancelled before this ran ends here without a form.
            biased;
            end = &mut trial => return (Some(end), false),
            command = command_rx.recv() => match command {
                Some(command) => {
                    let ends = command.ends_trial();
                    if !matches!(command, Command::CancelTrial) {
                        deferred.push_back(command);
                    }
                    if ends {
                        return (None, false);
                    }
                }
                None => return (None, true),
            },
        }
    }
}

/// The end of a trial abandoned mid-form (see `run_trial`): the connection logs out, and the window hears it as a
/// cancelled trial's reconnection.
fn abandoned(client: &ReolinkClient) -> Option<DeviceEvent> {
    reoling::diag::masked(format_args!(
        "[conn {}] TRIAL abandoned: the form under way is not waited for; the connection is closed",
        client.debug_id()
    ));
    Some(DeviceEvent::TrialReconnect { legacy: false, message: None, carried: Carried::default() })
}

/// What the connection does after a trial (`after_cipher`: its last step):
/// an accepted form's stream plays like any live stream; with none left
/// the window hears so. `Some`: the event that ends this connection, sent
/// once it is closed, for the device to be connected again.
async fn after_trial(
    end: reoling::TrialEnd,
    channel: u8,
    after_cipher: bool,
    frames: &mut Option<tokio_stream::wrappers::ReceiverStream<reoling::Result<VideoFrame>>>,
    tx: &async_channel::Sender<DeviceEvent>,
) -> Option<DeviceEvent> {
    let over = |message| {
        Some(DeviceEvent::TrialReconnect { legacy: false, message: Some(message), carried: Carried::default() })
    };
    match end {
        reoling::TrialEnd::Accepted { form, frames: accepted } => {
            let _ = tx.send(DeviceEvent::TrialAccepted { channel, stream: form.stream }).await;
            *frames = Some(accepted);
        }
        // The legacy cipher step was armed for that connection only: the next one is made as always.
        reoling::TrialEnd::NoneAccepted if after_cipher => return over(NO_FORM_ACCEPTED),
        reoling::TrialEnd::NoneAccepted => {
            let reason = NO_FORM_ACCEPTED.to_string();
            let _ = tx.send(DeviceEvent::PlayFailed { channel, reason, refused: false }).await;
        }
        reoling::TrialEnd::Reconnect => {
            return Some(DeviceEvent::TrialReconnect { legacy: true, message: None, carried: Carried::default() })
        }
        reoling::TrialEnd::TimedOut => return over(TRIAL_TIMED_OUT),
        reoling::TrialEnd::Lost => return over(TRIAL_LOST),
        // The window ended it and says so; the command that did waits in `deferred`. After the cipher step the
        // connection is replaced by an ordinary one.
        reoling::TrialEnd::Cancelled if after_cipher => {
            return Some(DeviceEvent::TrialReconnect { legacy: false, message: None, carried: Carried::default() })
        }
        reoling::TrialEnd::Cancelled => {}
    }
    None
}

/// A logged-in connection (see `spawn_device`), until a `Shutdown`, the
/// window's going or the connection's loss: the commands, the pushes, live
/// video, playback and downloads. Then the session ends on the device.
/// `legacy_cipher`: made for a trial's legacy cipher step, so a trial
/// cancelled on it ends it too, for an ordinary connection in its place.
/// `trial_epoch` as in `spawn_device`; `carried`: the commands carried
/// over, taken as this begins and served first (`Carried::into_commands_at`).
#[allow(clippy::too_many_arguments)]
async fn serve(
    mut client: ReolinkClient,
    mut command_rx: tokio::sync::mpsc::UnboundedReceiver<Command>,
    tx: async_channel::Sender<DeviceEvent>,
    mut channel_updates: tokio::sync::mpsc::UnboundedReceiver<DeviceUpdate>,
    mut audio: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    driver_gen: Arc<AtomicU64>,
    sink_request_tx: tokio::sync::mpsc::Sender<SinkRequest>,
    replay_sink_request_tx: tokio::sync::mpsc::Sender<ReplaySinkRequest>,
    audio_out: std::sync::Arc<crate::ui::audio::AudioOutput>,
    legacy_cipher: bool,
    trial_epoch: reoling::TrialGate,
    carried: Carried,
) {
    let (mut deferred, mut late) = carried.into_commands_at(&mut command_rx, std::time::Instant::now());
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
    // The last stream the device refused, with its code, for a trial's log.
    let mut refusal: Option<(u8, StreamProfile, u16)> = None;
    // What a trial tells the window once this connection is closed on the device (see `after_trial`).
    let mut farewell: Option<DeviceEvent> = None;

    loop {
        late.prune(&mut deferred, std::time::Instant::now());
        let replay_active = driver.is_active();
        let downloading = download.is_some();
        let has_deferred = !downloading && !deferred.is_empty();
        tokio::select! {
            incoming = async {
                if has_deferred { Incoming::Deferred } else { Incoming::Received(command_rx.recv().await) }
            } => match {
                late.took(&incoming);
                incoming.command(&mut deferred)
            } {
                Some(
                    command @ (Command::Play { .. }
                    | Command::TryLive { .. }
                    | Command::TryLiveAfterCipher { .. }
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
                            if let reoling::Error::StreamRefused { code, .. } = &e {
                                refusal = Some((channel, profile, *code));
                            }
                            let refused = matches!(e, reoling::Error::StreamRefused { .. });
                            let event = DeviceEvent::PlayFailed { channel, reason: e.to_string(), refused };
                            let _ = tx.send(event).await;
                        }
                    }
                }
                // A trial the window cancelled before this connection read it: nothing of it is done.
                Some(Command::TryLive { gen, .. } | Command::TryLiveAfterCipher { gen, .. })
                    if trial_epoch.cancelled(gen) => {}
                Some(Command::TryLive { channel: wanted, refused, gen }) => {
                    driver.end(&mut client).await;
                    if frames.take().is_some() {
                        let _ = client.stop_video(channel).await;
                    }
                    channel = wanted;
                    sink = None;
                    announced = false;
                    let code = refusal.filter(|r| r.0 == wanted && r.1 == refused).map(|r| r.2);
                    let trial = client.try_live(channel, refused, code, &trial_epoch, gen);
                    let (end, gone) = run_trial(trial, &mut command_rx, &mut deferred).await;
                    let gone = gone || closing(&mut command_rx, &mut deferred, end.is_none());
                    let farewell_now = match end {
                        Some(end) => after_trial(end, channel, false, &mut frames, &tx).await,
                        None => abandoned(&client),
                    };
                    let farewell_now = farewell_now.filter(|_| !gone);
                    if let Some(reconnect) = farewell_now {
                        farewell = Some(reconnect);
                        break;
                    }
                    if gone {
                        break;
                    }
                }
                Some(Command::TryLiveAfterCipher { channel: wanted, refused, gen }) => {
                    driver.end(&mut client).await;
                    if frames.take().is_some() {
                        let _ = client.stop_video(channel).await;
                    }
                    channel = wanted;
                    sink = None;
                    announced = false;
                    let trial = client.try_live_after_cipher(channel, refused, &trial_epoch, gen);
                    let (end, gone) = run_trial(trial, &mut command_rx, &mut deferred).await;
                    let gone = gone || closing(&mut command_rx, &mut deferred, end.is_none());
                    let farewell_now = match end {
                        Some(end) => after_trial(end, channel, true, &mut frames, &tx).await,
                        None => abandoned(&client),
                    };
                    let farewell_now = farewell_now.filter(|_| !gone);
                    if let Some(reconnect) = farewell_now {
                        farewell = Some(reconnect);
                        break;
                    }
                    if gone {
                        break;
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
                // No trial runs (`run_trial` takes it while one does). On a connection armed for the legacy
                // cipher step, whose trial was cancelled before its last forms, an ordinary one takes its place.
                Some(Command::CancelTrial) => {
                    if legacy_cipher {
                        let carried = Carried::default();
                        farewell = Some(DeviceEvent::TrialReconnect { legacy: false, message: None, carried });
                        break;
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
                    let refused = matches!(e, reoling::Error::StreamRefused { .. });
                    let _ = tx.send(DeviceEvent::PlayFailed { channel, reason: e.to_string(), refused }).await;
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
    if let Some(mut event) = farewell {
        if let DeviceEvent::TrialReconnect { carried, .. } = &mut event {
            *carried = Carried { deferred, rest: Some(command_rx), since: Some(std::time::Instant::now()) };
        }
        let _ = tx.send(event).await;
    }
}

/// Whether the window is closing (a `Shutdown` waits): then the connection only logs out. After an abandoned trial
/// (`drain`) the commands already sent are read first, so that a `Shutdown` right behind the cancel counts.
fn closing(
    command_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Command>,
    deferred: &mut VecDeque<Command>,
    drain: bool,
) -> bool {
    while let Some(command) = drain.then(|| command_rx.try_recv().ok()).flatten() {
        if !matches!(command, Command::CancelTrial) {
            deferred.push_back(command);
        }
    }
    deferred.iter().any(|c| matches!(c, Command::Shutdown))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        login_refused, on_live_frame, request_sink, serve, AfterFrame, Carried, Command, DeviceEvent, DeviceLink,
        LiveStream, SinkAnswer, SinkRequest,
    };
    use crate::ui::audio::AudioOutput;
    use crate::ui::video_view::fixtures::{self, View};
    use reoling::protocol::bc::model::{
        Bc, BcBody, BcMeta, ModernMsg, MSG_ID_GET_COMPRESSION, MSG_ID_LINK_TYPE, MSG_ID_LOGOUT, MSG_ID_VIDEO,
        MSG_ID_VIDEO_STOP,
    };
    use reoling::protocol::bc::xml::{Extension, XML_VERSION};
    use reoling::protocol::crypto::EncryptionProtocol;
    use reoling::transport::connection::BcConnection;
    use reoling::transport::discovery::PeerHandle;
    use reoling::{ReolinkClient, StreamProfile, VideoFrame, VideoType};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::UdpSocket;

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

    /// An event the window hears, as read here.
    fn said(event: DeviceEvent) -> String {
        match event {
            DeviceEvent::PlayFailed { channel, reason, .. } => format!("PlayFailed {channel}: {reason}"),
            DeviceEvent::Playing => "Playing".to_string(),
            _ => "another event".to_string(),
        }
    }

    /// The events the window has had since last asked, as read here.
    fn heard(events: &async_channel::Receiver<DeviceEvent>) -> Vec<String> {
        std::iter::from_fn(|| events.try_recv().ok()).map(said).collect()
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

    const PLAIN: EncryptionProtocol = EncryptionProtocol::Unencrypted;

    /// A client and the device's end of a connection over UDP on the
    /// loopback (BCUDP, unencrypted, no login), as the client's own tests
    /// make one.
    async fn client_and_device() -> (ReolinkClient, BcConnection) {
        let client_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("a socket"));
        let device_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("a socket"));
        let peer = |addr, local_connection_id, remote_connection_id| PeerHandle {
            addr,
            local_connection_id,
            remote_connection_id,
            nonce: None,
            is_direct: false,
        };
        let (client_addr, device_addr) = (client_socket.local_addr(), device_socket.local_addr());
        let device = BcConnection::new(device_socket, peer(client_addr.expect("an address"), 2, 1));
        let connection = BcConnection::new(client_socket, peer(device_addr.expect("an address"), 1, 2));
        (ReolinkClient::from_connection(connection, PLAIN), device)
    }

    /// A message of the device's under the number of `request`, code 200.
    fn device_message(request: &BcMeta, extension: Option<Vec<u8>>, payload: Option<Vec<u8>>) -> Bc {
        let meta = BcMeta { response_code: 200, class: 0x0000, ..request.clone() };
        Bc { meta, body: BcBody::Modern(ModernMsg { extension_xml: extension, payload }) }
    }

    /// One BcMedia key frame of `codec`, without an extra header.
    fn key_frame(codec: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut unit = 0x6364_3030u32.to_le_bytes().to_vec();
        unit.extend_from_slice(codec);
        unit.extend_from_slice(&(data.len() as u32).to_le_bytes());
        unit.extend_from_slice(&0u32.to_le_bytes());
        unit.extend_from_slice(&0u32.to_le_bytes());
        unit.extend_from_slice(&0u32.to_le_bytes());
        unit.extend_from_slice(data);
        unit.resize(unit.len() + (8 - data.len() % 8) % 8, 0);
        unit
    }

    /// The device's end: a Preview is answered and its stream starts with an
    /// H.265 key frame, a stop is answered. Every request but the idle
    /// keepalive goes to `asked`; the logout ends it.
    async fn h265_camera(mut device: BcConnection, asked: tokio::sync::mpsc::UnboundedSender<BcMeta>) {
        while let Ok(request) = device.recv_bc(&PLAIN).await {
            let meta = request.meta;
            if meta.msg_id == MSG_ID_LINK_TYPE {
                continue;
            }
            let _ = asked.send(meta.clone());
            match meta.msg_id {
                MSG_ID_VIDEO => {
                    device.send_bc(&device_message(&meta, None, None), &PLAIN).await.expect("the answer sent");
                    let binary = Extension {
                        version: XML_VERSION.to_string(),
                        binary_data: Some(1),
                        channel_id: Some(meta.channel_id),
                        ..Default::default()
                    };
                    let frame = key_frame(b"H265", &[0, 0, 0, 1, 0x40, 0x01]);
                    let media = device_message(&meta, Some(binary.to_bytes()), Some(frame));
                    device.send_bc(&media, &PLAIN).await.expect("the frame sent");
                }
                MSG_ID_VIDEO_STOP => {
                    device.send_bc(&device_message(&meta, None, None), &PLAIN).await.expect("the answer sent")
                }
                MSG_ID_LOGOUT => return,
                _ => {}
            }
        }
    }

    /// `future`'s output, if it comes within 5 s.
    async fn soon<T>(future: impl std::future::Future<Output = T>, what: &str) -> T {
        tokio::time::timeout(Duration::from_secs(5), future).await.unwrap_or_else(|_| panic!("no {what} within 5 s"))
    }

    #[tokio::test]
    async fn after_a_refused_live_sink_the_connection_serves_the_next_command() {
        // The connection's own loop (`serve`) with a client on the loopback,
        // and the window's live responder of the test above.
        let (requests, mut incoming) = tokio::sync::mpsc::channel::<SinkRequest>(8);
        let window = std::thread::spawn(move || {
            let view = View::live(Some(VideoType::H265));
            while let Some((video_type, reply)) = incoming.blocking_recv() {
                let _ = reply.send(view.ensure_sink(video_type));
            }
            view.reset();
        });
        let (mut client, device) = client_and_device().await;
        let (asked_tx, mut asked) = tokio::sync::mpsc::unbounded_channel();
        let device = tokio::spawn(h265_camera(device, asked_tx));
        let updates = client.take_channel_updates().expect("taken once per client");
        let audio = client.take_audio().expect("taken once per client");
        let (commands, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (tx, events) = async_channel::unbounded();
        let (replay_requests, _replay_incoming) = tokio::sync::mpsc::channel(1);
        let (gen, audio_out) = (Arc::new(AtomicU64::new(0)), Arc::new(AudioOutput::new(1.0)));
        let (replays, out) = (replay_requests, audio_out);
        let (epoch, carried) = (reoling::TrialGate::default(), Carried::default());
        let connection =
            serve(client, command_rx, tx, updates, audio, gen, requests, replays, out, false, epoch, carried);
        tokio::pin!(connection);
        let script = async {
            // Play: the stream's first frame is H.265, which this system
            // cannot show. The window hears why...
            commands.send(Command::Play { channel: CHANNEL, profile: StreamProfile::Sub }).expect("a connection");
            let failed = soon(events.recv(), "event").await.expect("the connection's events");
            assert_eq!(said(failed), format!("PlayFailed {CHANNEL}: Cannot show this stream: h265parse is missing."));
            // ...and the next command reaches the device.
            commands.send(Command::QueryEncoding { channel: CHANNEL }).expect("the connection takes commands");
            let mut seen: Vec<BcMeta> = Vec::new();
            while seen.last().map(|meta| meta.msg_id) != Some(MSG_ID_GET_COMPRESSION) {
                seen.push(soon(asked.recv(), "request").await.expect("the device"));
            }
            seen
        };
        let seen = tokio::select! {
            () = &mut connection => panic!("the connection ended after a refused stream"),
            seen = script => seen,
        };
        // The device was told to stop the stream, once, before the next
        // command; no logout.
        let requests: Vec<(u32, u8)> = seen.iter().map(|meta| (meta.msg_id, meta.channel_id)).collect();
        let stopped = [(MSG_ID_VIDEO, CHANNEL), (MSG_ID_VIDEO_STOP, CHANNEL), (MSG_ID_GET_COMPRESSION, CHANNEL)];
        assert_eq!(requests, stopped);
        assert!(heard(&events).is_empty());
        // The session ends with `Shutdown` only, on the device too.
        commands.send(Command::Shutdown).expect("the connection takes commands");
        soon(connection, "end of the connection").await;
        assert_eq!(soon(asked.recv(), "logout").await.map(|meta| meta.msg_id), Some(MSG_ID_LOGOUT));
        soon(device, "end of the device").await.expect("the device");
        assert!(heard(&events).is_empty());
        window.join().expect("the window's thread");
    }

    /// A device that refuses every Preview, holding back its answer to the
    /// first one until `go` is sent or dropped: the test sends its command
    /// meanwhile.
    async fn refusing_camera(
        mut device: BcConnection,
        asked: tokio::sync::mpsc::UnboundedSender<BcMeta>,
        go: tokio::sync::oneshot::Receiver<()>,
    ) {
        let mut go = Some(go);
        while let Ok(request) = device.recv_bc(&PLAIN).await {
            let meta = request.meta;
            if meta.msg_id == MSG_ID_LINK_TYPE {
                continue;
            }
            let _ = asked.send(meta.clone());
            match meta.msg_id {
                MSG_ID_VIDEO => {
                    if let Some(go) = go.take() {
                        let _ = go.await;
                    }
                    let mut refusal = device_message(&meta, None, None);
                    refusal.meta.response_code = 400;
                    device.send_bc(&refusal, &PLAIN).await.expect("the answer sent");
                }
                MSG_ID_LOGOUT => return,
                _ => {}
            }
        }
    }

    /// A link with no connection behind it: the test reads its commands (`Commands::said`).
    pub(crate) fn listened() -> (DeviceLink, Commands) {
        let (commands, sent) = tokio::sync::mpsc::unbounded_channel();
        let (_, events) = async_channel::unbounded();
        (DeviceLink { events, commands, playback_gen: Arc::new(AtomicU64::new(0)) }, Commands(sent))
    }

    /// What a `listened` link was told.
    pub(crate) struct Commands(tokio::sync::mpsc::UnboundedReceiver<Command>);

    impl Commands {
        /// The commands sent since last asked, in order: the ones the window's paths send by name, the rest as
        /// "another command".
        pub(crate) fn said(&mut self) -> Vec<String> {
            std::iter::from_fn(|| self.0.try_recv().ok())
                .map(|command| match command {
                    Command::Play { channel, profile } => format!("Play {channel} {profile:?}"),
                    Command::CancelTrial => "CancelTrial".to_string(),
                    Command::TalkStop { channel } => format!("TalkStop {channel}"),
                    Command::Stop => "Stop".to_string(),
                    Command::Shutdown => "Shutdown".to_string(),
                    _ => "another command".to_string(),
                })
                .collect()
        }
    }

    /// One device's connection as the window drives it: its link, the connection's own loop (`serve`) on the
    /// loopback, and a `refusing_camera` that answers its first Preview only when `answer` (else never).
    pub(crate) struct Bench {
        pub(crate) link: DeviceLink,
        asked: tokio::sync::mpsc::UnboundedReceiver<BcMeta>,
        connection: std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>,
        device: tokio::task::JoinHandle<()>,
        go: Option<tokio::sync::oneshot::Sender<()>>,
        _sinks: (tokio::sync::mpsc::Receiver<SinkRequest>, tokio::sync::mpsc::Receiver<super::ReplaySinkRequest>),
    }

    impl Bench {
        /// `epoch`: the device's trial number (see `spawn_device`); `carried`: what the connection serves first.
        pub(crate) async fn new(
            epoch: reoling::TrialGate,
            answer: bool,
            legacy_cipher: bool,
            carried: Carried,
        ) -> Self {
            let (requests, incoming) = tokio::sync::mpsc::channel::<SinkRequest>(1);
            let (replays, replay_incoming) = tokio::sync::mpsc::channel(1);
            let (mut client, device) = client_and_device().await;
            let (asked_tx, asked) = tokio::sync::mpsc::unbounded_channel();
            let (go, go_rx) = tokio::sync::oneshot::channel();
            let go = if answer { drop(go); None } else { Some(go) };
            let device = tokio::spawn(refusing_camera(device, asked_tx, go_rx));
            let updates = client.take_channel_updates().expect("taken once per client");
            let audio = client.take_audio().expect("taken once per client");
            let (commands, command_rx) = tokio::sync::mpsc::unbounded_channel();
            let (tx, events) = async_channel::unbounded();
            let (gen, out) = (Arc::new(AtomicU64::new(0)), Arc::new(AudioOutput::new(1.0)));
            let link = DeviceLink { events, commands, playback_gen: Arc::clone(&gen) };
            let connection = Box::pin(serve(
                client, command_rx, tx, updates, audio, gen, requests, replays, out, legacy_cipher, epoch, carried,
            ));
            Self { link, asked, connection, device, go, _sinks: (incoming, replay_incoming) }
        }

        /// A second handle on the link, as the window keeps it (see `Wiring`): the same queue and events. Dropped, it
        /// sends `Shutdown` as the window's does.
        pub(crate) fn window_link(&self) -> DeviceLink {
            let (events, commands) = (self.link.events.clone(), self.link.commands.clone());
            DeviceLink { events, commands, playback_gen: Arc::clone(&self.link.playback_gen) }
        }

        /// The device's next request, the connection running meanwhile.
        pub(crate) async fn next_request(&mut self) -> BcMeta {
            tokio::select! {
                () = &mut self.connection => panic!("the connection ended"),
                meta = soon(self.asked.recv(), "request") => meta.expect("the device"),
            }
        }

        /// Runs the connection to its end within `limit`, then the device's; the requests the device had since
        /// last asked.
        pub(crate) async fn end_within(&mut self, limit: Duration) -> Vec<u32> {
            tokio::time::timeout(limit, &mut self.connection).await.expect("the connection ended in time");
            // A held answer comes now, to a connection that has gone: the device reads the logout after it.
            self.go.take();
            soon(&mut self.device, "end of the device").await.expect("the device");
            std::iter::from_fn(|| self.asked.try_recv().ok()).map(|meta| meta.msg_id).collect()
        }
    }

    /// The window cancels a trial whose command the connection has not read yet, the first one or the one after
    /// the cipher step's reconnection: no Preview of it goes out, in 200 runs.
    #[tokio::test]
    async fn a_trial_cancelled_before_it_is_read_sends_no_preview() {
        let mut previews = 0;
        for run in 0..200 {
            let epoch = reoling::TrialGate::default();
            let after_cipher = run % 2 == 1;
            let mut bench = Bench::new(epoch.clone(), true, after_cipher, Carried::default()).await;
            if after_cipher {
                bench.link.try_live_after_cipher(CHANNEL, StreamProfile::Main, 0);
            } else {
                bench.link.try_live(CHANNEL, StreamProfile::Main, 0);
            }
            // The window's cancel (`Trials::cancel`, then `DeviceLink::cancel_trial`), and closing.
            epoch.cancel();
            bench.link.cancel_trial();
            bench.link.shutdown();
            let asked = bench.end_within(Duration::from_secs(5)).await;
            previews += asked.iter().filter(|id| **id == MSG_ID_VIDEO).count();
            assert_eq!(asked.last(), Some(&MSG_ID_LOGOUT), "run {run}: {asked:?}");
        }
        assert_eq!(previews, 0, "Previews of a cancelled trial");
    }

    /// A trial cancelled, or the window closed, while the device leaves a form unanswered: the connection does not
    /// wait for the form's 10 s but logs out at once. Cancelled, it makes way for an ordinary connection; closed, the
    /// window hears nothing more.
    #[tokio::test]
    async fn a_cancel_during_an_unanswered_form_logs_out_at_once() {
        for closing in [false, true] {
            let epoch = reoling::TrialGate::default();
            let mut bench = Bench::new(epoch.clone(), false, false, Carried::default()).await;
            bench.link.try_live(CHANNEL, StreamProfile::Main, 0);
            assert_eq!(bench.next_request().await.msg_id, MSG_ID_VIDEO);
            let cancelled = std::time::Instant::now();
            epoch.cancel();
            bench.link.cancel_trial();
            if closing {
                bench.link.shutdown();
            }
            assert_eq!(bench.end_within(Duration::from_secs(2)).await, [MSG_ID_LOGOUT], "closing {closing}");
            assert!(cancelled.elapsed() < Duration::from_secs(2));
            let event = bench.link.events.try_recv().ok();
            if closing {
                assert!(event.is_none(), "nothing after the window closed");
            } else {
                let event = event.expect("the window hears it");
                assert!(matches!(event, DeviceEvent::TrialReconnect { legacy: false, message: None, .. }));
            }
        }
    }

    /// Stop while a trial's first form is under way, with a command given meanwhile: no further form goes out, the
    /// connection, mid-reply, is closed, and the next connection serves both commands, in order.
    #[tokio::test]
    async fn commands_given_during_a_trial_run_on_the_next_connection() {
        let epoch = reoling::TrialGate::default();
        let mut bench = Bench::new(epoch.clone(), false, false, Carried::default()).await;
        bench.link.try_live(CHANNEL, StreamProfile::Main, 0);
        assert_eq!(bench.next_request().await.msg_id, MSG_ID_VIDEO);
        bench.link.query_encoding(CHANNEL);
        bench.link.stop();
        bench.link.query_presets(CHANNEL);
        assert_eq!(bench.end_within(Duration::from_secs(2)).await, [MSG_ID_LOGOUT], "nothing but the logout");
        let Ok(DeviceEvent::TrialReconnect { legacy: false, message: None, carried }) = bench.link.events.try_recv()
        else {
            panic!("the window hears that the connection makes way for another");
        };
        // The window drops the old link before the next connection is made (see `MainWindow::connect`).
        drop(bench.link);
        let mut next = Bench::new(epoch, false, false, carried).await;
        assert_eq!(next.next_request().await.msg_id, MSG_ID_GET_COMPRESSION);
        assert_eq!(next.next_request().await.msg_id, reoling::protocol::bc::model::MSG_ID_GET_PTZ_PRESET);
        next.link.shutdown();
        assert_eq!(next.end_within(Duration::from_secs(2)).await, [MSG_ID_LOGOUT]);
    }

    /// A settings page's write given while a trial ran is not carried to the next connection: the camera window
    /// reports it as dropped when the trial's connection makes way (`CameraSettings::reconnecting`). A read is
    /// carried, and so is an encoding save, which the window still waits for.
    #[tokio::test]
    async fn a_write_the_window_reported_dropped_never_reaches_the_device() {
        use reoling::protocol::bc::model::MSG_ID_SET_COMPRESSION;
        use reoling::protocol::bc::settings_write::led_write;
        use reoling::{CameraPage, DeviceKind, LedChange, RawBody, SettingsTarget};
        const LED_READ: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<LedState version=\"1.1\">\n\
            <channelId>1</channelId>\n<ledVersion>0</ledVersion>\n<IRLedBrightness>0</IRLedBrightness>\n\
            <state>auto</state>\n<lightState>open</lightState>\n<doorbellLightState>close</doorbellLightState>\n\
            </LedState>\n</body>\n";
        let write = led_write(&RawBody::new(LED_READ), 1, &LedChange::default()).expect("the captured shape");
        let epoch = reoling::TrialGate::default();
        let mut bench = Bench::new(epoch.clone(), false, false, Carried::default()).await;
        bench.link.try_live(CHANNEL, StreamProfile::Main, 0);
        assert_eq!(bench.next_request().await.msg_id, MSG_ID_VIDEO);
        let target = SettingsTarget::Camera { channel: 1, page: CameraPage::LightSound };
        bench.link.write_settings(target, DeviceKind::Hub, 1, vec![write]);
        bench.link.query_encoding(CHANNEL);
        bench.link.set_encoding(CHANNEL, "<Compression/>".to_string());
        bench.link.stop();
        assert_eq!(bench.end_within(Duration::from_secs(2)).await, [MSG_ID_LOGOUT]);
        let Ok(DeviceEvent::TrialReconnect { carried, .. }) = bench.link.events.try_recv() else {
            panic!("the connection makes way for another");
        };
        drop(bench.link);
        let mut next = Bench::new(epoch, false, false, carried).await;
        assert_eq!(next.next_request().await.msg_id, MSG_ID_GET_COMPRESSION, "the read, not the LED write");
        assert_eq!(next.next_request().await.msg_id, MSG_ID_SET_COMPRESSION, "the encoding save");
        next.link.shutdown();
        assert_eq!(next.end_within(Duration::from_secs(2)).await, [MSG_ID_LOGOUT]);
    }

    /// Carried commands that no connection took within `CARRY_LIMIT` are dropped: they would run long after the user
    /// gave them.
    #[test]
    fn carried_commands_not_taken_in_time_are_dropped() {
        let since = std::time::Instant::now();
        let carried = || {
            let play = Command::Play { channel: CHANNEL, profile: StreamProfile::Sub };
            let deferred = std::collections::VecDeque::from([play]);
            Carried { deferred, rest: None, since: Some(since) }
        };
        let (_link, mut pending) = tokio::sync::mpsc::unbounded_channel();
        assert_eq!(carried().into_commands_at(&mut pending, since + Duration::from_secs(1)).0.len(), 1);
        let late = since + super::CARRY_LIMIT + Duration::from_millis(1);
        assert!(carried().into_commands_at(&mut pending, late).0.is_empty());
    }

    /// The time limit is applied when the next connection begins to serve the carried commands, after its login:
    /// a Play handed over within `CARRY_LIMIT` whose connection begins serving after the limit never starts.
    #[tokio::test]
    async fn carried_commands_past_the_limit_when_served_are_dropped() {
        for (age, plays) in [(Duration::ZERO, true), (super::CARRY_LIMIT - Duration::from_millis(300), false)] {
            let play = Command::Play { channel: CHANNEL, profile: StreamProfile::Sub };
            let since = std::time::Instant::now() - age;
            let deferred = std::collections::VecDeque::from([play]);
            let carried = Carried { deferred, rest: None, since: Some(since) };
            let mut next = Bench::new(reoling::TrialGate::default(), true, false, carried).await;
            // The connection logs in meanwhile; it serves nothing before it is polled.
            tokio::time::sleep(Duration::from_millis(600)).await;
            if plays {
                assert_eq!(next.next_request().await.msg_id, MSG_ID_VIDEO, "taken in time");
            }
            next.link.shutdown();
            assert_eq!(next.end_within(Duration::from_secs(2)).await, [MSG_ID_LOGOUT], "{age:?}");
        }
    }

    /// Carried commands still waiting behind an earlier one (a carried download holds the queue) are dropped once
    /// `CARRY_LIMIT` has passed; a command given on the new link behind them stays, and so do carried ones already
    /// served.
    #[test]
    fn carried_commands_still_waiting_at_the_limit_are_dropped() {
        let since = std::time::Instant::now();
        let play = || Command::Play { channel: CHANNEL, profile: StreamProfile::Sub };
        let mut deferred = std::collections::VecDeque::from([play(), Command::QueryPresets { channel: CHANNEL }]);
        let mut late = super::LateCarried { left: 1, since: Some(since) };
        late.prune(&mut deferred, since + super::CARRY_LIMIT - Duration::from_millis(1));
        assert_eq!(deferred.len(), 2, "within the limit");
        late.prune(&mut deferred, since + super::CARRY_LIMIT + Duration::from_millis(1));
        assert!(matches!(deferred.make_contiguous(), [Command::QueryPresets { .. }]), "the new link's command stays");
        late.prune(&mut deferred, since + super::CARRY_LIMIT * 2);
        assert_eq!(deferred.len(), 1);
        // One of two carried commands already served: only the one behind it is dropped, not the new link's.
        let mut deferred = std::collections::VecDeque::from([play(), Command::QueryPresets { channel: CHANNEL }]);
        let mut late = super::LateCarried { left: 2, since: Some(since) };
        late.took(&super::Incoming::Deferred);
        late.prune(&mut deferred, since + super::CARRY_LIMIT * 2);
        assert!(matches!(deferred.make_contiguous(), [Command::QueryPresets { .. }]));
        late.took(&super::Incoming::Received(None));
        assert_eq!(late.left, 0);
    }

    /// How a trial's end reaches the window: a timeout or a lost connection
    /// with its own message, the cipher step's refusals as an ordinary
    /// reconnection, the cancelled one not at all.
    #[tokio::test]
    async fn a_trial_end_tells_the_window_its_reason() {
        let (tx, events) = async_channel::unbounded();
        let mut frames = None;
        let reason = |end, after_cipher| {
            let (tx, mut frames) = (tx.clone(), None);
            async move {
                match super::after_trial(end, CHANNEL, after_cipher, &mut frames, &tx).await {
                    Some(DeviceEvent::TrialReconnect { legacy, message, .. }) => Some((legacy, message)),
                    Some(_) => panic!("another event"),
                    None => None,
                }
            }
        };
        assert_eq!(reason(reoling::TrialEnd::TimedOut, false).await, Some((false, Some(super::TRIAL_TIMED_OUT))));
        assert_eq!(reason(reoling::TrialEnd::Lost, true).await, Some((false, Some(super::TRIAL_LOST))));
        assert_eq!(reason(reoling::TrialEnd::Reconnect, false).await, Some((true, None)));
        assert_eq!(reason(reoling::TrialEnd::NoneAccepted, true).await, Some((false, Some(super::NO_FORM_ACCEPTED))));
        assert_eq!(reason(reoling::TrialEnd::Cancelled, false).await, None);
        assert_eq!(reason(reoling::TrialEnd::Cancelled, true).await, Some((false, None)));
        assert!(heard(&events).is_empty());
        assert!(super::after_trial(reoling::TrialEnd::NoneAccepted, CHANNEL, false, &mut frames, &tx).await.is_none());
        assert_eq!(heard(&events), [format!("PlayFailed {CHANNEL}: {}", super::NO_FORM_ACCEPTED)]);
    }

    /// The connection made for a trial's legacy cipher step fails: the log
    /// says the trial ended; an ordinary connection's failure says nothing
    /// of a trial.
    #[test]
    fn a_failed_cipher_step_connection_ends_the_trial_in_the_log() {
        // The only test of the application that opens the log (the library's run in another process).
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/diag-tests/cipher-step");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("reoling-diagnostic.log");
        reoling::diag::set_file(Some(path.clone()));
        // A loopback port nobody listens on: the connection is refused at once.
        let port = std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).expect("a port").port();
        for legacy_cipher in [false, true] {
            let (requests, _incoming) = tokio::sync::mpsc::channel::<SinkRequest>(1);
            let (replay_requests, _replay_incoming) = tokio::sync::mpsc::channel(1);
            let target = super::ConnectTarget::Ip { addr: std::net::Ipv4Addr::LOCALHOST.into(), port };
            let link = super::spawn_device(
                target,
                "user".to_string(),
                "secret".to_string(),
                requests,
                replay_requests,
                Arc::new(AudioOutput::new(1.0)),
                7,
                legacy_cipher,
                reoling::TrialGate::default(),
                Carried::default(),
            );
            let event = link.events.recv_blocking().expect("an event");
            assert!(matches!(event, DeviceEvent::Lost(_)), "{}", said(event));
        }
        reoling::diag::set_file(None);
        let file = std::fs::read_to_string(&path).expect("the log");
        let ended: Vec<&str> = file.lines().filter(|l| l.contains("[dev 7]") && l.contains("TRIAL ended")).collect();
        assert_eq!(ended.len(), 1, "{file}");
        assert!(ended[0].ends_with("TRIAL ended: the connection for the legacy cipher step failed"), "{file}");
        // Tied to its connection, by the number the attempt was given.
        assert!(ended[0].contains("[dev 7] [conn "), "{file}");
        let failed: Vec<&str> = file.lines().filter(|l| l.contains("connecting to the device failed")).collect();
        assert_eq!(failed.len(), 2, "{file}");
        assert!(failed.iter().all(|l| l.contains("[dev 7] [conn ")), "{failed:?}");
    }

    /// A trial cancelled while its legacy cipher step's connection is made:
    /// that connection ends on the device and makes way for an ordinary one.
    #[tokio::test]
    async fn a_cancelled_trial_ends_the_connection_armed_for_its_cipher_step() {
        let mut bench = Bench::new(reoling::TrialGate::default(), false, true, Carried::default()).await;
        bench.link.cancel_trial();
        assert_eq!(bench.end_within(Duration::from_secs(5)).await, [MSG_ID_LOGOUT], "closed on the device, no form");
        let event = bench.link.events.try_recv().expect("the window hears it");
        assert!(matches!(event, DeviceEvent::TrialReconnect { legacy: false, message: None, .. }), "{}", said(event));
    }
}
