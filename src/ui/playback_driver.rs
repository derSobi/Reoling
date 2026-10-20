//! Recorded-video playback on a device's network thread.
//!
//! The Playback page only states what it wants (a day, a seek, play or
//! pause, a speed, a stream); `bridge.rs` hands every such command to the
//! `PlaybackDriver` of that device's thread. The driver records the intent,
//! and unless a newer command already supersedes it, turns the sum of all
//! intents into the client's searches, opens, resumes and closes, pushes the
//! footage (video and, at 1x, its audio) into the replay sink and reports
//! the player's state back (only when it changed). It is the only code that
//! calls `reset_for_open`, `chain_open`, `set_paused` and `end_of_footage`
//! on the replay sink, as those must run on the thread that pushes the
//! frames.
//!
//! Times: everything the page sees is device-local, exactly as the device
//! lists it. The replay sink's timeline is the device-local time in
//! microseconds on the `CivilTime::to_epoch_seconds` scale (see
//! [`timeline_of`]); a zone-free count, so it never needs the host's zone.

use crate::ui::bridge::{DeviceEvent, ReplaySinkRequest};
use crate::ui::video_view::{ReplaySinkStatus, VideoSink};
use reoling::{
    file_for_time, next_file, CivilTime, RecordStream, RecordedEvent, RecordingFile, ReolinkClient, ReplayResume,
    ReplaySpeed, ReplayStatus, ReplayStream, ReplayUnit, VideoFrame, VideoType,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

/// How often the driver looks at the running playback (end of a file, a
/// stall, a slow-motion backlog) while a session exists.
pub(crate) const PLAYBACK_TICK: Duration = Duration::from_millis(250);

/// A stalled path (footage came, then silence, short of the file's end) is
/// given this long before the file is opened again: a relay can pause for
/// tens of seconds and then go on.
const STALL_REOPEN_AFTER: Duration = Duration::from_secs(20);

/// After a slow-motion backlog the device is asked to send again once the
/// sink holds no more than this much footage: at 0.5x or slower that is 16 s
/// or more of wall time, enough for a slow open over a relay.
const BACKLOG_RESUME_BELOW: Duration = Duration::from_secs(8);

/// Less footage than this waiting in the sink, while more is still to come
/// for the open file, shows as buffering. The 1x lead is 2-10 s, so this
/// only trips once it has run dry.
const BUFFERING_BELOW: Duration = Duration::from_millis(300);

/// Automatic hops (the next file, a reopen) that fail in a row before the
/// driver gives up and pauses.
const AUTO_SKIP_LIMIT: u32 = 3;

/// How long a close waits for the device's answer when nothing is opened
/// right after it (ending the session, a new day search): the answer only
/// confirms the close, and trailing data are dropped by sequence number.
const CLOSE_WAIT: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// What the page sees
// ---------------------------------------------------------------------------

/// One recorded file of the selected day, as the page needs it (no `Id`, no
/// camera UID).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSpan {
    /// Device-local start.
    pub start: CivilTime,
    /// Device-local end, exclusive.
    pub end: CivilTime,
    pub encrypted: bool,
}

impl From<&RecordingFile> for FileSpan {
    fn from(file: &RecordingFile) -> Self {
        FileSpan { start: file.start, end: file.end, encrypted: file.encrypted }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerPhase {
    /// The session has its files (or none); nothing opened.
    Idle,
    /// A seek or open went out; its first frame has not been pushed yet.
    Opening,
    /// Frames are being pushed (also while the device's sending is held for
    /// a slow-motion backlog or a calendar search).
    Playing,
    /// Paused by the user (or after a refused open); where to go on is kept.
    Paused,
    /// The day's last file has ended; the sink plays out what it holds.
    Ended,
    /// The frame stream failed; the connection is lost.
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerState {
    pub session: u64,
    /// The link's playback generation of the newest command the device
    /// thread has taken up (superseded ones included). A state below the
    /// generation of the page's last Play, Pause or seek was sent before
    /// that command was seen, so its phase does not tell what plays now.
    pub gen: u64,
    pub phase: PlayerPhase,
    pub buffering: bool,
    /// Index into the session's last `Day` files of what is open or held.
    pub file: Option<usize>,
    pub speed: ReplaySpeed,
    pub stream: RecordStream,
    /// The last seek, open or pause target (device-local); the time label
    /// shows it while there is no picture.
    pub at: Option<CivilTime>,
    /// Width x height from the opening's info block.
    pub picture: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackEvent {
    /// Answer to `playback_month`: the recorded days (1-based), or why not.
    Month { channel: u8, year: u16, month: u8, days: Result<Vec<u8>, String> },
    /// Answer to `playback_day` (and the search again after a stream
    /// change): the day's files for `stream`, sorted, each once, or why not.
    Day { session: u64, channel: u8, day: CivilTime, stream: RecordStream, files: Result<Vec<FileSpan>, String> },
    /// The day's event windows for `stream`, listed after its `Day` (the
    /// recordings show first, the events when they arrive): sorted by
    /// start, each window once with all its kinds, or why not. A day
    /// without recordings has none.
    Events {
        session: u64,
        channel: u8,
        day: CivilTime,
        stream: RecordStream,
        events: Result<Vec<RecordedEvent>, String>,
    },
    /// The player changed (sent on change only).
    State(PlayerState),
    /// Something the user should read; the player stays usable.
    Error { session: u64, message: String },
}

/// The page's intents, in the order it sent them. `gen` is the link's
/// playback generation at sending (see `DeviceLink`): a command whose `gen`
/// is below the latest one is superseded, so only its intent is recorded.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PlaybackCommand {
    Month { channel: u8, year: u16, month: u8 },
    Day { gen: u64, session: u64, channel: u8, day: CivilTime, stream: RecordStream },
    Seek { gen: u64, at: CivilTime },
    Play { gen: u64 },
    Pause { gen: u64 },
    Speed { gen: u64, speed: ReplaySpeed },
    Stream { gen: u64, stream: RecordStream },
    Close { gen: u64 },
}

impl PlaybackCommand {
    /// The generation it was sent with (`None` for `Month`, which never
    /// supersedes and is never superseded).
    fn gen(&self) -> Option<u64> {
        match *self {
            PlaybackCommand::Month { .. } => None,
            PlaybackCommand::Day { gen, .. }
            | PlaybackCommand::Seek { gen, .. }
            | PlaybackCommand::Play { gen }
            | PlaybackCommand::Pause { gen }
            | PlaybackCommand::Speed { gen, .. }
            | PlaybackCommand::Stream { gen, .. }
            | PlaybackCommand::Close { gen } => Some(gen),
        }
    }
}

// ---------------------------------------------------------------------------
// Timeline helpers (the sink's timeline and the page's seconds of the day)
// ---------------------------------------------------------------------------

/// The replay sink's timeline value of a device-local time.
pub fn timeline_of(t: CivilTime) -> Duration {
    Duration::from_secs(u64::try_from(t.to_epoch_seconds()).unwrap_or(0))
}

/// The device-local time (whole second, floor) of a timeline value.
pub fn civil_of(timeline: Duration) -> CivilTime {
    CivilTime::from_epoch_seconds(i64::try_from(timeline.as_secs()).unwrap_or(i64::MAX))
}

/// Seconds since the device-local midnight of `day` (below 0 or above
/// 86 400 for footage of a file that crosses midnight; only drawing clamps).
pub fn seconds_of_day(timeline: Duration, day: CivilTime) -> f64 {
    let micros = i64::try_from(timeline.as_micros()).unwrap_or(i64::MAX);
    let midnight = midnight_of(day).to_epoch_seconds() * 1_000_000;
    micros.saturating_sub(midnight) as f64 / 1e6
}

/// The device-local time `seconds` after the midnight of `day` (floor).
pub fn civil_at(day: CivilTime, seconds: f64) -> CivilTime {
    let whole = if seconds.is_finite() { seconds.floor() as i64 } else { 0 };
    midnight_of(day).add_seconds(whole)
}

fn midnight_of(day: CivilTime) -> CivilTime {
    CivilTime::start_of_day(day.year, day.month, day.day)
}

/// `hh:mm:ss`.
fn clock_text(t: CivilTime) -> String {
    format!("{:02}:{:02}:{:02}", t.hour, t.minute, t.second)
}

/// `YYYY-MM-DD hh:mm:ss`, for the debug log.
fn civil_text(t: CivilTime) -> String {
    format!("{:04}-{:02}-{:02} {}", t.year, t.month, t.day, clock_text(t))
}

fn debug_replay() -> bool {
    std::env::var("REOLING_DEBUG_REPLAY").is_ok()
}

// ---------------------------------------------------------------------------
// Intents
// ---------------------------------------------------------------------------

/// The sum of what the page asked for in this session.
#[derive(Debug, Clone, PartialEq)]
struct Want {
    session: u64,
    channel: u8,
    /// Midnight of the session's day; `None`: no session.
    day: Option<CivilTime>,
    stream: RecordStream,
    speed: ReplaySpeed,
    playing: bool,
    /// A seek not acted on yet.
    seek: Option<CivilTime>,
    /// Play was pressed since the last executed reconcile (it starts the
    /// day over once it has ended).
    play_pressed: bool,
}

impl Default for Want {
    fn default() -> Self {
        Want {
            session: 0,
            channel: 0,
            day: None,
            stream: RecordStream::Sub,
            speed: ReplaySpeed::X1,
            playing: false,
            seek: None,
            play_pressed: false,
        }
    }
}

/// Records a command's intent. Superseded commands get this far too, so the
/// command that does run sees every intent before it.
fn apply_intent(want: &mut Want, command: &PlaybackCommand) {
    match *command {
        PlaybackCommand::Month { .. } => {}
        PlaybackCommand::Day { session, channel, day, stream, .. } => {
            *want = Want {
                session,
                channel,
                day: Some(midnight_of(day)),
                stream,
                speed: want.speed,
                playing: false,
                seek: None,
                play_pressed: false,
            };
        }
        PlaybackCommand::Seek { at, .. } => {
            want.seek = Some(at);
            want.playing = true;
        }
        PlaybackCommand::Play { .. } => {
            want.playing = true;
            want.play_pressed = true;
        }
        PlaybackCommand::Pause { .. } => want.playing = false,
        PlaybackCommand::Speed { speed, .. } => want.speed = speed,
        PlaybackCommand::Stream { stream, .. } => want.stream = stream,
        PlaybackCommand::Close { .. } => *want = Want::default(),
    }
}

/// Whether a command sent with `gen` is superseded by a newer one (`latest`
/// being the link's generation now).
fn is_superseded(gen: u64, latest: u64) -> bool {
    gen < latest
}

/// Whether a new file list for `want` replaces `loaded` (channel, day,
/// stream) for the same day and channel, i.e. only the stream changed: the
/// position on screen then carries over to the new list.
fn carries_position(loaded: (u8, CivilTime, RecordStream), want: (u8, CivilTime, RecordStream)) -> bool {
    loaded.0 == want.0 && loaded.1 == want.1 && loaded.2 != want.2
}

// ---------------------------------------------------------------------------
// The plan: what to do about the intents (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WantSummary {
    pub playing: bool,
    pub seek: Option<CivilTime>,
    pub speed: ReplaySpeed,
    pub stream: RecordStream,
    pub has_files: bool,
    pub play_pressed: bool,
}

/// What runs on the device now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ActualSummary {
    /// Nothing open, nothing held (`ended`: after the day's last file).
    Idle { ended: bool },
    Open { speed: ReplaySpeed, stream: RecordStream },
    /// The device's playback is closed and where to go on is kept
    /// (`backlog`: held for a slow-motion backlog, which the tick resumes).
    Held { speed: ReplaySpeed, stream: RecordStream, backlog: bool },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Plan {
    Nothing,
    /// Nothing is recorded on the day: tell the user, play nothing.
    NoRecordings,
    /// Close, flush the sink, open at this time.
    OpenFresh(CivilTime),
    /// Close, flush the sink to this time and stay paused there.
    ParkAt(CivilTime),
    /// Close the running playback, keeping what the sink holds.
    PauseHold,
    /// Carry on from the hold, onto what the sink holds.
    ContinueHold,
    /// The day's first file at its start.
    FirstFile,
}

/// What to do so that the device does what `want` asks. `screen` is the
/// position on screen, where a speed change reopens.
pub(crate) fn plan(want: &WantSummary, actual: &ActualSummary, screen: CivilTime) -> Plan {
    if !want.has_files {
        return if want.playing || want.seek.is_some() { Plan::NoRecordings } else { Plan::Nothing };
    }
    if let Some(at) = want.seek {
        return if want.playing { Plan::OpenFresh(at) } else { Plan::ParkAt(at) };
    }
    if !want.playing {
        return match actual {
            ActualSummary::Open { .. } => Plan::PauseHold,
            _ => Plan::Nothing,
        };
    }
    let same = |speed: &ReplaySpeed, stream: &RecordStream| *speed == want.speed && *stream == want.stream;
    match actual {
        ActualSummary::Open { speed, stream } if same(speed, stream) => Plan::Nothing,
        ActualSummary::Open { .. } => Plan::OpenFresh(screen),
        ActualSummary::Held { speed, stream, backlog: true } if same(speed, stream) => Plan::Nothing,
        ActualSummary::Held { speed, stream, .. } if same(speed, stream) => Plan::ContinueHold,
        ActualSummary::Held { .. } => Plan::OpenFresh(screen),
        ActualSummary::Idle { ended: true } if !want.play_pressed => Plan::Nothing,
        ActualSummary::Idle { .. } => Plan::FirstFile,
    }
}

// ---------------------------------------------------------------------------
// The tick: what the running playback needs (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HoldReason {
    Paused,
    Backlog,
    Search,
}

pub(crate) struct TickInput<'a> {
    pub phase: PlayerPhase,
    pub open: Option<&'a ReplayStatus>,
    /// The open's first frame is still to come and carries straight on from
    /// what the sink holds (the next file, a resume).
    pub chained: bool,
    pub stall_retried: bool,
    pub hold: Option<HoldReason>,
    pub sink: Option<ReplaySinkStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TickAction {
    Nothing,
    /// The device has finished this file: on to the next one.
    FileEnded,
    /// The path stalled short of the file's end: open it again where it
    /// stopped.
    ReopenStalled,
    /// Slow motion piled up too much footage: close, play on from the sink.
    HoldForBacklog,
    /// The backlog has drained: carry on.
    ResumeHold,
}

/// The tick's decision, and whether the player is buffering.
pub(crate) fn decide(input: &TickInput) -> (TickAction, bool) {
    let buffering = buffering_of(input);
    if let Some(open) = input.open {
        if open.at_end() {
            return (TickAction::FileEnded, buffering);
        }
        if open.stalled() && open.silent_for >= STALL_REOPEN_AFTER {
            let action = if input.stall_retried { TickAction::FileEnded } else { TickAction::ReopenStalled };
            return (action, buffering);
        }
        if input.sink.is_some_and(|sink| sink.slow_motion_backlog) {
            return (TickAction::HoldForBacklog, buffering);
        }
    }
    if input.hold == Some(HoldReason::Backlog) && input.sink.is_some_and(|sink| sink.buffered <= BACKLOG_RESUME_BELOW) {
        return (TickAction::ResumeHold, buffering);
    }
    (TickAction::Nothing, buffering)
}

/// Whether the picture is waiting for footage.
fn buffering_of(input: &TickInput) -> bool {
    let dry = input.sink.map_or(true, |sink| sink.buffered < BUFFERING_BELOW);
    match input.phase {
        // A chained open keeps showing what the sink holds meanwhile.
        PlayerPhase::Opening => !input.chained || dry,
        PlayerPhase::Playing => {
            input.open.is_some_and(|open| open.stalled())
                || input.hold == Some(HoldReason::Search)
                || (dry && input.open.is_some_and(|open| !open.delivered_to_end()))
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Holds and first frames (pure)
// ---------------------------------------------------------------------------

/// Where to go on once the device's playback has been closed.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Continue {
    /// The same file again where the opening stopped, dropping what it
    /// repeats; `fallback` places the first frame when the key frames carry
    /// no time.
    Resume { index: usize, resume: ReplayResume, fallback: CivilTime },
    /// The opening reached its file's end: the next file.
    Next { index: usize },
    /// Nothing of the opening was shown: open at `at`. `chain` when what
    /// the sink holds leads straight up to `at` (an opening of the next
    /// file paused before its first frame).
    Fresh { at: CivilTime, chain: bool },
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Hold {
    next: Continue,
    speed: ReplaySpeed,
    stream: RecordStream,
    reason: HoldReason,
}

/// Where to go on after closing an opening of file `index` whose progress
/// (taken after every frame was drained) is `status`. `chain_pending`: the
/// opening's first frame was to carry on from what the sink holds.
fn continuation(status: &ReplayStatus, index: usize, chain_pending: bool) -> Continue {
    if status.frames == 0 && status.skip_through_microseconds.is_none() {
        return Continue::Fresh { at: status.seek_time, chain: chain_pending };
    }
    if status.delivered_to_end() {
        return Continue::Next { index };
    }
    // Also an opening that only repeated what an earlier one showed: its
    // resume point is that earlier one's.
    Continue::Resume { index, resume: status.resume_point(), fallback: status.position() }
}

/// Where an opening's first frame (footage time `microseconds`) lies on the
/// sink's timeline: its real time from the key frames' own times and the
/// device's zone, else `fallback` (the time it was opened at).
fn first_frame_timeline(status: &ReplayStatus, microseconds: u32, fallback: CivilTime) -> Duration {
    status
        .local_micros_of(microseconds)
        .and_then(|us| u64::try_from(us).ok())
        .map(Duration::from_micros)
        .unwrap_or_else(|| timeline_of(fallback))
}

/// The response code in one of the client's refusal messages ("... (code
/// 431)").
fn refusal_code(message: &str) -> Option<u16> {
    let rest = &message[message.find("(code ")? + "(code ".len()..];
    rest[..rest.find(')')?].trim().parse().ok()
}

/// What the user reads when a recording could not be opened.
fn open_failure_text(error: &str, encrypted: bool) -> String {
    match refusal_code(error) {
        Some(code) if encrypted => format!("This recording is encrypted (code {code})"),
        Some(code) => format!("The device refused to play this recording (code {code})"),
        None => format!("Could not play this recording: {error}"),
    }
}

// ---------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------

/// The day's files as the client listed them (they carry the `Id`s: never
/// print this).
struct LoadedDay {
    channel: u8,
    day: CivilTime,
    stream: RecordStream,
    files: Vec<RecordingFile>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FirstFrame {
    /// Start the sink's timeline again (it was flushed, or is new).
    Reset { fallback: CivilTime },
    /// Carry on after what the sink holds.
    Chain { fallback: CivilTime },
    Done,
}

/// The running opening.
struct Open {
    session: u64,
    index: usize,
    stream: ReplayStream,
    speed: ReplaySpeed,
    kind: RecordStream,
    first: FirstFrame,
    stall_retried: bool,
    /// The frame stream has ended (its reader stopped); nothing to poll.
    exhausted: bool,
}

/// What `close_and_drain` leaves of an opening.
struct Closed {
    index: usize,
    speed: ReplaySpeed,
    kind: RecordStream,
    chain_pending: bool,
    status: ReplayStatus,
}

/// Who asked for an open: the user (a failure pauses with a notice) or the
/// driver itself (a failure skips to the next file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    User,
    Auto,
}

pub(crate) struct PlaybackDriver {
    gen: Arc<AtomicU64>,
    /// The generation of the newest command taken up (see
    /// `PlayerState::gen`).
    applied_gen: u64,
    sinks: tokio::sync::mpsc::Sender<ReplaySinkRequest>,
    want: Want,
    files: Option<LoadedDay>,
    open: Option<Open>,
    hold: Option<Hold>,
    phase: PlayerPhase,
    sink: Option<(VideoType, VideoSink)>,
    /// A frame went into the sink since its last `reset_for_open`.
    pushed_since_reset: bool,
    auto_failures: u32,
    last_sent: Option<PlayerState>,
    /// The last seek, open or pause target.
    at: Option<CivilTime>,
    /// The file last opened or held.
    file: Option<usize>,
    picture: Option<(u32, u32)>,
    /// The page is owed a `Day` event (a new session, a stream change).
    day_owed: bool,
    /// The opening being asked for carries on from what the sink holds.
    opening_chained: bool,
}

type Events = async_channel::Sender<DeviceEvent>;

impl PlaybackDriver {
    pub(crate) fn new(gen: Arc<AtomicU64>, sinks: tokio::sync::mpsc::Sender<ReplaySinkRequest>) -> Self {
        PlaybackDriver {
            gen,
            sinks,
            applied_gen: 0,
            want: Want::default(),
            files: None,
            open: None,
            hold: None,
            phase: PlayerPhase::Idle,
            sink: None,
            pushed_since_reset: false,
            auto_failures: 0,
            last_sent: None,
            at: None,
            file: None,
            picture: None,
            day_owed: false,
            opening_chained: false,
        }
    }

    /// Whether a playback session exists (the tick runs then).
    pub(crate) fn is_active(&self) -> bool {
        self.want.day.is_some()
    }

    /// The running opening's units (video frames and audio), if there is one
    /// to read.
    pub(crate) fn units(&mut self) -> Option<&mut ReceiverStream<reoling::Result<ReplayUnit>>> {
        self.open.as_mut().filter(|open| !open.exhausted).map(|open| &mut open.stream.units)
    }

    /// One command from the page.
    pub(crate) async fn command(&mut self, command: PlaybackCommand, client: &mut ReolinkClient, tx: &Events) {
        match command {
            PlaybackCommand::Month { channel, year, month } => {
                self.month(channel, year, month, client, tx).await;
                self.send_state(tx).await;
                return;
            }
            PlaybackCommand::Close { .. } => {
                self.end(client).await;
                return;
            }
            _ => {}
        }
        apply_intent(&mut self.want, &command);
        match command {
            PlaybackCommand::Day { .. } => {
                // The page starts every session on a fresh picture, so the
                // sink held so far is gone; the old session's frames are
                // dropped from here on (see `Open::session`).
                self.sink = None;
                self.pushed_since_reset = false;
                self.files = None;
                self.hold = None;
                self.phase = PlayerPhase::Idle;
                self.auto_failures = 0;
                self.at = None;
                self.file = None;
                self.picture = None;
                self.day_owed = true;
            }
            PlaybackCommand::Stream { .. } => self.day_owed = true,
            _ => {}
        }
        if let Some(gen) = command.gen() {
            self.applied_gen = gen;
        }
        let latest = self.gen.load(Ordering::SeqCst);
        if command.gen().is_some_and(|gen| is_superseded(gen, latest)) {
            return;
        }
        self.reconcile(client, tx).await;
        self.send_state(tx).await;
    }

    /// One unit (or the end, or an error) from the running opening.
    pub(crate) async fn on_unit(&mut self, item: Option<reoling::Result<ReplayUnit>>, tx: &Events) {
        let Some(mut open) = self.open.take() else { return };
        match item {
            Some(Ok(unit)) => {
                if open.session == self.want.session {
                    let was = self.phase;
                    self.push_unit(&mut open, unit).await;
                    self.open = Some(open);
                    if self.phase != was {
                        self.send_state(tx).await;
                    }
                } else {
                    self.open = Some(open);
                }
            }
            Some(Err(e)) => {
                // The connection failed; `wait_for_pushes` reports it as lost.
                self.phase = PlayerPhase::Failed;
                self.want.playing = false;
                self.hold = None;
                self.send_error(tx, format!("The recording broke off: {e}")).await;
                self.send_state(tx).await;
            }
            None => {
                open.exhausted = true;
                self.open = Some(open);
            }
        }
    }

    /// The periodic look at the running playback.
    pub(crate) async fn tick(&mut self, client: &mut ReolinkClient, tx: &Events) {
        let stale = self.open.as_ref().is_some_and(|open| open.session != self.want.session);
        if !stale {
            let status = self.open.as_ref().map(|open| open.stream.progress.status());
            let action = decide(&self.tick_input(status.as_ref())).0;
            match action {
                TickAction::Nothing => {}
                TickAction::FileEnded => {
                    if let Some(closed) = self.close_and_drain(client).await {
                        if debug_replay() {
                            eprintln!("playback: file {} ended", self.file_name(closed.index));
                        }
                        self.hop(Continue::Next { index: closed.index }, Origin::Auto, false, client, tx).await;
                    }
                }
                TickAction::ReopenStalled => {
                    if let Some(closed) = self.close_and_drain(client).await {
                        let next = continuation(&closed.status, closed.index, closed.chain_pending);
                        if debug_replay() {
                            eprintln!("playback: file {} stalled; opening it again", self.file_name(closed.index));
                        }
                        let retried = matches!(next, Continue::Resume { .. });
                        self.hop(next, Origin::Auto, retried, client, tx).await;
                    }
                }
                TickAction::HoldForBacklog => {
                    if let Some(closed) = self.close_and_drain(client).await {
                        self.hold_from(closed, HoldReason::Backlog);
                    }
                }
                TickAction::ResumeHold => self.continue_hold(Origin::Auto, client, tx).await,
            }
        }
        self.send_state(tx).await;
    }

    /// Ends the session: closes the device's playback with a short wait (no
    /// drain), drops the hold and the sink. Does nothing at all on the
    /// device when no playback is open.
    pub(crate) async fn end(&mut self, client: &mut ReolinkClient) {
        if self.open.is_some() || client.replay_progress().is_some() {
            let _ = client.stop_replay_within(CLOSE_WAIT).await;
        }
        *self = PlaybackDriver::new(Arc::clone(&self.gen), self.sinks.clone());
    }

    // ---- reconcile ----------------------------------------------------------

    /// Brings the device in line with the intents.
    async fn reconcile(&mut self, client: &mut ReolinkClient, tx: &Events) {
        if self.want.day.is_none() {
            return;
        }
        if !self.ensure_files(client, tx).await {
            self.apply_paused_state();
            return;
        }
        if self.day_owed {
            self.send_day(tx).await;
        }
        let has_files = self.files.as_ref().is_some_and(|day| !day.files.is_empty());
        let want = WantSummary {
            playing: self.want.playing,
            seek: self.want.seek,
            speed: self.want.speed,
            stream: self.want.stream,
            has_files,
            play_pressed: self.want.play_pressed,
        };
        let actual = match (&self.open, &self.hold) {
            (Some(open), _) => ActualSummary::Open { speed: open.speed, stream: open.kind },
            (None, Some(hold)) => {
                ActualSummary::Held { speed: hold.speed, stream: hold.stream, backlog: hold.reason == HoldReason::Backlog }
            }
            (None, None) => ActualSummary::Idle { ended: self.phase == PlayerPhase::Ended },
        };
        let screen = self.screen_position().or_else(|| self.first_start()).unwrap_or_default();
        let plan = plan(&want, &actual, screen);
        self.want.seek = None;
        self.want.play_pressed = false;
        match plan {
            Plan::Nothing => {
                if !self.want.playing {
                    if let Some(hold) = self.hold.as_mut() {
                        hold.reason = HoldReason::Paused;
                        self.phase = PlayerPhase::Paused;
                    }
                }
            }
            Plan::NoRecordings => {
                // Nothing can open: a position carried over from the other
                // stream (or an opening about to start) is dropped too.
                self.want.playing = false;
                self.hold = None;
                self.file = None;
                self.phase = PlayerPhase::Idle;
                self.send_error(tx, "No recordings on this day".to_string()).await;
            }
            Plan::OpenFresh(at) => self.open_fresh(at, client, tx).await,
            Plan::ParkAt(at) => self.park_at(at, client, tx).await,
            Plan::PauseHold => {
                // The page froze the picture already; keep it so while the
                // device's playback is closed and its last frames go in.
                if let Some((_, sink)) = &self.sink {
                    sink.set_paused(true);
                }
                let screen = self.screen_position();
                if let Some(closed) = self.close_and_drain(client).await {
                    self.hold_from(closed, HoldReason::Paused);
                }
                self.phase = PlayerPhase::Paused;
                self.at = self.screen_position().or(screen).or(self.at);
            }
            Plan::ContinueHold => self.continue_hold(Origin::User, client, tx).await,
            Plan::FirstFile => {
                if let Some(start) = self.first_start() {
                    self.open_fresh(start, client, tx).await;
                }
            }
        }
        self.apply_paused_state();
    }

    /// Lists the day's files for the wanted channel, day and stream unless
    /// they are loaded. A stream change keeps the position on screen as a
    /// hold (`Fresh`), since the other stream's files are other files; after
    /// the day's end it keeps nothing, so nothing plays until Play.
    /// `false` when the search failed (the page has been told).
    async fn ensure_files(&mut self, client: &mut ReolinkClient, tx: &Events) -> bool {
        let Some(day) = self.want.day else { return false };
        let wanted = (self.want.channel, day, self.want.stream);
        let loaded = self.files.as_ref().map(|f| (f.channel, f.day, f.stream));
        if loaded == Some(wanted) {
            return true;
        }
        let carried = loaded
            .filter(|&loaded| {
                carries_position(loaded, wanted) && self.at.is_some() && self.phase != PlayerPhase::Ended
            })
            .and_then(|_| self.screen_position());
        if carried.is_some() {
            // Hold the picture where it is while the other stream is listed.
            if let Some((_, sink)) = &self.sink {
                sink.set_paused(true);
            }
            if self.want.playing {
                self.phase = PlayerPhase::Opening;
                self.send_state(tx).await;
            }
        }
        if self.open.take().is_some() || client.replay_progress().is_some() {
            let _ = client.stop_replay_within(CLOSE_WAIT).await;
        }
        self.hold = None;
        self.files = None;
        match client.search_day(self.want.channel, day, self.want.stream).await {
            Ok(files) => {
                if debug_replay() {
                    eprintln!(
                        "playback: {} files on {:04}-{:02}-{:02} ({:?})",
                        files.len(),
                        day.year,
                        day.month,
                        day.day,
                        self.want.stream
                    );
                }
                self.files = Some(LoadedDay { channel: self.want.channel, day, stream: self.want.stream, files });
                self.day_owed = true;
                if let Some(at) = carried {
                    self.hold = Some(Hold {
                        next: Continue::Fresh { at, chain: false },
                        speed: self.want.speed,
                        stream: self.want.stream,
                        reason: HoldReason::Paused,
                    });
                    self.at = Some(at);
                    if !self.want.playing {
                        self.phase = PlayerPhase::Paused;
                    }
                }
                // The recordings go to the page first, then the events are
                // listed (a second or two more over a relay) before anything
                // opens: the device answers searches only while nothing plays.
                self.send_day(tx).await;
                self.send_events(client, tx).await;
                true
            }
            Err(e) => {
                self.day_owed = false;
                self.phase = PlayerPhase::Idle;
                self.want.playing = false;
                let event = PlaybackEvent::Day {
                    session: self.want.session,
                    channel: self.want.channel,
                    day,
                    stream: self.want.stream,
                    files: Err(e.to_string()),
                };
                let _ = tx.send(DeviceEvent::Playback(event)).await;
                false
            }
        }
    }

    /// Opens at `at` with a flushed sink (a seek, a speed or stream change).
    async fn open_fresh(&mut self, at: CivilTime, client: &mut ReolinkClient, tx: &Events) {
        let Some((index, from)) = self.files.as_ref().and_then(|day| file_for_time(&day.files, at)) else {
            self.nothing_after(tx).await;
            return;
        };
        // What the sink holds is from before the seek: gone now, and the
        // cursor shows where the new opening starts.
        self.open = None;
        self.hold = None;
        self.flush_to(from);
        let first = FirstFrame::Reset { fallback: from };
        if let Err(text) = self.start(index, ReplayResume::at(from), first, false, client, tx).await {
            self.send_error(tx, text).await;
            self.pause_with(Continue::Fresh { at: from, chain: false });
        }
    }

    /// Stays paused at `at`: the sink flushed to it, nothing open.
    async fn park_at(&mut self, at: CivilTime, client: &mut ReolinkClient, tx: &Events) {
        let Some((index, from)) = self.files.as_ref().and_then(|day| file_for_time(&day.files, at)) else {
            self.nothing_after(tx).await;
            return;
        };
        if self.open.take().is_some() || client.replay_progress().is_some() {
            let _ = client.stop_replay_within(CLOSE_WAIT).await;
        }
        self.flush_to(from);
        self.file = Some(index);
        self.at = Some(from);
        self.pause_with(Continue::Fresh { at: from, chain: false });
    }

    /// Carries on from the hold.
    async fn continue_hold(&mut self, origin: Origin, client: &mut ReolinkClient, tx: &Events) {
        let Some(hold) = self.hold.take() else { return };
        self.hop(hold.next, origin, false, client, tx).await;
    }

    /// Opens what `next` says, at the wanted speed. A failed open of the
    /// user's pauses there with a notice; a failed automatic one skips to the
    /// following file, and pauses after `AUTO_SKIP_LIMIT` in a row.
    async fn hop(
        &mut self,
        mut next: Continue,
        origin: Origin,
        stall_retried: bool,
        client: &mut ReolinkClient,
        tx: &Events,
    ) {
        loop {
            let Some(files) = self.files.as_ref().map(|day| &day.files) else { return };
            let following = |index: usize| {
                next_file(files, index).map(|(index, at)| (index, ReplayResume::at(at), self.chain_or_reset(true, at)))
            };
            let target = match next {
                // A resume point at the file's end means the file is done.
                Continue::Resume { index, resume, fallback } => match files.get(index) {
                    Some(file) if resume.seek_time < file.end => {
                        Some((index, resume, self.chain_or_reset(true, fallback)))
                    }
                    _ => following(index),
                },
                Continue::Next { index } => following(index),
                Continue::Fresh { at, chain } => file_for_time(files, at)
                    .map(|(index, at)| (index, ReplayResume::at(at), self.chain_or_reset(chain, at))),
            };
            let Some((index, from, first)) = target else {
                match (origin, next) {
                    (Origin::User, Continue::Fresh { .. }) => {
                        self.nothing_after(tx).await;
                        self.pause_with(next);
                    }
                    _ => self.end_of_day(),
                }
                return;
            };
            match first {
                // What the sink holds plays on at once, while the device
                // opens what follows it.
                FirstFrame::Chain { .. } => {
                    if let Some((_, sink)) = &self.sink {
                        sink.set_paused(false);
                    }
                }
                // What the sink holds does not lead up to this opening.
                FirstFrame::Reset { fallback } if self.pushed_since_reset => self.flush_to(fallback),
                _ => {}
            }
            let retried = stall_retried && matches!(next, Continue::Resume { .. });
            match self.start(index, from, first, retried, client, tx).await {
                Ok(()) => return,
                Err(text) if origin == Origin::User => {
                    self.send_error(tx, text).await;
                    self.pause_with(next);
                    return;
                }
                Err(text) => {
                    self.auto_failures += 1;
                    if self.auto_failures >= AUTO_SKIP_LIMIT {
                        self.send_error(tx, format!("{text}; playback paused")).await;
                        let chain = self.pushed_since_reset;
                        self.pause_with(Continue::Fresh { at: from.seek_time, chain });
                        return;
                    }
                    self.send_error(tx, format!("{text}; skipped to the next recording")).await;
                    next = Continue::Next { index };
                }
            }
        }
    }

    /// Asks the client to open file `index`. On success the opening runs
    /// (phase `Opening` until its first frame); on failure, the text for
    /// the user.
    async fn start(
        &mut self,
        index: usize,
        from: ReplayResume,
        first: FirstFrame,
        stall_retried: bool,
        client: &mut ReolinkClient,
        tx: &Events,
    ) -> Result<(), String> {
        let Some(file) = self.files.as_ref().and_then(|day| day.files.get(index)).cloned() else {
            return Err("The recording is no longer listed".to_string());
        };
        let speed = self.want.speed;
        self.open = None;
        self.phase = PlayerPhase::Opening;
        self.opening_chained = matches!(first, FirstFrame::Chain { .. });
        self.at = Some(from.seek_time);
        self.file = Some(index);
        self.send_state(tx).await;
        if debug_replay() {
            eprintln!(
                "playback: opening {} at {} ({:?}, {}{})",
                file.name,
                civil_text(from.seek_time),
                speed,
                if matches!(first, FirstFrame::Chain { .. }) { "chained" } else { "fresh" },
                if from.after_microseconds.is_some() { ", resumed" } else { "" },
            );
        }
        let opened = if from.after_microseconds.is_some() {
            client.resume_replay(&file, from, speed).await
        } else {
            client.start_replay(&file, from.seek_time, speed).await
        };
        match opened {
            Ok(stream) => {
                if let Some(info) = stream.info {
                    self.picture = Some((info.width, info.height));
                }
                self.open = Some(Open {
                    session: self.want.session,
                    index,
                    stream,
                    speed,
                    kind: file.stream,
                    first,
                    stall_retried,
                    exhausted: false,
                });
                Ok(())
            }
            Err(e) => {
                if debug_replay() {
                    eprintln!("playback: opening {} failed: {e}", file.name);
                }
                Err(open_failure_text(&e.to_string(), file.encrypted))
            }
        }
    }

    /// Closes the running opening (waiting for the device's answer) and
    /// pushes the units still in its stream, so the sink plays on to where
    /// the device stopped.
    async fn close_and_drain(&mut self, client: &mut ReolinkClient) -> Option<Closed> {
        let mut open = self.open.take()?;
        let _ = client.stop_replay().await;
        if open.session == self.want.session {
            while let Some(Ok(unit)) = open.stream.units.next().await {
                self.push_unit(&mut open, unit).await;
            }
        }
        Some(Closed {
            index: open.index,
            speed: open.speed,
            kind: open.kind,
            chain_pending: matches!(open.first, FirstFrame::Chain { .. }),
            status: open.stream.progress.status(),
        })
    }

    fn hold_from(&mut self, closed: Closed, reason: HoldReason) {
        let next = continuation(&closed.status, closed.index, closed.chain_pending);
        self.file = Some(closed.index);
        self.hold = Some(Hold { next, speed: closed.speed, stream: closed.kind, reason });
    }

    /// Paused, to go on with `next` on Play.
    fn pause_with(&mut self, next: Continue) {
        self.open = None;
        self.hold =
            Some(Hold { next, speed: self.want.speed, stream: self.want.stream, reason: HoldReason::Paused });
        self.phase = PlayerPhase::Paused;
        self.want.playing = false;
        if let Some((_, sink)) = &self.sink {
            sink.set_paused(true);
        }
    }

    /// No file follows: the sink plays out what it holds.
    fn end_of_day(&mut self) {
        self.open = None;
        self.hold = None;
        if let Some((_, sink)) = &self.sink {
            sink.end_of_footage();
        }
        self.phase = PlayerPhase::Ended;
    }

    /// A seek after the last file: a notice, and the player stays as it was.
    async fn nothing_after(&mut self, tx: &Events) {
        let last_end = self.files.as_ref().and_then(|day| day.files.iter().map(|f| f.end).max());
        let message = match last_end {
            Some(end) => format!("Nothing recorded after {}", clock_text(end)),
            None => "No recordings on this day".to_string(),
        };
        self.send_error(tx, message).await;
        self.want.playing = self.open.is_some()
            || self.hold.is_some_and(|hold| hold.reason != HoldReason::Paused)
            || self.phase == PlayerPhase::Ended;
    }

    /// The calendar of a month. A running opening is held meanwhile (the
    /// sink plays on from what it holds) and carried on after.
    async fn month(&mut self, channel: u8, year: u16, month: u8, client: &mut ReolinkClient, tx: &Events) {
        let stale = self.open.as_ref().is_some_and(|open| open.session != self.want.session);
        let mut held = false;
        if stale {
            self.open = None;
            let _ = client.stop_replay_within(CLOSE_WAIT).await;
        } else if let Some(closed) = self.close_and_drain(client).await {
            self.hold_from(closed, HoldReason::Search);
            held = true;
            self.send_state(tx).await;
        }
        let days = client.search_month(channel, year, month).await.map_err(|e| e.to_string());
        let _ = tx.send(DeviceEvent::Playback(PlaybackEvent::Month { channel, year, month, days })).await;
        if held {
            if self.want.playing {
                self.continue_hold(Origin::Auto, client, tx).await;
            } else if let Some(hold) = self.hold.as_mut() {
                hold.reason = HoldReason::Paused;
                self.phase = PlayerPhase::Paused;
            }
        }
    }

    // ---- the sink ---------------------------------------------------------

    /// Pushes one unit of `open` into the sink: a video frame as `push`, an
    /// audio unit after the opening's first video frame has placed the
    /// opening on the sink's timeline (the sink itself plays audio at 1x
    /// only, and in time with the picture).
    async fn push_unit(&mut self, open: &mut Open, unit: ReplayUnit) {
        match unit {
            ReplayUnit::Video(frame) => self.push(open, frame).await,
            ReplayUnit::Audio(aac) => {
                if open.first == FirstFrame::Done {
                    if let Some((_, sink)) = &self.sink {
                        sink.push_replay_audio(&aac);
                    }
                }
            }
        }
    }

    /// Pushes one frame of `open` into the sink, asking for a sink first
    /// when there is none for its codec, and placing the opening's first
    /// frame on the timeline.
    async fn push(&mut self, open: &mut Open, frame: VideoFrame) {
        let mut new_sink = false;
        // A sink the page has torn down (a new session, another codec) is
        // no sink.
        if self.sink.as_ref().is_some_and(|(_, sink)| sink.is_retired()) {
            self.sink = None;
        }
        if self.sink.as_ref().map(|(video_type, _)| *video_type) != Some(frame.video_type) {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            if self.sinks.send((open.session, frame.video_type, reply_tx)).await.is_err() {
                return; // the window is gone
            }
            // No answer: the page has left this session (another day or
            // device, or the page itself); the frame is dropped.
            let Ok(sink) = reply_rx.await else { return };
            self.sink = Some((frame.video_type, sink));
            self.pushed_since_reset = false;
            new_sink = true;
        }
        let Some((_, sink)) = &self.sink else { return };
        if open.first != FirstFrame::Done || new_sink {
            let status = open.stream.progress.status();
            let (fallback, chain) = match open.first {
                FirstFrame::Reset { fallback } => (fallback, false),
                FirstFrame::Chain { fallback } => (fallback, true),
                FirstFrame::Done => {
                    (status.position_of(frame.microseconds).unwrap_or_else(|| status.position()), false)
                }
            };
            let at = first_frame_timeline(&status, frame.microseconds, fallback);
            // A new sink holds nothing to carry on from.
            if chain && self.pushed_since_reset && !new_sink {
                sink.chain_open(at);
            } else {
                sink.reset_for_open(at, open.speed.factor());
            }
            if open.first != FirstFrame::Done {
                open.first = FirstFrame::Done;
                self.phase = PlayerPhase::Playing;
                self.auto_failures = 0;
            }
        }
        sink.push_frame(&frame);
        self.pushed_since_reset = true;
    }

    /// Flushes the sink to `at` (nothing held is shown any more).
    fn flush_to(&mut self, at: CivilTime) {
        if let Some((_, sink)) = &self.sink {
            sink.reset_for_open(timeline_of(at), self.want.speed.factor());
        }
        self.pushed_since_reset = false;
    }

    /// `Chain` when the opening may carry on from what the sink holds
    /// (`chain`) and the sink holds something, else `Reset`.
    fn chain_or_reset(&self, chain: bool, fallback: CivilTime) -> FirstFrame {
        if chain && self.pushed_since_reset {
            FirstFrame::Chain { fallback }
        } else {
            FirstFrame::Reset { fallback }
        }
    }

    /// The sink plays exactly when the page wants it to: also after a
    /// `freeze()` of the page's that a later Play superseded.
    fn apply_paused_state(&self) {
        if let Some((_, sink)) = &self.sink {
            sink.set_paused(!self.want.playing);
        }
    }

    fn sink_status(&self) -> Option<ReplaySinkStatus> {
        self.sink.as_ref().and_then(|(_, sink)| sink.replay_status())
    }

    /// The position on screen, else the last target.
    fn screen_position(&self) -> Option<CivilTime> {
        self.sink_status()
            .map(|status| status.position)
            .filter(|position| !position.is_zero())
            .map(civil_of)
            .or(self.at)
    }

    fn first_start(&self) -> Option<CivilTime> {
        self.files.as_ref().and_then(|day| day.files.first()).map(|file| file.start)
    }

    fn file_name(&self, index: usize) -> String {
        self.files.as_ref().and_then(|day| day.files.get(index)).map_or_else(String::new, |f| f.name.clone())
    }

    // ---- reporting ----------------------------------------------------------

    fn tick_input<'a>(&self, status: Option<&'a ReplayStatus>) -> TickInput<'a> {
        TickInput {
            phase: self.phase,
            open: status,
            chained: self
                .open
                .as_ref()
                .map_or(self.opening_chained, |open| matches!(open.first, FirstFrame::Chain { .. })),
            stall_retried: self.open.as_ref().is_some_and(|open| open.stall_retried),
            hold: self.hold.map(|hold| hold.reason),
            sink: self.sink_status(),
        }
    }

    fn state(&self) -> PlayerState {
        let status = self.open.as_ref().map(|open| open.stream.progress.status());
        PlayerState {
            session: self.want.session,
            gen: self.applied_gen,
            phase: self.phase,
            buffering: buffering_of(&self.tick_input(status.as_ref())),
            file: self.file,
            speed: self.want.speed,
            stream: self.want.stream,
            at: self.at,
            picture: self.picture,
        }
    }

    /// Sends the player's state if it changed since the last one sent.
    async fn send_state(&mut self, tx: &Events) {
        if !self.is_active() {
            return;
        }
        let state = self.state();
        if self.last_sent.as_ref() != Some(&state) {
            self.last_sent = Some(state.clone());
            let _ = tx.send(DeviceEvent::Playback(PlaybackEvent::State(state))).await;
        }
    }

    async fn send_day(&mut self, tx: &Events) {
        let (Some(day), Some(loaded)) = (self.want.day, self.files.as_ref()) else { return };
        self.day_owed = false;
        let event = PlaybackEvent::Day {
            session: self.want.session,
            channel: loaded.channel,
            day,
            stream: loaded.stream,
            files: Ok(loaded.files.iter().map(FileSpan::from).collect()),
        };
        let _ = tx.send(DeviceEvent::Playback(event)).await;
    }

    /// Lists the loaded day's event windows and sends them as `Events`. A
    /// day without recordings has no windows to list, so nothing is asked.
    async fn send_events(&mut self, client: &mut ReolinkClient, tx: &Events) {
        let Some(loaded) = self.files.as_ref() else { return };
        let (channel, day, stream) = (loaded.channel, loaded.day, loaded.stream);
        let events = if loaded.files.is_empty() {
            Ok(Vec::new())
        } else {
            client.search_events(channel, day, stream).await.map_err(|e| e.to_string())
        };
        if debug_replay() {
            match &events {
                Ok(events) => eprintln!("playback: {} events on {} ({stream:?})", events.len(), civil_text(day)),
                Err(e) => eprintln!("playback: listing the events of {} failed: {e}", civil_text(day)),
            }
        }
        let event = PlaybackEvent::Events { session: self.want.session, channel, day, stream, events };
        let _ = tx.send(DeviceEvent::Playback(event)).await;
    }

    async fn send_error(&self, tx: &Events, message: String) {
        let event = PlaybackEvent::Error { session: self.want.session, message };
        let _ = tx.send(DeviceEvent::Playback(event)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reoling::{FootageAnchor, InfoBlock, RecordingId, ReplayKeyframe};

    fn t(h: u8, m: u8, s: u8) -> CivilTime {
        CivilTime::new(2026, 10, 19, h, m, s)
    }

    fn day() -> CivilTime {
        CivilTime::start_of_day(2026, 10, 19)
    }

    // ---- timeline helpers ----------------------------------------------------

    #[test]
    fn the_timeline_round_trips_device_local_times() {
        for time in [t(0, 0, 0), t(14, 3, 27), t(23, 59, 59), CivilTime::new(2024, 2, 29, 12, 0, 0)] {
            assert_eq!(civil_of(timeline_of(time)), time);
        }
        // Sub-second parts are floored.
        assert_eq!(civil_of(timeline_of(t(14, 3, 27)) + Duration::from_millis(999)), t(14, 3, 27));
        // The scale is zone-free arithmetic: one second is one second.
        assert_eq!(timeline_of(t(14, 3, 28)) - timeline_of(t(14, 3, 27)), Duration::from_secs(1));
    }

    #[test]
    fn seconds_of_the_day_cross_midnight_both_ways() {
        assert_eq!(seconds_of_day(timeline_of(t(0, 0, 0)), day()), 0.0);
        assert_eq!(seconds_of_day(timeline_of(t(14, 3, 27)), day()), 50_607.0);
        assert_eq!(seconds_of_day(timeline_of(t(14, 3, 27)) + Duration::from_micros(250_000), day()), 50_607.25);
        // A file that started the evening before, and one that runs past
        // midnight.
        assert_eq!(seconds_of_day(timeline_of(CivilTime::new(2026, 10, 18, 23, 59, 58)), day()), -2.0);
        assert_eq!(seconds_of_day(timeline_of(CivilTime::new(2026, 10, 20, 0, 0, 5)), day()), 86_405.0);
        // Any time of the day names the same day.
        assert_eq!(seconds_of_day(timeline_of(t(1, 0, 0)), t(17, 30, 0)), 3_600.0);
        // ...across the month's end too.
        let last = CivilTime::start_of_day(2026, 10, 31);
        assert_eq!(seconds_of_day(timeline_of(CivilTime::new(2026, 11, 1, 0, 0, 1)), last), 86_401.0);
    }

    #[test]
    fn a_point_on_the_day_is_a_whole_second_floored() {
        assert_eq!(civil_at(day(), 50_607.8), t(14, 3, 27));
        assert_eq!(civil_at(day(), 0.0), t(0, 0, 0));
        assert_eq!(civil_at(day(), 86_399.999), t(23, 59, 59));
        assert_eq!(civil_at(day(), -0.5), CivilTime::new(2026, 10, 18, 23, 59, 59));
        assert_eq!(civil_at(day(), 86_400.0), CivilTime::new(2026, 10, 20, 0, 0, 0));
        assert_eq!(civil_at(t(9, 0, 0), 60.0), t(0, 1, 0), "the day's time of day is ignored");
        assert_eq!(civil_at(day(), f64::NAN), t(0, 0, 0));
        // And back.
        assert_eq!(seconds_of_day(timeline_of(civil_at(day(), 50_607.8)), day()), 50_607.0);
    }

    // ---- file spans -----------------------------------------------------------

    #[test]
    fn a_file_span_keeps_times_and_encryption_only() {
        let file = RecordingFile {
            channel_id: 2,
            name: "0320261019140000".to_string(),
            id: RecordingId::new("1-2-0-01261019110000-00000-TESTCAMUID000001"),
            stream: RecordStream::Main,
            start: t(14, 0, 0),
            end: t(14, 30, 0),
            size_bytes: Some(1 << 20),
            support_sub: Some(1),
            encrypted: true,
            has_audio: true,
        };
        let span = FileSpan::from(&file);
        assert_eq!(span, FileSpan { start: t(14, 0, 0), end: t(14, 30, 0), encrypted: true });
        let shown = format!("{span:?}");
        assert!(!shown.contains("TESTCAMUID") && !shown.contains("0320261019"), "{shown}");
    }

    // ---- intents --------------------------------------------------------------

    fn day_command(gen: u64, session: u64, stream: RecordStream) -> PlaybackCommand {
        PlaybackCommand::Day { gen, session, channel: 2, day: t(17, 5, 0), stream }
    }

    #[test]
    fn every_command_records_its_intent() {
        let mut want = Want { speed: ReplaySpeed::X4, playing: true, ..Want::default() };
        apply_intent(&mut want, &day_command(1, 7, RecordStream::Main));
        assert_eq!(
            want,
            Want {
                session: 7,
                channel: 2,
                day: Some(day()),
                stream: RecordStream::Main,
                speed: ReplaySpeed::X4,
                playing: false,
                seek: None,
                play_pressed: false,
            },
            "a new session: its day at midnight, paused, the speed kept"
        );
        apply_intent(&mut want, &PlaybackCommand::Seek { gen: 2, at: t(14, 3, 27) });
        assert_eq!((want.seek, want.playing), (Some(t(14, 3, 27)), true));
        apply_intent(&mut want, &PlaybackCommand::Pause { gen: 3 });
        assert_eq!((want.seek, want.playing), (Some(t(14, 3, 27)), false), "a seek then a pause parks there");
        apply_intent(&mut want, &PlaybackCommand::Play { gen: 4 });
        assert!(want.playing && want.play_pressed);
        apply_intent(&mut want, &PlaybackCommand::Speed { gen: 5, speed: ReplaySpeed::Quarter });
        apply_intent(&mut want, &PlaybackCommand::Stream { gen: 6, stream: RecordStream::Sub });
        assert_eq!((want.speed, want.stream), (ReplaySpeed::Quarter, RecordStream::Sub));
        let before = want.clone();
        apply_intent(&mut want, &PlaybackCommand::Month { channel: 2, year: 2026, month: 9 });
        assert_eq!(want, before, "a calendar question changes nothing");
        apply_intent(&mut want, &PlaybackCommand::Close { gen: 7 });
        assert_eq!(want, Want::default());
    }

    #[test]
    fn only_the_newest_command_does_device_work() {
        // Five clicks while an open takes long: the commands carry gens 1..5,
        // the link is at 5 by the time the first is read.
        let latest = 5;
        let mut want = Want::default();
        apply_intent(&mut want, &day_command(1, 1, RecordStream::Sub));
        let mut ran = Vec::new();
        for (gen, minute) in (2..=5).zip([10u8, 20, 30, 40]) {
            let command = PlaybackCommand::Seek { gen, at: t(14, minute, 0) };
            apply_intent(&mut want, &command);
            if !is_superseded(command.gen().unwrap(), latest) {
                ran.push(gen);
            }
        }
        assert_eq!(ran, vec![5]);
        assert_eq!(want.seek, Some(t(14, 40, 0)), "the one open goes where the last click was");
        assert!(want.playing);
        // The calendar has no generation: never superseded.
        assert_eq!(PlaybackCommand::Month { channel: 0, year: 2026, month: 10 }.gen(), None);
        assert_eq!(PlaybackCommand::Close { gen: 3 }.gen(), Some(3));
        assert!(!is_superseded(5, 5) && is_superseded(4, 5));
    }

    #[test]
    fn only_a_stream_change_carries_the_position_over() {
        let loaded = (2, day(), RecordStream::Sub);
        assert!(carries_position(loaded, (2, day(), RecordStream::Main)));
        assert!(!carries_position(loaded, (2, day(), RecordStream::Sub)));
        assert!(!carries_position(loaded, (3, day(), RecordStream::Main)), "another channel starts afresh");
        let next_day = CivilTime::start_of_day(2026, 10, 20);
        assert!(!carries_position(loaded, (2, next_day, RecordStream::Main)), "another day starts afresh");
    }

    // ---- plan -----------------------------------------------------------------

    fn wants(playing: bool) -> WantSummary {
        WantSummary {
            playing,
            seek: None,
            speed: ReplaySpeed::X1,
            stream: RecordStream::Sub,
            has_files: true,
            play_pressed: false,
        }
    }

    const OPEN: ActualSummary = ActualSummary::Open { speed: ReplaySpeed::X1, stream: RecordStream::Sub };
    const HELD: ActualSummary = ActualSummary::Held { speed: ReplaySpeed::X1, stream: RecordStream::Sub, backlog: false };
    const BACKLOG: ActualSummary =
        ActualSummary::Held { speed: ReplaySpeed::Quarter, stream: RecordStream::Sub, backlog: true };
    const IDLE: ActualSummary = ActualSummary::Idle { ended: false };
    const ENDED: ActualSummary = ActualSummary::Idle { ended: true };

    #[test]
    fn a_seek_opens_there_or_parks_there() {
        let screen = t(9, 0, 0);
        let seek = WantSummary { seek: Some(t(14, 3, 27)), ..wants(true) };
        for actual in [OPEN, HELD, IDLE, ENDED, BACKLOG] {
            assert_eq!(plan(&seek, &actual, screen), Plan::OpenFresh(t(14, 3, 27)), "{actual:?}");
        }
        let seek_then_pause = WantSummary { playing: false, ..seek };
        for actual in [OPEN, HELD, IDLE] {
            assert_eq!(plan(&seek_then_pause, &actual, screen), Plan::ParkAt(t(14, 3, 27)), "{actual:?}");
        }
    }

    #[test]
    fn pause_holds_an_open_and_otherwise_does_nothing() {
        let screen = t(9, 0, 0);
        assert_eq!(plan(&wants(false), &OPEN, screen), Plan::PauseHold);
        for actual in [HELD, IDLE, ENDED, BACKLOG] {
            assert_eq!(plan(&wants(false), &actual, screen), Plan::Nothing, "{actual:?}");
        }
    }

    #[test]
    fn playing_goes_on_or_reopens_at_the_screen_on_a_change() {
        let screen = t(9, 30, 12);
        assert_eq!(plan(&wants(true), &OPEN, screen), Plan::Nothing);
        let faster = WantSummary { speed: ReplaySpeed::X8, ..wants(true) };
        assert_eq!(plan(&faster, &OPEN, screen), Plan::OpenFresh(screen));
        let clear = WantSummary { stream: RecordStream::Main, ..wants(true) };
        assert_eq!(plan(&clear, &OPEN, screen), Plan::OpenFresh(screen));
        // From a hold: carry on, unless the speed changed meanwhile.
        assert_eq!(plan(&wants(true), &HELD, screen), Plan::ContinueHold);
        assert_eq!(plan(&faster, &HELD, screen), Plan::OpenFresh(screen), "a speed change while held");
        // A backlog hold is the tick's to resume; a speed change reopens.
        let quarter = WantSummary { speed: ReplaySpeed::Quarter, ..wants(true) };
        assert_eq!(plan(&quarter, &BACKLOG, screen), Plan::Nothing);
        assert_eq!(plan(&wants(true), &BACKLOG, screen), Plan::OpenFresh(screen));
    }

    #[test]
    fn play_with_nothing_starts_the_day_and_after_its_end_only_on_play() {
        let screen = t(23, 0, 0);
        assert_eq!(plan(&wants(true), &IDLE, screen), Plan::FirstFile);
        let pressed = WantSummary { play_pressed: true, ..wants(true) };
        assert_eq!(plan(&pressed, &ENDED, screen), Plan::FirstFile);
        // A speed change at the end does not start the day over.
        let faster = WantSummary { speed: ReplaySpeed::X2, ..wants(true) };
        assert_eq!(plan(&faster, &ENDED, screen), Plan::Nothing);
    }

    #[test]
    fn without_files_nothing_plays() {
        let none = |playing| WantSummary { has_files: false, ..wants(playing) };
        assert_eq!(plan(&none(true), &IDLE, t(0, 0, 0)), Plan::NoRecordings);
        assert_eq!(plan(&WantSummary { seek: Some(t(1, 0, 0)), ..none(false) }, &IDLE, t(0, 0, 0)), Plan::NoRecordings);
        assert_eq!(plan(&none(false), &IDLE, t(0, 0, 0)), Plan::Nothing);
    }

    // ---- tick -----------------------------------------------------------------

    /// A playback opened at 09:21:07 of a file 09:21:07..10:21:05, estimated
    /// position (no info block).
    fn status(footage_s: u64, frames: u64, silent_for: Duration) -> ReplayStatus {
        ReplayStatus {
            seq: 1_792_348_477,
            speed: ReplaySpeed::X1,
            seek_time: t(9, 21, 7),
            skip_through_microseconds: None,
            file_start: t(9, 21, 7),
            file_end: t(10, 21, 5),
            info: None,
            first_microseconds: (frames > 0).then_some(4_000_000_000),
            last_microseconds: (frames > 0).then_some(4_000_000_000u32.wrapping_add((footage_s * 1_000_000) as u32)),
            footage_micros: footage_s * 1_000_000,
            frames,
            keyframes: 0,
            last_keyframe: None,
            anchor: None,
            data_messages: frames,
            bytes: 0,
            foreign_messages: 0,
            silent_for,
            ended: false,
        }
    }

    fn sink(buffered: Duration, backlog: bool) -> ReplaySinkStatus {
        ReplaySinkStatus { buffered, slow_motion_backlog: backlog, ..ReplaySinkStatus::default() }
    }

    fn input(phase: PlayerPhase, open: Option<&ReplayStatus>, sink_status: Option<ReplaySinkStatus>) -> TickInput<'_> {
        TickInput { phase, open, chained: false, stall_retried: false, hold: None, sink: sink_status }
    }

    const LEAD: Duration = Duration::from_secs(5);

    #[test]
    fn the_end_of_a_file_goes_on_to_the_next() {
        let ended = status(3_596, 400, Duration::from_secs(5));
        assert_eq!(decide(&input(PlayerPhase::Playing, Some(&ended), Some(sink(LEAD, false)))).0, TickAction::FileEnded);
        // A file the device does not send at all.
        let nothing = status(0, 0, Duration::from_secs(6));
        assert_eq!(decide(&input(PlayerPhase::Opening, Some(&nothing), None)).0, TickAction::FileEnded);
        // Mid-file, the usual pause between GOPs: nothing.
        let mid = status(600, 400, Duration::from_secs(4));
        assert_eq!(decide(&input(PlayerPhase::Playing, Some(&mid), Some(sink(LEAD, false)))), (TickAction::Nothing, false));
    }

    #[test]
    fn a_stall_is_waited_out_then_reopened_once_then_skipped() {
        let stalled = status(600, 400, Duration::from_secs(8));
        let (action, buffering) = decide(&input(PlayerPhase::Playing, Some(&stalled), Some(sink(LEAD, false))));
        assert_eq!((action, buffering), (TickAction::Nothing, true), "buffering while the path may come back");
        let long = status(600, 400, STALL_REOPEN_AFTER);
        assert_eq!(decide(&input(PlayerPhase::Playing, Some(&long), Some(sink(LEAD, false)))).0, TickAction::ReopenStalled);
        let again = TickInput { stall_retried: true, ..input(PlayerPhase::Playing, Some(&long), Some(sink(LEAD, false))) };
        assert_eq!(decide(&again).0, TickAction::FileEnded, "a reopen that stalls again: the file is shorter than listed");
    }

    #[test]
    fn slow_motion_holds_the_device_and_resumes_once_drained() {
        let running = status(600, 400, Duration::ZERO);
        let full = sink(Duration::from_secs(60), true);
        assert_eq!(decide(&input(PlayerPhase::Playing, Some(&running), Some(full))), (TickAction::HoldForBacklog, false));
        let held = |buffered| TickInput { hold: Some(HoldReason::Backlog), ..input(PlayerPhase::Playing, None, Some(sink(buffered, false))) };
        assert_eq!(decide(&held(Duration::from_secs(20))), (TickAction::Nothing, false));
        assert_eq!(decide(&held(BACKLOG_RESUME_BELOW)).0, TickAction::ResumeHold);
        assert_eq!(decide(&held(Duration::from_secs(1))).0, TickAction::ResumeHold);
        // A pause or a search hold is not resumed by the tick.
        for reason in [HoldReason::Paused, HoldReason::Search] {
            let other = TickInput { hold: Some(reason), ..held(Duration::from_secs(1)) };
            assert_eq!(decide(&other).0, TickAction::Nothing);
        }
    }

    #[test]
    fn buffering_shows_while_nothing_can_be_shown() {
        let running = status(600, 400, Duration::ZERO);
        let dry = Some(sink(Duration::from_millis(100), false));
        let fed = Some(sink(LEAD, false));
        // Opening a fresh position: nothing to show until the first frame.
        assert!(decide(&input(PlayerPhase::Opening, None, fed)).1);
        // Opening the next file while the sink still plays the last one.
        let chained = |s| TickInput { chained: true, ..input(PlayerPhase::Opening, Some(&running), s) };
        assert!(!decide(&chained(fed)).1);
        assert!(decide(&chained(dry)).1);
        // Playing: the lead ran dry while more is to come.
        assert!(decide(&input(PlayerPhase::Playing, Some(&running), dry)).1);
        assert!(!decide(&input(PlayerPhase::Playing, Some(&running), fed)).1);
        // ...but not at the file's end, where the sink simply plays out.
        let at_end = status(3_596, 400, Duration::ZERO);
        assert!(!decide(&input(PlayerPhase::Playing, Some(&at_end), dry)).1);
        // A calendar search holds the device.
        let search = TickInput { hold: Some(HoldReason::Search), ..input(PlayerPhase::Playing, None, fed) };
        assert!(decide(&search).1);
        // Paused, ended and idle never buffer.
        for phase in [PlayerPhase::Paused, PlayerPhase::Ended, PlayerPhase::Idle, PlayerPhase::Failed] {
            assert!(!decide(&input(phase, None, dry)).1, "{phase:?}");
        }
    }

    // ---- holds ----------------------------------------------------------------

    #[test]
    fn a_hold_goes_on_where_the_opening_stopped() {
        // Nothing shown: open at the seek time again; chained if the
        // opening was the next file.
        let none = status(0, 0, Duration::ZERO);
        assert_eq!(continuation(&none, 3, false), Continue::Fresh { at: t(9, 21, 7), chain: false });
        assert_eq!(continuation(&none, 3, true), Continue::Fresh { at: t(9, 21, 7), chain: true });
        // Delivered to the file's end: the next file, never this one again.
        assert_eq!(continuation(&status(3_596, 400, Duration::ZERO), 3, false), Continue::Next { index: 3 });
        // Mid-file: resume at the resume point.
        let mid = status(600, 400, Duration::ZERO);
        assert_eq!(
            continuation(&mid, 3, false),
            Continue::Resume { index: 3, resume: mid.resume_point(), fallback: t(9, 31, 7) }
        );
        // An opening that so far only repeated an earlier one's footage
        // resumes where that one would have.
        let repeats = ReplayStatus { skip_through_microseconds: Some(123_456), ..status(0, 0, Duration::ZERO) };
        let Continue::Resume { resume, .. } = continuation(&repeats, 3, true) else { panic!("a resume") };
        assert_eq!(resume, ReplayResume { seek_time: t(9, 21, 7), after_microseconds: Some(123_456) });
    }

    // ---- first frame ----------------------------------------------------------

    /// A file listed 23:59:50 (Oct 18) .. 00:00:40 device-local, UTC+3: the
    /// info block says 20:59:50..21:00:40 UTC.
    fn anchored_across_midnight() -> ReplayStatus {
        let local_start = CivilTime::new(2026, 10, 18, 23, 59, 50);
        let utc_start = CivilTime::new(2026, 10, 18, 20, 59, 50);
        let key_us = 1_968_952_017u32;
        // The key frame 23:59:58 local carries 20:59:58 UTC; the anchor keeps
        // a finer estimate of :58.25.
        let key_utc = CivilTime::new(2026, 10, 18, 20, 59, 58).to_epoch_seconds();
        ReplayStatus {
            seek_time: local_start.add_seconds(7),
            file_start: local_start,
            file_end: local_start.add_seconds(50),
            info: Some(InfoBlock { width: 896, height: 512, start_utc: Some(utc_start), end_utc: Some(utc_start.add_seconds(50)) }),
            first_microseconds: Some(key_us),
            last_microseconds: Some(key_us),
            last_keyframe: Some(ReplayKeyframe { microseconds: key_us, unix_seconds: Some(key_utc as u32) }),
            anchor: Some(FootageAnchor { microseconds: key_us, unix_micros: key_utc * 1_000_000 + 250_000 }),
            frames: 1,
            ..status(0, 0, Duration::ZERO)
        }
    }

    #[test]
    fn the_first_frame_is_placed_at_its_real_time() {
        let s = anchored_across_midnight();
        let key_us = s.anchor.unwrap().microseconds;
        let fallback = s.seek_time;
        // Device-local 23:59:58.25 (UTC+3), not the seek time 23:59:57 and
        // not the host's zone.
        let at = first_frame_timeline(&s, key_us, fallback);
        assert_eq!(civil_of(at), CivilTime::new(2026, 10, 18, 23, 59, 58));
        assert_eq!(at.subsec_micros(), 250_000);
        // On the selected day (the 19th) that is just before its midnight.
        assert_eq!(seconds_of_day(at, day()), -1.75);
        // 2.5 s later, past midnight.
        let later = first_frame_timeline(&s, key_us + 2_500_000, fallback);
        assert_eq!(seconds_of_day(later, day()), 0.75);
        // Without key-frame times (or without the info block): the time it
        // was opened at.
        let plain = ReplayStatus { anchor: None, ..s.clone() };
        assert_eq!(first_frame_timeline(&plain, key_us, fallback), timeline_of(fallback));
        let no_zone = ReplayStatus { info: None, ..s };
        assert_eq!(first_frame_timeline(&no_zone, key_us, fallback), timeline_of(fallback));
    }

    // ---- notices --------------------------------------------------------------

    #[test]
    fn a_refusal_names_its_code() {
        let refused = "protocol error: the device refused the playback (code 431)";
        assert_eq!(refusal_code(refused), Some(431));
        assert_eq!(refusal_code("timed out waiting for a reply"), None);
        assert_eq!(refusal_code("(code x)"), None);
        assert_eq!(open_failure_text(refused, false), "The device refused to play this recording (code 431)");
        assert_eq!(open_failure_text(refused, true), "This recording is encrypted (code 431)");
        assert_eq!(
            open_failure_text("timed out waiting for a reply", false),
            "Could not play this recording: timed out waiting for a reply"
        );
        assert_eq!(clock_text(t(23, 15, 0)), "23:15:00");
    }
}
