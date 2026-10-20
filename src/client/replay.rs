//! Recorded video: the recording calendar, the file list of a day, and
//! playing one file from a given time.
//!
//! On the wire a playback is a seek (`MSG_ID_REPLAY_SEEK`, which also sets a
//! sequence number) followed by an open of the file by its `Id`
//! (`MSG_ID_REPLAY`). The open has no XML reply: the device answers with a
//! 32-byte info block (class 0x0000, code 200) and then the recording as
//! BcMedia, in `MSG_ID_REPLAY` messages whose class and response code
//! together carry the sequence number. Nothing marks the end of a file; the
//! data simply stop. A close (`MSG_ID_REPLAY_CLOSE`) names the file by its
//! `name`. The calendar, search, seek, open and close requests carry no
//! Extension.
//!
//! The device starts a playback at a key frame from about 1.1 s before the
//! seek time to about one GOP after it (observed). Every key frame of a
//! recording carries its time (UTC, whole seconds) at the start of its extra
//! header, and footage times belong to the file: the same frame has the same
//! footage time in every opening of the file.

use super::*;
use crate::protocol::bcmedia::model::{keyframe_unix_time, parse_info_block, InfoBlock};
use crate::protocol::time::CivilTime;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio_stream::StreamExt;

/// How long a calendar, search, seek, open or close request waits for its
/// answer. Over a relay path replies have been seen to arrive several
/// seconds late (up to about 8 s), so this is well above that.
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);

/// How long each message of an event-window search waits for its answer.
/// The list is a decoration of the day's timeline, so a device that never
/// answers (none seen so far) must not hold the day up for the full
/// `REPLY_TIMEOUT` every time, but the wait stays above the late replies
/// seen over a relay; after one unanswered start the client asks no more
/// (see [`ReolinkClient::search_events`]).
const EVENT_SEARCH_REPLY_WAIT: Duration = Duration::from_secs(10);

/// How long starting live video waits for the device's answer to closing a
/// playback that still runs. The answer only confirms the close; live
/// video's own reader drops whatever of the playback still trails in (and a
/// late answer), so there is no reason to hold up the picture for long.
pub(super) const LIVE_SWITCH_CLOSE_WAIT: Duration = Duration::from_secs(3);

/// The playback reader holds the connection lock only this long per read,
/// so that the end-of-file silence never keeps the lock held for long.
const READ_SLICE: Duration = Duration::from_secs(1);

/// A file search returns at most `FILE_SEARCH_PAGE_SIZE` files per page and
/// is asked again while pages are full. This bounds the number of pages one
/// day can take (a day of event recordings has been seen with 156 files).
const MAX_SEARCH_PAGES: usize = 64;

/// An event search returns 50 or 60 windows per page (NVR, Home Hub) and
/// is asked again until a page says it is the last. This bounds the number
/// of pages one day can take (a day has been seen with 380 windows).
const MAX_EVENT_PAGES: usize = 200;

/// How close to the file's end the footage received must come to count as
/// "delivered to the end". At 16x the device sends I-frames only, so the
/// last one can lie up to one GOP (about 4.2 s on a sub stream) before the
/// end; at the other speeds every frame arrives and only the rounding of
/// the listed end time is left.
const END_POSITION_TOLERANCE_IFRAMES_ONLY_SECS: i64 = 5;
const END_POSITION_TOLERANCE_ALL_FRAMES_SECS: i64 = 2;

/// Added to the end tolerance when the position is only estimated (no key
/// frame carried its time) and the playback was opened inside the file: the
/// device starts at a key frame from about 1.1 s before the seek time to
/// about one GOP after it (observed), so the estimate (seek time plus
/// footage) can lag by up to one GOP; up to 4.7 s was measured, whole-second
/// rounding included.
const END_SEEK_LAG_TOLERANCE_SECS: i64 = 5;

/// A key frame's own time (from its header) is believed only within this
/// far outside the file's UTC span as the info block gives it.
const KEYFRAME_TIME_SLACK_SECS: i64 = 60;

/// The running footage-to-time estimate may lie this far past a key frame's
/// whole-second time (which truncates) before it is set back to it: beyond
/// that the camera's footage clock has run fast or jumped.
const ANCHOR_AHEAD_LIMIT_MICROS: i64 = 1_500_000;

/// Time zone offsets are whole quarter hours; the device's offset from UTC
/// is rounded to this, so that a second or two between the listed start and
/// the info block's does not matter.
const ZONE_OFFSET_STEP_SECS: i64 = 900;

/// The largest offset from UTC a time zone has (14 h), plus one step.
const MAX_ZONE_OFFSET_SECS: i64 = 14 * 3_600 + ZONE_OFFSET_STEP_SECS;

/// How far (either way) from its anchor a footage time is still placed by
/// it: well inside the half wrap of the footage clock that a signed
/// difference can tell apart.
const MAX_ANCHOR_REACH_MICROS: u32 = 1_800_000_000;

/// Somewhat more than one GOP of footage: at 1x the device sends one GOP
/// roughly every 4 s, so a pause of that length between bursts is normal.
const END_GOP_FOOTAGE_SECS: f64 = 4.5;

/// No data for this long while playing: when nothing at all has arrived
/// since the open, the device is not going to send this file (the end);
/// after some footage, far from the file's end, the path has stalled.
const END_SILENCE: Duration = Duration::from_secs(6);

/// Footage-time steps between consecutive frames above this are not
/// counted as footage (a camera clock jump, not elapsed recording).
const MAX_FOOTAGE_STEP_US: u32 = 30_000_000;

/// Where the sequence numbers start when the host clock is not a plausible
/// Unix time (they must lie in `REPLAY_SEQ_RANGE` for the data to be framed).
const FALLBACK_SEQ_BASE: u32 = 1_800_000_000;

/// How many playback data messages the probe describes per open.
const PROBE_LOGGED_MESSAGES: usize = 20;

/// How many key-frame headers the probe describes per open.
const PROBE_LOGGED_KEYFRAMES: usize = 3;

/// Playback speed. The device itself plays at 1, 2, 4, 8 or 16 times real
/// time; at 16x it sends I-frames only and no audio. The slower speeds are
/// opened at 1x and slowed down by the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ReplaySpeed {
    Quarter,
    Half,
    #[default]
    X1,
    X2,
    X4,
    X8,
    X16,
}

impl ReplaySpeed {
    pub const ALL: [ReplaySpeed; 7] = [
        ReplaySpeed::Quarter,
        ReplaySpeed::Half,
        ReplaySpeed::X1,
        ReplaySpeed::X2,
        ReplaySpeed::X4,
        ReplaySpeed::X8,
        ReplaySpeed::X16,
    ];

    /// Footage seconds per wall-clock second as the viewer sees it.
    pub fn factor(self) -> f64 {
        match self {
            ReplaySpeed::Quarter => 0.25,
            ReplaySpeed::Half => 0.5,
            ReplaySpeed::X1 => 1.0,
            ReplaySpeed::X2 => 2.0,
            ReplaySpeed::X4 => 4.0,
            ReplaySpeed::X8 => 8.0,
            ReplaySpeed::X16 => 16.0,
        }
    }

    /// The open request's `<playSpeed>`, or `None` to leave it out (the
    /// slower-than-real-time speeds, which the device then sends at 1x).
    pub fn play_speed_tag(self) -> Option<u32> {
        match self {
            ReplaySpeed::Quarter | ReplaySpeed::Half => None,
            ReplaySpeed::X1 => Some(1),
            ReplaySpeed::X2 => Some(2),
            ReplaySpeed::X4 => Some(4),
            ReplaySpeed::X8 => Some(8),
            ReplaySpeed::X16 => Some(16),
        }
    }

    /// The speed at which the device delivers footage for this setting.
    pub fn device_factor(self) -> f64 {
        f64::from(self.play_speed_tag().unwrap_or(1))
    }

    /// Whether the playback's audio is passed on: only at real time, the
    /// only speed at which it is played (the device sends it up to 8x).
    pub fn plays_audio(self) -> bool {
        self == ReplaySpeed::X1
    }
}

/// One recorded file as the day search lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingFile {
    /// The channel the search was made for (and that the open and close
    /// requests name).
    pub channel_id: u8,
    /// What a close names the file by: the channel number plus one as two
    /// digits, then the device-local start as `YYYYMMDDhhmmss`.
    pub name: String,
    /// What an open names the file by. Pass it back unchanged; never log
    /// it as text (on a Home Hub it ends with the camera's UID).
    pub id: RecordingId,
    pub stream: RecordStream,
    /// Device-local start and end. A file can overlap the next one by a few
    /// seconds; treat it as the half-open span `start..end`.
    pub start: CivilTime,
    pub end: CivilTime,
    /// `sizeH:sizeL` as listed, in bytes. The device rounds it up and it
    /// can exceed what a playback or download delivers: an upper estimate.
    pub size_bytes: Option<u64>,
    /// `supportSub` as listed.
    pub support_sub: Option<u8>,
    pub encrypted: bool,
    pub has_audio: bool,
}

impl RecordingFile {
    /// Whether `time` lies in the half-open span `start..end`.
    pub fn contains(&self, time: CivilTime) -> bool {
        self.start <= time && time < self.end
    }

    /// Length in seconds.
    pub fn duration_seconds(&self) -> i64 {
        self.end.to_epoch_seconds() - self.start.to_epoch_seconds()
    }

    /// A search-page entry as a playable file of `channel_id`; `searched`
    /// is the stream the search asked for, used when the entry names none.
    fn from_entry(channel_id: u8, searched: RecordStream, entry: &FileInfo) -> Option<Self> {
        let listed = entry.recording()?;
        Some(RecordingFile {
            channel_id,
            name: listed.name,
            id: listed.id,
            stream: listed.stream.unwrap_or(searched),
            start: listed.start,
            end: listed.end,
            size_bytes: listed.size_bytes,
            support_sub: entry.support_sub,
            encrypted: listed.encrypted,
            has_audio: listed.has_audio,
        })
    }
}

/// One event window of a day as the event search lists it: the span of a
/// detection inside a recorded file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedEvent {
    /// Device-local start and end (`start < end`).
    pub start: CivilTime,
    pub end: CivilTime,
    /// What was detected, as the device names it (`md`, `people`,
    /// `vehicle`, `dog_cat`, ...), each once, in the order listed. A window
    /// the device lists under several kinds is one event with all of them.
    pub kinds: Vec<String>,
}

/// One listed window: a span and one kind.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ListedEvent {
    start: CivilTime,
    end: CivilTime,
    kind: String,
}

/// The windows of one event-search page. An entry without a start, an end
/// or a kind, or whose end lies before its start, is dropped; a window of
/// no length is given one second, so that it still shows. A kind list
/// (`md, people`) gives one window per kind.
fn events_of_page(page: &AlarmVideoInfo) -> Vec<ListedEvent> {
    page.entries()
        .iter()
        .filter_map(|entry| {
            let (start, mut end) = (entry.start_time?, entry.end_time?);
            if end < start {
                return None;
            }
            if end == start {
                end = start.add_seconds(1);
            }
            let kinds: Vec<String> = entry
                .alarm_type
                .as_deref()?
                .split(',')
                .map(str::trim)
                .filter(|kind| !kind.is_empty())
                .map(str::to_string)
                .collect();
            Some(kinds.into_iter().map(move |kind| ListedEvent { start, end, kind }))
        })
        .flatten()
        .collect()
}

/// Sorts windows by start (then end) and folds those with the same span
/// into one event that carries each of their kinds once.
fn merge_events(mut listed: Vec<ListedEvent>) -> Vec<RecordedEvent> {
    // A stable sort: the kinds of one window keep the order listed.
    listed.sort_by_key(|event| (event.start, event.end));
    let mut events: Vec<RecordedEvent> = Vec::new();
    for ListedEvent { start, end, kind } in listed {
        match events.last_mut() {
            Some(last) if last.start == start && last.end == end => {
                if !last.kinds.contains(&kind) {
                    last.kinds.push(kind);
                }
            }
            _ => events.push(RecordedEvent { start, end, kinds: vec![kind] }),
        }
    }
    events
}

/// The pages of one event search, as they come in.
#[derive(Debug, Default)]
struct EventPages {
    listed: Vec<ListedEvent>,
    /// Pages taken in.
    pages: usize,
    /// Entries listed on them, before merging.
    entries: usize,
}

impl EventPages {
    /// Takes one page in. Whether another is to be asked for: not after the
    /// page that says it is the last, nor after one that lists nothing (the
    /// search would make no progress), nor past `MAX_EVENT_PAGES`.
    fn take(&mut self, page: &AlarmVideoInfo) -> bool {
        self.pages += 1;
        let entries = page.entries().len();
        self.entries += entries;
        self.listed.extend(events_of_page(page));
        !page.is_finished() && entries > 0 && self.pages < MAX_EVENT_PAGES
    }
}

/// Sorts files by start and drops every entry whose name repeats an earlier
/// one (pages can list a file twice), keeping the first.
pub fn sort_and_dedup_files(files: &mut Vec<RecordingFile>) {
    files.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.name.cmp(&b.name)));
    let mut seen = HashSet::new();
    files.retain(|f| seen.insert(f.name.clone()));
}

/// Where to open for time `time` in a sorted file list: the index of the
/// file and the seek time. When `time` lies in two overlapping files the
/// later one is used: opening a file in its last second or two yields one
/// I-frame and then nothing, while its successor covers the same moment
/// from its own start. A `time` in a gap (or before the first file) opens
/// the next file at its start; after the last file there is nothing.
pub fn file_for_time(files: &[RecordingFile], time: CivilTime) -> Option<(usize, CivilTime)> {
    if let Some(index) = files.iter().rposition(|f| f.contains(time)) {
        return Some((index, time));
    }
    files.iter().position(|f| f.start > time).map(|index| (index, files[index].start))
}

/// What to play once file `current` of a sorted list has ended: the next
/// entry of the list, opened at `max(its start, the ended file's end)` so
/// that an overlap is not played twice. Never the ended file itself, nor
/// any entry that would be opened at or after its own end (one that lies
/// inside the ended file's span); those are skipped.
pub fn next_file(files: &[RecordingFile], current: usize) -> Option<(usize, CivilTime)> {
    let ended = files.get(current)?;
    files.iter().enumerate().skip(current + 1).find_map(|(index, file)| {
        let at = file.start.max(ended.end);
        (at < file.end).then_some((index, at))
    })
}

/// The sequence number for the next playback on a connection: the Unix
/// time plus the number of playbacks opened before, never equal to (and
/// never below) the previous one, and always one whose data the codec frames
/// as playback data (`is_usable_replay_seq`).
pub(super) fn replay_seq_for(now_unix: u64, opened_before: u32, previous: Option<u32>) -> u32 {
    let base = u32::try_from(now_unix).ok().filter(|t| REPLAY_SEQ_RANGE.contains(t)).unwrap_or(FALLBACK_SEQ_BASE);
    let mut seq = base.saturating_add(opened_before);
    if let Some(previous) = previous {
        seq = seq.max(previous.saturating_add(1));
    }
    loop {
        if is_usable_replay_seq(seq) && Some(seq) != previous {
            return seq;
        }
        seq = if REPLAY_SEQ_RANGE.contains(&seq) {
            // The high half is one of the fixed header classes: skip it.
            ((seq >> 16) + 1) << 16
        } else {
            *REPLAY_SEQ_RANGE.start()
        };
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The playback currently open on a client.
pub(crate) struct ActiveReplay {
    file: RecordingFile,
    progress: ReplayProgress,
}

/// A started playback: its frames and its progress.
///
/// Dropping `units` stops the reader but does not close the playback on
/// the device, which goes on sending the rest of the file: end it with
/// [`ReolinkClient::stop_replay`] (a new playback, live video or a search
/// on this client closes it too).
pub struct ReplayStream {
    /// Video frames (exactly as live video delivers them) and, at 1x, the
    /// audio units between them, in footage order: each audio unit comes
    /// after the video frame it follows in the recording. Audio never goes
    /// to the client's audio channel (`take_audio`), which is live only.
    /// An `Err` ends the stream (the connection failed); the end of the
    /// file does not: the device just stops sending, see
    /// [`ReplayStatus::at_end`] (and [`ReplayStatus::stalled`] for a pause
    /// short of the end).
    pub units: ReceiverStream<crate::Result<ReplayUnit>>,
    /// The info block that opened the playback (`None` if the device sent
    /// data without one): picture size and the file's span in UTC.
    pub info: Option<InfoBlock>,
    /// The sequence number this playback's data carry.
    pub seq: u32,
    pub progress: ReplayProgress,
}

/// One unit of a playback's data (see [`ReplayStream::units`]).
#[derive(Debug, Clone, PartialEq)]
pub enum ReplayUnit {
    Video(VideoFrame),
    /// One AAC (ADTS) audio unit, after the video frame it follows. The
    /// device sends about 16 a second (64 ms each) at 1x to 8x and none at
    /// 16x; only at 1x are they handed on (see [`ReplaySpeed::plays_audio`]).
    Audio(Vec<u8>),
}

/// A playback's progress, shared with the task that reads its data. Cheap
/// to clone; [`ReplayProgress::status`] takes a snapshot.
#[derive(Clone)]
pub struct ReplayProgress(Arc<std::sync::Mutex<ProgressState>>);

struct ProgressState {
    seq: u32,
    speed: ReplaySpeed,
    seek_time: CivilTime,
    /// The resume threshold this playback was opened with.
    skip_through_us: Option<u32>,
    file_start: CivilTime,
    file_end: CivilTime,
    info: Option<InfoBlock>,
    first_us: Option<u32>,
    last_us: Option<u32>,
    footage_us: u64,
    frames: u64,
    keyframes: u64,
    last_keyframe: Option<ReplayKeyframe>,
    anchor: Option<FootageAnchor>,
    data_messages: u64,
    bytes: u64,
    foreign_messages: u64,
    /// When data last arrived, or when the reader last had to wait for the
    /// frame consumer (time spent waiting for it is not device silence).
    last_activity: Instant,
    consumer_blocked: bool,
    ended: bool,
}

/// The latest key frame of a playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayKeyframe {
    /// Its footage time.
    pub microseconds: u32,
    /// Its own time from its header (Unix time, UTC, whole seconds), when
    /// it carries one that lies in the file's span.
    pub unix_seconds: Option<u32>,
}

/// Ties footage time to the device's clock: the footage time of a key frame
/// and its time in microseconds since the Unix epoch (UTC). Key frame times
/// are whole seconds (truncated); the anchor keeps the running estimate as
/// long as it agrees with them, so that it is finer than one second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FootageAnchor {
    pub microseconds: u32,
    pub unix_micros: i64,
}

impl FootageAnchor {
    /// Microseconds since the Unix epoch (UTC) of footage time
    /// `microseconds`, which may lie before or after the anchor (by up to
    /// about 35 minutes, half the footage clock's wrap).
    pub fn unix_micros_of(&self, microseconds: u32) -> i64 {
        self.unix_micros + i64::from(microseconds.wrapping_sub(self.microseconds) as i32)
    }
}

/// Where to open a file again to carry on from an earlier opening of it
/// without a gap: see [`ReplayStatus::resume_point`] and
/// [`ReolinkClient::resume_replay`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayResume {
    /// Where to seek (device-local).
    pub seek_time: CivilTime,
    /// Footage time of the last frame the earlier opening handed on, if
    /// any. The new opening drops the video frames at or before it (and the
    /// audio until the first video frame after it): they are repeats.
    pub after_microseconds: Option<u32>,
}

impl ReplayResume {
    /// A plain opening at `seek_time`, dropping nothing.
    pub fn at(seek_time: CivilTime) -> Self {
        ReplayResume { seek_time, after_microseconds: None }
    }
}

/// A snapshot of a playback's progress.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayStatus {
    pub seq: u32,
    pub speed: ReplaySpeed,
    /// Where the playback was opened (device-local). The device starts at a
    /// key frame from about 1.1 s before it to about one GOP after it
    /// (observed).
    pub seek_time: CivilTime,
    /// The `after_microseconds` of the [`ReplayResume`] it was opened with.
    pub skip_through_microseconds: Option<u32>,
    /// The file's span as the day search listed it (device-local).
    pub file_start: CivilTime,
    pub file_end: CivilTime,
    pub info: Option<InfoBlock>,
    /// Footage time (camera clock, wraps every 2^32 µs) of the first and the
    /// latest video frame handed on.
    pub first_microseconds: Option<u32>,
    pub last_microseconds: Option<u32>,
    /// Footage handed on, from the first video frame to the latest.
    pub footage_micros: u64,
    pub frames: u64,
    pub keyframes: u64,
    /// The latest key frame received (handed on, or dropped as a repeat).
    pub last_keyframe: Option<ReplayKeyframe>,
    /// Set by the key frames that carry their time.
    pub anchor: Option<FootageAnchor>,
    pub data_messages: u64,
    /// Payload bytes of the data messages.
    pub bytes: u64,
    /// Data messages of other (closed) playbacks dropped since the seek was
    /// sent. A relay path has been seen to deliver more than 30 of them
    /// after the close was answered.
    pub foreign_messages: u64,
    /// How long no data have arrived while the reader was waiting for some.
    pub silent_for: Duration,
    /// The reader has stopped (connection failed, or the frames were
    /// dropped).
    pub ended: bool,
}

impl ReplayStatus {
    /// The device's offset from UTC in seconds (device-local = UTC +
    /// offset): the file's listed start against its start in the info
    /// block, to the quarter hour. Never the host's zone. `None` without an
    /// info block that gives the start, or for an offset no zone has. A
    /// file that spans a daylight-saving change is an hour off after it.
    pub fn zone_offset_seconds(&self) -> Option<i64> {
        let start_utc = self.info?.start_utc?;
        let raw = self.file_start.to_epoch_seconds() - start_utc.to_epoch_seconds();
        let offset = (raw + ZONE_OFFSET_STEP_SECS / 2).div_euclid(ZONE_OFFSET_STEP_SECS) * ZONE_OFFSET_STEP_SECS;
        (offset.abs() <= MAX_ZONE_OFFSET_SECS).then_some(offset)
    }

    /// Whether positions come from the key frames' own times rather than
    /// from the seek time plus the footage.
    pub fn is_anchored(&self) -> bool {
        self.anchor.is_some() && self.zone_offset_seconds().is_some()
    }

    /// Device-local time of footage time `microseconds`, in microseconds on
    /// the [`CivilTime::to_epoch_seconds`] scale, from the key frames' own
    /// times (sub-second part kept). `None` when not anchored (see
    /// [`ReplayStatus::is_anchored`]) or when `microseconds` lies out of the
    /// anchor's reach.
    pub fn local_micros_of(&self, microseconds: u32) -> Option<i64> {
        let anchor = self.anchor?;
        let offset = self.zone_offset_seconds()?;
        if (microseconds.wrapping_sub(anchor.microseconds) as i32).unsigned_abs() > MAX_ANCHOR_REACH_MICROS {
            return None;
        }
        Some(anchor.unix_micros_of(microseconds) + offset * 1_000_000)
    }

    /// Device-local time of footage time `microseconds` from the anchor.
    fn anchored(&self, microseconds: u32) -> Option<CivilTime> {
        let local_micros = self.local_micros_of(microseconds)?;
        Some(CivilTime::from_epoch_seconds(local_micros.div_euclid(1_000_000)))
    }

    /// The seek time plus the footage handed on. The device starts at a key
    /// frame from about 1.1 s before the seek time to about one GOP after it
    /// (observed), so after a seek inside the file this lags by up to one GOP
    /// or leads by up to about 1.1 s.
    fn estimated_position(&self) -> CivilTime {
        self.seek_time.add_seconds((self.footage_micros / 1_000_000) as i64)
    }

    /// Footage position of the latest frame handed on (device-local, whole
    /// seconds). From the latest key frame's own time when the key frames
    /// carry it (see [`ReplayStatus::is_anchored`]); otherwise the seek time
    /// plus the footage, which after a seek inside the file lags by up to
    /// one GOP. The seek time before any frame.
    pub fn position(&self) -> CivilTime {
        self.last_microseconds.and_then(|us| self.anchored(us)).unwrap_or_else(|| self.estimated_position())
    }

    /// The same for a frame with footage time `microseconds` (for a cursor
    /// that follows what is being shown, which lags behind what has been
    /// received). `None` before the first frame.
    pub fn position_of(&self, microseconds: u32) -> Option<CivilTime> {
        let first = self.first_microseconds?;
        self.anchored(microseconds)
            .or_else(|| Some(self.seek_time.add_seconds(i64::from(microseconds.wrapping_sub(first) / 1_000_000))))
    }

    /// Where to open this file again to carry on without a gap, after a
    /// stall, a pause or when slow motion has piled up too much footage:
    /// the resume seeks to the latest key frame's own time, and the new
    /// opening drops what it repeats (see [`ReplayResume`]); hardware
    /// probes resumed this way without a gap and without a repeat. Without
    /// key-frame times, at an estimate of that key frame's time that lies
    /// at or before it, so that still nothing is skipped. Take it once
    /// every frame of the stream has been taken (after
    /// [`ReolinkClient::stop_replay`] the stream still yields what it
    /// holds, then ends). Use it only when the opening has not
    /// [`ReplayStatus::delivered_to_end`]; otherwise go on with
    /// [`next_file`].
    pub fn resume_point(&self) -> ReplayResume {
        let estimate = |key: ReplayKeyframe| {
            // The seek time plus the footage since the first frame handed
            // on, less a second: that frame can lie up to about 1.1 s
            // before the seek time. A key frame before it (a repeat) counts
            // as at it.
            let since_first = key.microseconds.wrapping_sub(self.first_microseconds?) as i32;
            Some(self.seek_time.add_seconds(i64::from(since_first.max(0)) / 1_000_000 - 1))
        };
        let seek_time = match self.last_keyframe {
            Some(key) => key
                .unix_seconds
                .zip(self.zone_offset_seconds())
                .map(|(unix, offset)| CivilTime::from_epoch_seconds(i64::from(unix) + offset))
                .or_else(|| estimate(key))
                .unwrap_or(self.seek_time),
            None => self.seek_time,
        };
        ReplayResume { seek_time, after_microseconds: self.last_microseconds.or(self.skip_through_microseconds) }
    }

    /// Whether the footage received reaches the file's end (within one GOP
    /// at 16x, where only I-frames are sent; within 2 s otherwise; one GOP
    /// more when the position is only estimated after a seek inside the
    /// file).
    pub fn delivered_to_end(&self) -> bool {
        let mut tolerance = if self.speed.device_factor() >= 16.0 {
            END_POSITION_TOLERANCE_IFRAMES_ONLY_SECS
        } else {
            END_POSITION_TOLERANCE_ALL_FRAMES_SECS
        };
        if !self.is_anchored() && self.seek_time > self.file_start {
            tolerance += END_SEEK_LAG_TOLERANCE_SECS;
        }
        self.frames > 0 && self.position().add_seconds(tolerance) >= self.file_end
    }

    /// Whether the device has finished sending this file: the footage
    /// reached its end and nothing has followed for a little more than one
    /// GOP at the delivery speed, or no video at all has arrived since the
    /// open for `END_SILENCE`. The caller then closes the playback and
    /// opens [`next_file`] — never the same file again at its own end.
    /// Silence after some footage, far from the file's end, is not the end
    /// but [`ReplayStatus::stalled`].
    pub fn at_end(&self) -> bool {
        let settle = Duration::from_secs_f64((END_GOP_FOOTAGE_SECS / self.speed.device_factor()).max(1.0));
        (self.delivered_to_end() && self.silent_for >= settle) || (self.frames == 0 && self.silent_for >= END_SILENCE)
    }

    /// Whether the playback has stopped short of the file's end: footage
    /// came, then nothing for `END_SILENCE`, and what came does not reach
    /// the end. Usually a stalled path (a relay can pause for tens of
    /// seconds and then go on): keep the playback open and show
    /// "buffering". How long to wait is the caller's choice; to give up,
    /// close it and open the same file again at
    /// [`ReplayStatus::resume_point`] (a new open, so a new sequence
    /// number). Should that open stall short of the end too, the file holds
    /// less than its listed span: treat it as ended and go on with
    /// [`next_file`].
    pub fn stalled(&self) -> bool {
        self.frames > 0 && !self.delivered_to_end() && self.silent_for >= END_SILENCE
    }
}

impl ReplayProgress {
    fn new(file: &RecordingFile, from: ReplayResume, speed: ReplaySpeed, seq: u32, info: Option<InfoBlock>) -> Self {
        ReplayProgress(Arc::new(std::sync::Mutex::new(ProgressState {
            seq,
            speed,
            seek_time: from.seek_time,
            skip_through_us: from.after_microseconds,
            file_start: file.start,
            file_end: file.end,
            info,
            first_us: None,
            last_us: None,
            footage_us: 0,
            frames: 0,
            keyframes: 0,
            last_keyframe: None,
            anchor: None,
            data_messages: 0,
            bytes: 0,
            foreign_messages: 0,
            last_activity: Instant::now(),
            consumer_blocked: false,
            ended: false,
        })))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ProgressState> {
        self.0.lock().expect("replay progress mutex poisoned")
    }

    pub fn status(&self) -> ReplayStatus {
        let s = self.state();
        ReplayStatus {
            seq: s.seq,
            speed: s.speed,
            seek_time: s.seek_time,
            skip_through_microseconds: s.skip_through_us,
            file_start: s.file_start,
            file_end: s.file_end,
            info: s.info,
            first_microseconds: s.first_us,
            last_microseconds: s.last_us,
            footage_micros: s.footage_us,
            frames: s.frames,
            keyframes: s.keyframes,
            last_keyframe: s.last_keyframe,
            anchor: s.anchor,
            data_messages: s.data_messages,
            bytes: s.bytes,
            foreign_messages: s.foreign_messages,
            silent_for: if s.consumer_blocked { Duration::ZERO } else { s.last_activity.elapsed() },
            ended: s.ended,
        }
    }

    fn on_data(&self, payload_bytes: usize) {
        let mut s = self.state();
        s.data_messages += 1;
        s.bytes += payload_bytes as u64;
        s.last_activity = Instant::now();
    }

    fn on_foreign(&self, messages: u64) {
        self.state().foreign_messages += messages;
    }

    /// A key frame of this playback arrived (handed on or not), with the
    /// time its header carries, if any. A time outside the file's span (as
    /// the info block gives it, give or take `KEYFRAME_TIME_SLACK_SECS`) is
    /// not believed, nor any without the info block's span. The anchor
    /// moves to every key frame with a time: it keeps the running estimate
    /// while that lies in `time .. time + ANCHOR_AHEAD_LIMIT_MICROS`, and
    /// takes the key frame's time otherwise.
    fn on_keyframe(&self, microseconds: u32, unix_seconds: Option<u32>) {
        let mut s = self.state();
        let span = s.info.and_then(|i| i.start_utc.zip(i.end_utc));
        let unix_seconds = unix_seconds.filter(|&t| {
            span.is_some_and(|(start, end)| {
                let t = i64::from(t);
                t >= start.to_epoch_seconds() - KEYFRAME_TIME_SLACK_SECS
                    && t <= end.to_epoch_seconds() + KEYFRAME_TIME_SLACK_SECS
            })
        });
        s.last_keyframe = Some(ReplayKeyframe { microseconds, unix_seconds });
        if let Some(t) = unix_seconds {
            let floor = i64::from(t) * 1_000_000;
            let unix_micros = s
                .anchor
                .map(|a| a.unix_micros_of(microseconds))
                .filter(|&running| running >= floor && running - floor < ANCHOR_AHEAD_LIMIT_MICROS)
                .unwrap_or(floor);
            s.anchor = Some(FootageAnchor { microseconds, unix_micros });
        }
    }

    /// A video frame was handed on.
    fn on_frame(&self, microseconds: u32, keyframe: bool) {
        let mut s = self.state();
        s.frames += 1;
        s.keyframes += u64::from(keyframe);
        match s.last_us {
            None => s.first_us = Some(microseconds),
            Some(last) => {
                let step = microseconds.wrapping_sub(last);
                if step <= MAX_FOOTAGE_STEP_US {
                    s.footage_us += u64::from(step);
                }
            }
        }
        s.last_us = Some(microseconds);
    }

    fn set_consumer_blocked(&self, blocked: bool) {
        let mut s = self.state();
        s.consumer_blocked = blocked;
        s.last_activity = Instant::now();
    }

    fn mark_ended(&self) {
        self.state().ended = true;
    }
}

/// What a successful open returns: the sequence number its data carry, the
/// info block, any data of it read before the info block, and how many data
/// messages of other playbacks were dropped meanwhile.
struct OpenedReplay {
    seq: u32,
    open_msg_num: u16,
    info: Option<InfoBlock>,
    early: Vec<Bc>,
    foreign: u64,
}

/// Turns one playback's data messages into BcMedia units (the synchronous
/// part of the playback task, the same steps as live video).
struct ReplayReader {
    seq: u32,
    open_msg_num: u16,
    /// Video frames at or before this footage time (and audio until the
    /// first video frame after it) repeat an earlier opening: dropped.
    skip_through: Option<u32>,
    /// A video frame has been handed on. Audio before the first one is
    /// dropped: an opening in the middle of a file can begin with a burst
    /// of up to a few hundred audio units (24 s) ahead of its first video
    /// frame, while its last seconds still carry their own audio, so that
    /// burst is footage from before the picture starts.
    seen_video: bool,
    /// The probe's device label, when the probe runs this playback.
    probe: Option<String>,
    buffer: Vec<u8>,
    guard: MediaGuard,
    msg_num_mismatches: u64,
    probe_logged: usize,
    probe_keyframes: usize,
}

impl ReplayReader {
    fn new(seq: u32, open_msg_num: u16, skip_through: Option<u32>, probe: Option<String>) -> Self {
        ReplayReader {
            seq,
            open_msg_num,
            skip_through,
            seen_video: false,
            probe,
            buffer: Vec::new(),
            guard: MediaGuard::default(),
            msg_num_mismatches: 0,
            probe_logged: 0,
            probe_keyframes: 0,
        }
    }

    /// Whether `bc` is data of this playback. Playback data are recognised
    /// by their sequence number alone; data of an earlier playback can still
    /// trail in and are dropped. A `msg_num` other than the open request's
    /// is reported once and accepted.
    fn is_ours(&mut self, bc: &Bc) -> bool {
        if bc.meta.msg_id != MSG_ID_REPLAY {
            return false;
        }
        let ours = bc.meta.replay_seq() == Some(self.seq);
        if let Some(tag) = self.probe.as_deref().filter(|_| self.probe_logged < PROBE_LOGGED_MESSAGES) {
            self.probe_logged += 1;
            eprintln!(
                "PROBE replay [{tag}] data #{}: {} msg_num sent={} echoed={} hdr12-13=({},{}) seq={:?} ours={} class={:#06x} code={:#06x} payload={}",
                self.probe_logged,
                if ours { "accepted" } else { "dropped" },
                self.open_msg_num,
                bc.meta.msg_num,
                bc.meta.channel_id,
                bc.meta.stream_type,
                bc.meta.replay_seq(),
                self.seq,
                bc.meta.class,
                bc.meta.response_code,
                payload_len(bc),
            );
        }
        if ours && bc.meta.msg_num != self.open_msg_num {
            self.msg_num_mismatches += 1;
            if self.msg_num_mismatches == 1 && debug_replay() {
                eprintln!(
                    "replay: data carry msg_num {} while the open request had {}; accepted (the sequence number identifies them)",
                    bc.meta.msg_num, self.open_msg_num
                );
            }
        }
        ours
    }

    /// The BcMedia units completed by `bc`, each key frame with the time
    /// its header carries (see `keyframe_unix_time`).
    fn feed(&mut self, bc: Bc) -> Vec<(BcMediaMessage, Option<u32>)> {
        let BcBody::Modern(ModernMsg { extension_xml, payload: Some(payload) }) = bc.body else {
            return Vec::new();
        };
        let extension = extension_xml.as_deref().and_then(|xml| Extension::from_bytes(xml).ok());
        match self.guard.judge(extension.as_ref(), &payload) {
            Verdict::Accept => {}
            Verdict::Skip => return Vec::new(),
            Verdict::Damaged => {
                self.buffer.clear();
                return Vec::new();
            }
        }
        self.buffer.extend_from_slice(&payload);
        let mut units = Vec::new();
        loop {
            match parse_one(&self.buffer) {
                Ok(Some((unit, used))) => {
                    let keyframe_time = match &unit {
                        BcMediaMessage::Video(frame) if frame.is_keyframe => {
                            self.probe_keyframe_header();
                            keyframe_unix_time(&self.buffer[..used])
                        }
                        _ => None,
                    };
                    self.buffer.drain(..used);
                    units.push((unit, keyframe_time));
                }
                Ok(None) => break,
                Err(e) => {
                    let skip = crate::protocol::bcmedia::model::resync_offset(&self.buffer);
                    if debug_replay() {
                        eprintln!("replay: media resync: {e}; skipped {skip} bytes");
                    }
                    if skip == 0 {
                        break;
                    }
                    self.buffer.drain(..skip);
                }
            }
        }
        units
    }

    /// Whether `unit` repeats footage that an earlier opening of the file
    /// already handed on (see [`ReplayResume`]). The first video frame past
    /// the threshold ends the dropping. Footage times are compared as a
    /// signed difference, so across the clock's wrap too.
    fn is_repeat(&mut self, unit: &BcMediaMessage) -> bool {
        let repeat = match self.skip_through {
            None => false,
            Some(after) => match unit {
                BcMediaMessage::Video(frame) if frame.microseconds.wrapping_sub(after) as i32 > 0 => {
                    self.skip_through = None;
                    false
                }
                _ => true,
            },
        };
        if repeat {
            return true;
        }
        match unit {
            BcMediaMessage::Video(_) => {
                self.seen_video = true;
                false
            }
            // Audio before the opening's first video frame (see `seen_video`).
            BcMediaMessage::Audio(_) => !self.seen_video,
            _ => false,
        }
    }

    /// Probe only: describes the header of the key frame at the start of
    /// the buffer (its footage time and the extra header bytes after the
    /// fixed part, read as a little-endian Unix time when there are four or
    /// more), for the first few key frames of the playback.
    fn probe_keyframe_header(&mut self) {
        let Some(tag) = self.probe.as_deref().filter(|_| self.probe_keyframes < PROBE_LOGGED_KEYFRAMES) else {
            return;
        };
        let b = &self.buffer;
        if b.len() < 24 {
            return;
        }
        self.probe_keyframes += 1;
        let microseconds = u32::from_le_bytes([b[16], b[17], b[18], b[19]]);
        let extra_len = u32::from_le_bytes([b[12], b[13], b[14], b[15]]) as usize;
        let extra = &b[24..b.len().min(24 + extra_len)];
        let hex = extra.iter().take(16).map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ");
        let time = keyframe_unix_time(b)
            .map(|t| CivilTime::from_epoch_seconds(i64::from(t)))
            .map_or("-".to_string(), |t| format!("{} UTC", civil(&t)));
        eprintln!(
            "PROBE replay [{tag}] key frame #{}: footage time {microseconds} us, extra header {extra_len} B [{hex}], first 4 as Unix time: {time}",
            self.probe_keyframes
        );
    }
}

fn payload_len(bc: &Bc) -> usize {
    match &bc.body {
        BcBody::Modern(ModernMsg { payload: Some(p), .. }) => p.len(),
        _ => 0,
    }
}

fn payload_of(bc: &Bc) -> Option<&[u8]> {
    match &bc.body {
        BcBody::Modern(ModernMsg { payload: Some(p), .. }) => Some(p),
        _ => None,
    }
}

/// The XML of a reply (empty when it has no payload).
fn reply_xml(bc: &Bc) -> crate::Result<BcXml> {
    match payload_of(bc) {
        Some(payload) => BcXml::from_bytes(payload),
        None => Ok(BcXml::default()),
    }
}

fn expect_ok(bc: &Bc, what: &str) -> crate::Result<()> {
    if bc.meta.response_code == 200 {
        Ok(())
    } else {
        Err(Error::ProtocolError(format!("the device refused the {what} (code {})", bc.meta.response_code)))
    }
}

fn debug_replay() -> bool {
    std::env::var("REOLING_DEBUG_REPLAY").is_ok()
        || std::env::var("REOLING_PROBE_REPLAY").is_ok()
        || std::env::var("REOLING_PROBE_EVENTS").is_ok()
}

/// The channel `REOLING_PROBE_EVENTS` asks the event-search probe to run on.
pub(super) fn probe_events_channel() -> Option<u8> {
    let value = std::env::var("REOLING_PROBE_EVENTS").ok()?;
    match value.trim().parse() {
        Ok(channel_id) => Some(channel_id),
        Err(_) => {
            eprintln!("PROBE events: REOLING_PROBE_EVENTS must be a channel number, got {value:?}");
            None
        }
    }
}

/// The channel `REOLING_PROBE_REPLAY` asks the playback probe to run on.
pub(super) fn probe_replay_channel() -> Option<u8> {
    let value = std::env::var("REOLING_PROBE_REPLAY").ok()?;
    match value.trim().parse() {
        Ok(channel_id) => Some(channel_id),
        Err(_) => {
            eprintln!("PROBE replay: REOLING_PROBE_REPLAY must be a channel number, got {value:?}");
            None
        }
    }
}

/// `YYYY-MM-DD hh:mm:ss`.
fn civil(t: &CivilTime) -> String {
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second)
}

fn describe_info(info: &InfoBlock) -> String {
    let span = |t: &Option<CivilTime>| t.as_ref().map_or("-".to_string(), civil);
    format!("{}x{} file {} .. {} UTC", info.width, info.height, span(&info.start_utc), span(&info.end_utc))
}

/// Months to look through for the probe: the current one, then the one
/// before.
fn current_and_previous_month(now: CivilTime) -> [(u16, u8); 2] {
    let previous = if now.month <= 1 { (now.year.saturating_sub(1), 12) } else { (now.year, now.month - 1) };
    [(now.year, now.month), previous]
}

/// The shortest file the probe prefers: long enough for a seek inside it
/// and a resume well before its end.
const PROBE_MIN_FILE_SECS: i64 = 60;

/// Which file of a day the probe opens: not the newest, which may still be
/// recording, when there is another; of the others the latest that runs
/// for at least `PROBE_MIN_FILE_SECS`, else the second newest.
fn probe_pick(files: &[RecordingFile]) -> Option<&RecordingFile> {
    match files.len() {
        0 => None,
        1 => files.first(),
        n => files[..n - 1]
            .iter()
            .rev()
            .find(|f| f.duration_seconds() >= PROBE_MIN_FILE_SECS)
            .or_else(|| files.get(n - 2)),
    }
}

impl ReolinkClient {
    /// A request with its body only and no Extension (the calendar, search,
    /// seek, open and close messages). Returns its `msg_num`.
    async fn send_request(&mut self, msg_id: u32, channel_id: u8, payload: Vec<u8>) -> crate::Result<u16> {
        let msg_num = self.next_msg_num();
        let request = Bc {
            meta: BcMeta { msg_id, channel_id, stream_type: 0, msg_num, response_code: 0, class: 0x6414 },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: Some(payload) }),
        };
        self.connection.lock().await.send_bc(&request, &self.encryption).await?;
        Ok(msg_num)
    }

    /// Passes a message that is not the one being waited for to the push
    /// handling (channel lists, alarms and the like keep arriving); anything
    /// else, such as data trailing in from a closed playback, is dropped.
    fn forward_push(&self, bc: &Bc, image_chunks: &mut HashMap<u16, Vec<u8>>) {
        if let Some(update) = pushed_update(bc, image_chunks, &self.pending_images, &self.channel_uids) {
            let _ = self.channel_updates_tx.send(update);
        }
    }

    /// Reads the connection until the answer to request `msg_id` arrives,
    /// for up to `REPLY_TIMEOUT`. The answer is recognised by its message
    /// id; a `msg_num` other than the request's is accepted (and reported
    /// under the replay debug switches).
    async fn await_reply(&mut self, msg_id: u32, msg_num: u16) -> crate::Result<Bc> {
        self.await_reply_within(msg_id, msg_num, REPLY_TIMEOUT).await
    }

    /// `await_reply`, waiting up to `wait`.
    async fn await_reply_within(&mut self, msg_id: u32, msg_num: u16, wait_for: Duration) -> crate::Result<Bc> {
        self.await_reply_counting(msg_id, msg_num, wait_for).await.map(|(bc, _)| bc)
    }

    /// `await_reply_within`, also counting the data messages of playbacks
    /// (any sequence number) that arrived meanwhile and were dropped.
    async fn await_reply_counting(
        &mut self,
        msg_id: u32,
        msg_num: u16,
        wait_for: Duration,
    ) -> crate::Result<(Bc, u64)> {
        let connection = Arc::clone(&self.connection);
        let encryption = self.encryption.clone();
        let mut image_chunks = HashMap::new();
        let mut foreign = 0u64;
        let reading = async {
            loop {
                let received = connection.lock().await.recv_bc(&encryption).await;
                let bc = match received {
                    Ok(bc) => bc,
                    // Nothing at all for a while: keep waiting until our
                    // own deadline.
                    Err(Error::ReplyTimeout) => continue,
                    Err(e) => return Err(e),
                };
                if bc.meta.msg_id == msg_id {
                    if bc.meta.msg_num != msg_num && debug_replay() {
                        eprintln!(
                            "replay: reply to {msg_id} carries msg_num {} (request had {msg_num}); accepted",
                            bc.meta.msg_num
                        );
                    }
                    return Ok((bc, foreign));
                }
                if bc.meta.msg_id == MSG_ID_REPLAY && bc.meta.replay_seq().is_some() {
                    foreign += 1;
                }
                self.forward_push(&bc, &mut image_chunks);
            }
        };
        tokio::time::timeout(wait_for, reading).await.map_err(|_| Error::ReplyTimeout)?
    }

    async fn request(&mut self, msg_id: u32, channel_id: u8, payload: Vec<u8>) -> crate::Result<Bc> {
        let msg_num = self.send_request(msg_id, channel_id, payload).await?;
        self.await_reply(msg_id, msg_num).await
    }

    /// Requests that read their own answers need the connection to
    /// themselves: a running stream's reader would take the answer.
    fn ensure_no_stream(&self, what: &str) -> crate::Result<()> {
        if self.video_task.as_ref().is_some_and(|t| !t.is_finished()) {
            return Err(Error::ProtocolError(format!("live video is running on this device; stop it before {what}")));
        }
        if self.replay_task.as_ref().is_some_and(|t| !t.is_finished()) {
            return Err(Error::ProtocolError(format!("a recording is playing on this device; stop it before {what}")));
        }
        Ok(())
    }

    /// A playback whose reader has stopped (its frames were dropped, or the
    /// connection failed) can still be open on the device, which then goes
    /// on sending: close it before a request that reads its own answer.
    async fn close_finished_replay(&mut self) {
        if self.replay.is_some() && self.replay_task.as_ref().map_or(true, |t| t.is_finished()) {
            let _ = self.stop_replay().await;
        }
    }

    /// The days of `month` (1–12) of `year` on which channel `channel_id`
    /// has recordings, as day numbers in ascending order. Fails while a
    /// live stream or a playback runs on this client (a playback whose
    /// frames were dropped is closed first).
    pub async fn search_month(&mut self, channel_id: u8, year: u16, month: u8) -> crate::Result<Vec<u8>> {
        self.close_finished_replay().await;
        self.ensure_no_stream("searching recordings")?;
        if !(1..=12).contains(&month) {
            return Err(Error::ProtocolError(format!("no such month: {month}")));
        }
        let request = DayRecords::month(channel_id, self.channel_uid(channel_id), year, month);
        let reply = self.request(MSG_ID_RECORD_CALENDAR, channel_id, request.to_bytes()).await?;
        expect_ok(&reply, "recording calendar request")?;
        Ok(reply_xml(&reply)?.day_records.map(|c| month_days(&c, channel_id, year, month)).unwrap_or_default())
    }

    /// The files channel `channel_id` recorded on the device-local day of
    /// `day` (its time of day is ignored) for `stream`, sorted by start,
    /// each name once. Main and sub stream recordings are separate files,
    /// so a stream switch needs a new search. Fails while a live stream or
    /// a playback runs on this client (a playback whose frames were dropped
    /// is closed first).
    pub async fn search_day(
        &mut self,
        channel_id: u8,
        day: CivilTime,
        stream: RecordStream,
    ) -> crate::Result<Vec<RecordingFile>> {
        self.close_finished_replay().await;
        self.ensure_no_stream("searching recordings")?;
        if !CivilTime::start_of_day(day.year, day.month, day.day).is_valid() {
            return Err(Error::ProtocolError("not a valid day".to_string()));
        }
        let uid = self.channel_uid(channel_id);
        let start = FileSearchStart::whole_day(channel_id, uid.clone(), stream, day.year, day.month, day.day);
        let reply = self.request(MSG_ID_FILE_SEARCH_START, channel_id, start.to_bytes()).await?;
        expect_ok(&reply, "file search")?;
        let handle = reply_xml(&reply)?
            .file_info_list
            .and_then(|list| list.handle())
            .ok_or_else(|| Error::ProtocolError("the file search reply has no search handle".to_string()))?;
        let page_request = FileSearchPage::new(channel_id, uid, handle).to_bytes();
        let pages = self.search_pages(channel_id, stream, &page_request).await;
        // Release the search whatever happened to the pages.
        let ended = self.request(MSG_ID_FILE_SEARCH_END, channel_id, page_request).await;
        if let Err(e) = ended {
            if debug_replay() {
                eprintln!("replay: ending the file search failed: {e}");
            }
        }
        let mut files = pages?;
        sort_and_dedup_files(&mut files);
        Ok(files)
    }

    /// Asks for pages until one is not full (or adds nothing new).
    async fn search_pages(
        &mut self,
        channel_id: u8,
        stream: RecordStream,
        page_request: &[u8],
    ) -> crate::Result<Vec<RecordingFile>> {
        let mut files = Vec::new();
        let mut names = HashSet::new();
        for page in 0..MAX_SEARCH_PAGES {
            let reply = self.request(MSG_ID_FILE_SEARCH_NEXT, channel_id, page_request.to_vec()).await?;
            if reply.meta.response_code != 200 {
                if page == 0 {
                    expect_ok(&reply, "file search page request")?;
                }
                // A refusal after a full page: no more pages.
                break;
            }
            let list = reply_xml(&reply)?.file_info_list.unwrap_or_default();
            let mut added = 0;
            for file in list.files.iter().filter_map(|entry| RecordingFile::from_entry(channel_id, stream, entry)) {
                if names.insert(file.name.clone()) {
                    files.push(file);
                    added += 1;
                }
            }
            if !list.is_full_page() || added == 0 {
                break;
            }
        }
        Ok(files)
    }

    /// A request with its body and an Extension naming the channel (the
    /// event search messages). Returns its `msg_num`.
    async fn send_channel_request(&mut self, msg_id: u32, channel_id: u8, payload: Vec<u8>) -> crate::Result<u16> {
        let msg_num = self.next_msg_num();
        let extension = Extension { version: XML_VERSION.to_string(), channel_id: Some(channel_id), ..Default::default() };
        let request = Bc {
            meta: BcMeta { msg_id, channel_id, stream_type: 0, msg_num, response_code: 0, class: 0x6414 },
            body: BcBody::Modern(ModernMsg { extension_xml: Some(extension.to_bytes()), payload: Some(payload) }),
        };
        self.connection.lock().await.send_bc(&request, &self.encryption).await?;
        Ok(msg_num)
    }

    /// `send_channel_request`, then its answer, waiting up to `wait_for`.
    async fn channel_request(
        &mut self,
        msg_id: u32,
        channel_id: u8,
        payload: Vec<u8>,
        wait_for: Duration,
    ) -> crate::Result<Bc> {
        let msg_num = self.send_channel_request(msg_id, channel_id, payload).await?;
        self.await_reply_within(msg_id, msg_num, wait_for).await
    }

    /// The event windows (motion, person, vehicle, animal and the like)
    /// channel `channel_id` detected on the device-local day of `day` (its
    /// time of day is ignored) for `stream`, sorted by start; a window
    /// listed under several kinds is one event with all of them. Every kind
    /// the device knows is asked for; filtering is the caller's. Fails while
    /// a live stream or a playback runs on this client (a playback whose
    /// frames were dropped is closed first). Each message waits
    /// `EVENT_SEARCH_REPLY_WAIT` for its answer; once a device has left the
    /// search's start unanswered, it is taken to have no event listing and
    /// every later search on this connection lists nothing without asking.
    pub async fn search_events(
        &mut self,
        channel_id: u8,
        day: CivilTime,
        stream: RecordStream,
    ) -> crate::Result<Vec<RecordedEvent>> {
        self.search_events_counting(channel_id, day, stream).await.map(|(events, _)| events)
    }

    /// `search_events`, also telling how many pages the device sent and how
    /// many windows they listed before merging.
    async fn search_events_counting(
        &mut self,
        channel_id: u8,
        day: CivilTime,
        stream: RecordStream,
    ) -> crate::Result<(Vec<RecordedEvent>, (usize, usize))> {
        if self.event_search_unanswered {
            return Ok((Vec::new(), (0, 0)));
        }
        self.close_finished_replay().await;
        self.ensure_no_stream("searching events")?;
        if !CivilTime::start_of_day(day.year, day.month, day.day).is_valid() {
            return Err(Error::ProtocolError("not a valid day".to_string()));
        }
        let uid = self.channel_uid(channel_id);
        let start = FindAlarmVideo::whole_day(channel_id, uid, stream, day.year, day.month, day.day);
        let reply = self
            .channel_request(MSG_ID_EVENT_SEARCH_START, channel_id, start.to_bytes(), EVENT_SEARCH_REPLY_WAIT)
            .await;
        let reply = match reply {
            Err(Error::ReplyTimeout) => {
                self.event_search_unanswered = true;
                return Err(Error::ReplyTimeout);
            }
            other => other?,
        };
        expect_ok(&reply, "event search")?;
        let handle = reply_xml(&reply)?
            .find_alarm_video
            .and_then(|reply| reply.file_handle)
            .ok_or_else(|| Error::ProtocolError("the event search reply has no search handle".to_string()))?;
        let page_request = FindAlarmVideo::page(channel_id, handle).to_bytes();
        let pages = self.event_pages(channel_id, &page_request).await;
        // Release the search whatever happened to the pages.
        let ended =
            self.channel_request(MSG_ID_EVENT_SEARCH_END, channel_id, page_request, EVENT_SEARCH_REPLY_WAIT).await;
        if let Err(e) = ended {
            if debug_replay() {
                eprintln!("replay: ending the event search failed: {e}");
            }
        }
        let pages = pages?;
        let counts = (pages.pages, pages.entries);
        Ok((merge_events(pages.listed), counts))
    }

    /// Asks for pages until one says it is the last (see
    /// [`EventPages::take`]).
    async fn event_pages(&mut self, channel_id: u8, page_request: &[u8]) -> crate::Result<EventPages> {
        let mut pages = EventPages::default();
        loop {
            let reply = self
                .channel_request(MSG_ID_EVENT_SEARCH_NEXT, channel_id, page_request.to_vec(), EVENT_SEARCH_REPLY_WAIT)
                .await?;
            if reply.meta.response_code != 200 {
                if pages.pages == 0 {
                    expect_ok(&reply, "event search page request")?;
                }
                // A refusal after a page: no more pages.
                break;
            }
            let page = reply_xml(&reply)?.alarm_video_info.unwrap_or_default();
            if !pages.take(&page) {
                break;
            }
        }
        Ok(pages)
    }

    fn next_replay_seq(&mut self) -> u32 {
        let seq = replay_seq_for(unix_now(), self.replay_opens, self.last_replay_seq);
        self.replay_opens = self.replay_opens.wrapping_add(1);
        self.last_replay_seq = Some(seq);
        seq
    }

    /// Plays `file` (from [`ReolinkClient::search_day`]) from `seek_time`
    /// (device-local) at `speed`, on the file's own channel and stream. A
    /// seek before the file's start starts at its start; a seek at or after
    /// its end is refused (the device would send one I-frame and stop — use
    /// [`next_file`] instead). The footage starts at a key frame from about
    /// 1.1 s before `seek_time` to about one GOP after it (observed).
    ///
    /// A playback already running on this client is closed first (seeking,
    /// pausing and changing speed are all a close and a new open). Live
    /// video on the device must have been stopped by the caller. Pushes
    /// keep being delivered through `take_channel_updates` while playing.
    pub async fn start_replay(
        &mut self,
        file: &RecordingFile,
        seek_time: CivilTime,
        speed: ReplaySpeed,
    ) -> crate::Result<ReplayStream> {
        self.start_replay_with(file, ReplayResume::at(seek_time), speed, 0, speed.plays_audio(), None).await
    }

    /// Opens `file` again to carry on from an earlier opening of it (after
    /// a stall, or to let a slow-motion backlog drain) without a gap and
    /// without repeats: at `from.seek_time`, dropping the frames at or
    /// before `from.after_microseconds` (see [`ReplayStatus::resume_point`]).
    /// Otherwise as [`ReolinkClient::start_replay`].
    pub async fn resume_replay(
        &mut self,
        file: &RecordingFile,
        from: ReplayResume,
        speed: ReplaySpeed,
    ) -> crate::Result<ReplayStream> {
        self.start_replay_with(file, from, speed, 0, speed.plays_audio(), None).await
    }

    /// `start_replay` / `resume_replay` with the seek request's `channelId`,
    /// the audio switch and the probe's device label (`Some` only when the
    /// probe runs it). The message header always names the file's channel;
    /// the `channelId` inside the seek has made no difference: 0 and the
    /// real channel gave the same footage on a Home Hub Pro (channel 2,
    /// whose channel 0 is another camera) and an RLN36 NVR (channels 0, 2
    /// and 13), so 0 is kept.
    async fn start_replay_with(
        &mut self,
        file: &RecordingFile,
        from: ReplayResume,
        speed: ReplaySpeed,
        seek_channel: u8,
        forward_audio: bool,
        probe: Option<&str>,
    ) -> crate::Result<ReplayStream> {
        if self.video_task.as_ref().is_some_and(|t| !t.is_finished()) {
            return Err(Error::ProtocolError(
                "live video is running on this device; stop it before playing a recording".to_string(),
            ));
        }
        if self.replay.is_some() || self.replay_task.is_some() {
            let _ = self.stop_replay().await;
        }
        if from.seek_time >= file.end {
            return Err(Error::ProtocolError("the seek time is at or after the end of the recording".to_string()));
        }
        let from = ReplayResume { seek_time: from.seek_time.max(file.start), ..from };
        let seek_time = from.seek_time;
        self.connection.lock().await.set_replay_buffering(true);
        let opened = match self.open_replay(file, seek_time, speed, seek_channel, probe).await {
            Ok(opened) => opened,
            Err(e) => {
                self.connection.lock().await.set_replay_buffering(false);
                return Err(e);
            }
        };
        let info = opened.info;
        let seq = opened.seq;
        let progress = ReplayProgress::new(file, from, speed, seq, info);
        progress.on_foreign(opened.foreign);
        let units = self.spawn_replay_task(opened, from.after_microseconds, progress.clone(), forward_audio, probe);
        self.replay = Some(ActiveReplay { file: file.clone(), progress: progress.clone() });
        Ok(ReplayStream { units, info, seq, progress })
    }

    /// Seek (123), then open (5) and wait for the info block. If the open
    /// was sent but did not succeed, the playback is closed again.
    async fn open_replay(
        &mut self,
        file: &RecordingFile,
        seek_time: CivilTime,
        speed: ReplaySpeed,
        seek_channel: u8,
        probe: Option<&str>,
    ) -> crate::Result<OpenedReplay> {
        let seq = self.next_replay_seq();
        let channel_id = file.channel_id;
        let seek = ReplaySeek::new(seek_channel, seq, seek_time).to_bytes();
        let seek_msg_num = self.send_request(MSG_ID_REPLAY_SEEK, channel_id, seek).await?;
        // Data of an earlier playback still trailing in are dropped while
        // waiting, and counted with those `await_open` drops.
        let reply = self.await_reply_counting(MSG_ID_REPLAY_SEEK, seek_msg_num, REPLY_TIMEOUT).await;
        if let Some(tag) = probe {
            match &reply {
                Ok((bc, _)) => eprintln!(
                    "PROBE replay [{tag}]: seek (123, channelId {seek_channel}, seq {seq}) reply code {} msg_num sent={} echoed={} hdr12-13=({},{}) class={:#06x} payload={}",
                    bc.meta.response_code,
                    seek_msg_num,
                    bc.meta.msg_num,
                    bc.meta.channel_id,
                    bc.meta.stream_type,
                    bc.meta.class,
                    payload_len(bc),
                ),
                Err(e) => eprintln!("PROBE replay [{tag}]: seek (123, channelId {seek_channel}) got no reply: {e}"),
            }
        }
        let (reply, foreign_before_open) = reply?;
        expect_ok(&reply, "playback seek")?;

        let open = ReplayOpen::new(
            channel_id,
            file.id.clone(),
            self.channel_uid(channel_id),
            file.stream,
            speed.play_speed_tag(),
        );
        let open_msg_num = self.send_request(MSG_ID_REPLAY, channel_id, open.to_bytes()).await?;
        match self.await_open(seq, open_msg_num, probe).await {
            Ok((info, early, foreign)) => {
                Ok(OpenedReplay { seq, open_msg_num, info, early, foreign: foreign_before_open + foreign })
            }
            Err(e) => {
                // The device may have started anyway; do not leave it
                // sending.
                let _ = self.send_close(file).await;
                Err(e)
            }
        }
    }

    /// Waits for the start of the playback with sequence number `seq`: the
    /// info block (msg 5 without a sequence number, code 200, the 32-byte
    /// info block as payload), or failing that its first data. A code-200
    /// msg 5 whose payload is not an info block is not taken for it. Data
    /// of other playbacks are dropped (and counted); pushes are passed on;
    /// another code is a refusal.
    async fn await_open(
        &mut self,
        seq: u32,
        open_msg_num: u16,
        probe: Option<&str>,
    ) -> crate::Result<(Option<InfoBlock>, Vec<Bc>, u64)> {
        let connection = Arc::clone(&self.connection);
        let encryption = self.encryption.clone();
        let mut image_chunks = HashMap::new();
        let mut foreign = 0usize;
        let wait = async {
            loop {
                let bc = match connection.lock().await.recv_bc(&encryption).await {
                    Ok(bc) => bc,
                    Err(Error::ReplyTimeout) => continue,
                    Err(e) => return Err(e),
                };
                if bc.meta.msg_id != MSG_ID_REPLAY {
                    self.forward_push(&bc, &mut image_chunks);
                    continue;
                }
                match bc.meta.replay_seq() {
                    Some(s) if s == seq => {
                        if let Some(tag) = probe {
                            eprintln!(
                                "PROBE replay [{tag}]: data (msg_num sent={open_msg_num} echoed={}) arrived before any info block",
                                bc.meta.msg_num
                            );
                        }
                        return Ok((None, vec![bc], foreign as u64));
                    }
                    Some(other) => {
                        foreign += 1;
                        if let Some(tag) = probe.filter(|_| foreign <= PROBE_LOGGED_MESSAGES) {
                            eprintln!(
                                "PROBE replay [{tag}]: dropped data of another playback before the open answer (seq {other}, ours {seq}, payload {})",
                                payload_len(&bc)
                            );
                        }
                        continue;
                    }
                    None => {}
                }
                let info = payload_of(&bc).and_then(parse_info_block);
                if let Some(tag) = probe {
                    eprintln!(
                        "PROBE replay [{tag}]: open answer: code {} msg_num sent={open_msg_num} echoed={} hdr12-13=({},{}) seq={:?} ours={seq} class={:#06x} payload={} info={}",
                        bc.meta.response_code,
                        bc.meta.msg_num,
                        bc.meta.channel_id,
                        bc.meta.stream_type,
                        bc.meta.replay_seq(),
                        bc.meta.class,
                        payload_len(&bc),
                        info.as_ref().map_or("none".to_string(), describe_info),
                    );
                }
                expect_ok(&bc, "playback")?;
                let Some(info) = info else {
                    // Not the open's answer: the info block, or this
                    // playback's first data, are still to come.
                    if debug_replay() {
                        eprintln!(
                            "replay: a code-200 message without an info block ({} payload bytes) before the open answer; still waiting",
                            payload_len(&bc)
                        );
                    }
                    continue;
                };
                if bc.meta.msg_num != open_msg_num && debug_replay() {
                    eprintln!(
                        "replay: the open answer carries msg_num {} (request had {open_msg_num}); accepted",
                        bc.meta.msg_num
                    );
                }
                return Ok((Some(info), Vec::new(), foreign as u64));
            }
        };
        tokio::time::timeout(REPLY_TIMEOUT, wait).await.map_err(|_| Error::ReplyTimeout)?
    }

    /// The reader of one playback: data with its sequence number become
    /// units (video, plus audio if `forward_audio`, in order), pushes are
    /// passed on, everything else is dropped. Silence is normal (between
    /// GOPs, and at the end of the file) and never ends it. Frames at or
    /// before `skip_through` repeat an earlier opening and are dropped too,
    /// as is audio ahead of the opening's first video frame.
    fn spawn_replay_task(
        &mut self,
        opened: OpenedReplay,
        skip_through: Option<u32>,
        progress: ReplayProgress,
        forward_audio: bool,
        probe: Option<&str>,
    ) -> ReceiverStream<crate::Result<ReplayUnit>> {
        let (tx, rx) = channel(32);
        let connection = Arc::clone(&self.connection);
        let encryption = self.encryption.clone();
        let channel_updates = self.channel_updates_tx.clone();
        let pending_images = Arc::clone(&self.pending_images);
        let channel_uids = Arc::clone(&self.channel_uids);
        let mut reader = ReplayReader::new(opened.seq, opened.open_msg_num, skip_through, probe.map(str::to_string));
        let mut early = opened.early.into_iter();
        self.replay_task = Some(tokio::spawn(async move {
            let mut image_chunks = HashMap::new();
            loop {
                let bc = match early.next() {
                    Some(bc) => bc,
                    None => {
                        let received =
                            tokio::time::timeout(READ_SLICE, async { connection.lock().await.recv_bc(&encryption).await })
                                .await;
                        match received {
                            Err(_) | Ok(Err(Error::ReplyTimeout)) => {
                                if tx.is_closed() {
                                    progress.mark_ended();
                                    return;
                                }
                                continue;
                            }
                            Ok(Err(e)) => {
                                progress.mark_ended();
                                let _ = tx.send(Err(e)).await;
                                return;
                            }
                            Ok(Ok(bc)) => bc,
                        }
                    }
                };
                if let Some(update) = pushed_update(&bc, &mut image_chunks, &pending_images, &channel_uids) {
                    let _ = channel_updates.send(update);
                    continue;
                }
                if !reader.is_ours(&bc) {
                    if bc.meta.msg_id == MSG_ID_REPLAY && bc.meta.replay_seq().is_some() {
                        progress.on_foreign(1);
                    }
                    continue;
                }
                progress.on_data(payload_len(&bc));
                for (unit, keyframe_time) in reader.feed(bc) {
                    if let BcMediaMessage::Video(frame) = &unit {
                        if frame.is_keyframe {
                            progress.on_keyframe(frame.microseconds, keyframe_time);
                        }
                    }
                    if reader.is_repeat(&unit) {
                        continue;
                    }
                    match unit {
                        BcMediaMessage::Video(frame) => {
                            let (microseconds, keyframe) = (frame.microseconds, frame.is_keyframe);
                            if !send_unit(&tx, ReplayUnit::Video(frame), &progress).await {
                                progress.mark_ended();
                                return; // the units were dropped
                            }
                            // Counted once handed on: what the consumer has
                            // is what a resume carries on from.
                            progress.on_frame(microseconds, keyframe);
                        }
                        BcMediaMessage::Audio(aac) if forward_audio => {
                            // In order with the video, on the same stream.
                            if !send_unit(&tx, ReplayUnit::Audio(aac), &progress).await {
                                progress.mark_ended();
                                return;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }));
        ReceiverStream::new(rx)
    }

    /// Ends the running playback: stops its reader and closes it on the
    /// device (7), waiting for the device's answer while dropping the data
    /// still trailing in. `Ok` with nothing playing. The playback is gone
    /// from this client whatever the device answers.
    pub async fn stop_replay(&mut self) -> crate::Result<()> {
        self.stop_replay_within(REPLY_TIMEOUT).await
    }

    /// `stop_replay`, waiting at most `wait` for the device's answer; the
    /// answer only confirms the close, and data still trailing in are
    /// dropped by sequence number by whatever reads next.
    pub async fn stop_replay_within(&mut self, wait: Duration) -> crate::Result<()> {
        if let Some(task) = self.replay_task.take() {
            task.abort();
            let _ = task.await;
        }
        let Some(active) = self.replay.take() else {
            return Ok(());
        };
        active.progress.mark_ended();
        let result = match self.send_close(&active.file).await {
            Ok(msg_num) => match self.await_reply_within(MSG_ID_REPLAY_CLOSE, msg_num, wait).await {
                Ok(reply) => expect_ok(&reply, "playback close"),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        self.connection.lock().await.set_replay_buffering(false);
        result
    }

    /// Sends the close (7) for `file`: channel, name and stream only.
    async fn send_close(&mut self, file: &RecordingFile) -> crate::Result<u16> {
        let close = ReplayClose::new(file.channel_id, file.name.clone(), file.stream);
        self.send_request(MSG_ID_REPLAY_CLOSE, file.channel_id, close.to_bytes()).await
    }

    /// The running playback's progress, if one runs.
    pub fn replay_progress(&self) -> Option<ReplayProgress> {
        self.replay.as_ref().map(|r| r.progress.clone())
    }

    /// Reads pushes until the channel list has given `channel_id` a camera
    /// UID, for up to `wait`.
    async fn wait_for_channel_uid(&mut self, channel_id: u8, wait: Duration) -> bool {
        let connection = Arc::clone(&self.connection);
        let encryption = self.encryption.clone();
        let mut image_chunks = HashMap::new();
        let deadline = tokio::time::Instant::now() + wait;
        while self.channel_uid(channel_id).is_none() {
            let received = tokio::time::timeout_at(deadline, async { connection.lock().await.recv_bc(&encryption).await }).await;
            match received {
                Ok(Ok(bc)) => self.forward_push(&bc, &mut image_chunks),
                Ok(Err(Error::ReplyTimeout)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        self.channel_uid(channel_id).is_some()
    }

    /// The latest recorded day of channel `channel_id` from the calendar of
    /// the current month (or the one before), reported on stderr with lines
    /// that start with `prefix`.
    async fn probe_latest_day(&mut self, channel_id: u8, prefix: &str) -> Option<CivilTime> {
        let now = CivilTime::from_epoch_seconds(unix_now() as i64);
        for (year, month) in current_and_previous_month(now) {
            match self.search_month(channel_id, year, month).await {
                Ok(days) => {
                    eprintln!("{prefix}: calendar {year}-{month:02}: days with recordings {days:?}");
                    if let Some(&d) = days.last() {
                        return Some(CivilTime::start_of_day(year, month, d));
                    }
                }
                Err(e) => eprintln!("{prefix}: calendar {year}-{month:02} failed: {e}"),
            }
        }
        None
    }

    /// Diagnostic-only (`REOLING_PROBE_EVENTS=<channel>`): lists the event
    /// windows of the latest recorded day of one channel, for the sub and
    /// the main stream, and reports on stderr with lines that start with
    /// "PROBE events [tag]", `tag` naming the device (every device the app
    /// connects to runs the probe): how many events and pages, how long the
    /// search took, the span from the first start to the last end, a
    /// histogram of the kinds, how many events lie inside a listed
    /// recording, and the first few windows. Never prints the camera UID.
    pub(super) async fn probe_events(&mut self, channel_id: u8, tag: &str) {
        eprintln!("PROBE events [{tag}]: channel {channel_id}");
        let has_uid = self.wait_for_channel_uid(channel_id, Duration::from_secs(10)).await;
        eprintln!(
            "PROBE events [{tag}]: camera uid for channel {channel_id}: {}",
            if has_uid { "present (sent in the requests)" } else { "absent (requests go without <uid>)" }
        );
        let Some(day) = self.probe_latest_day(channel_id, &format!("PROBE events [{tag}]")).await else {
            eprintln!("PROBE events [{tag}]: no recorded day found; done");
            return;
        };
        for stream in [RecordStream::Sub, RecordStream::Main] {
            let files = match self.search_day(channel_id, day, stream).await {
                Ok(files) => files,
                Err(e) => {
                    eprintln!("PROBE events [{tag}]: {stream:?} day search failed: {e}");
                    Vec::new()
                }
            };
            let started = Instant::now();
            let (events, (pages, entries)) = match self.search_events_counting(channel_id, day, stream).await {
                Ok(found) => found,
                Err(e) => {
                    eprintln!("PROBE events [{tag}]: {stream:?} event search failed after {:.2} s: {e}", started.elapsed().as_secs_f64());
                    continue;
                }
            };
            let mut kinds: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
            for event in &events {
                for kind in &event.kinds {
                    *kinds.entry(kind).or_default() += 1;
                }
            }
            let multi = events.iter().filter(|e| e.kinds.len() > 1).count();
            let inside = events.iter().filter(|e| files.iter().any(|f| f.contains(e.start))).count();
            let span = match (events.first(), events.iter().map(|e| e.end).max()) {
                (Some(first), Some(last)) => format!("{} .. {}", civil(&first.start), civil(&last)),
                _ => "none".to_string(),
            };
            eprintln!(
                "PROBE events [{tag}]: {stream:?} on {:04}-{:02}-{:02}: {} events from {entries} listed windows in {pages} pages, {:.2} s; span {span}; {multi} windows with several kinds; {inside} of {} start inside one of the {} listed files",
                day.year,
                day.month,
                day.day,
                events.len(),
                started.elapsed().as_secs_f64(),
                events.len(),
                files.len(),
            );
            eprintln!("PROBE events [{tag}]: {stream:?} kinds {kinds:?}");
            for event in events.iter().take(8) {
                eprintln!(
                    "PROBE events [{tag}]:   {} .. {} ({} s) {}",
                    civil(&event.start),
                    civil(&event.end),
                    event.end.to_epoch_seconds() - event.start.to_epoch_seconds(),
                    event.kinds.join(",")
                );
            }
        }
        eprintln!("PROBE events [{tag}]: done");
    }

    /// Diagnostic-only (`REOLING_PROBE_REPLAY=<channel>`): a scripted
    /// playback session on one channel, reported on stderr with lines that
    /// start with "PROBE replay [tag]", `tag` naming the device (every
    /// device the app connects to runs the probe). It reads the calendar of
    /// the current month (or the one before), searches the sub stream of
    /// the latest recorded day, opens one file (see `probe_pick`) at its
    /// start with seek `channelId` 0, describes the info block, the first data messages
    /// (`msg_num` sent and echoed, header bytes 12–13, sequence number) and
    /// the first key-frame headers, counts what arrives for about 15 s and
    /// closes it. Then it opens the same file twice at a time inside it,
    /// once with seek `channelId` 0 and once with the real channel (the
    /// header always names the real channel), and reports how far the first
    /// frame's footage time lies from the one of the opening at the start:
    /// about the seek offset (from about 1.1 s before it to about one GOP
    /// after it, where the device was observed to start) when the seek was
    /// honoured, about zero when it was not. Last, unless it
    /// reached the file's end, it resumes the first of those openings at
    /// its resume point and reports the gap between the last frame handed
    /// on before and the first one after. Never prints
    /// the camera UID or a recording `Id`.
    pub(super) async fn probe_replay(&mut self, channel_id: u8, tag: &str) {
        eprintln!("PROBE replay [{tag}]: channel {channel_id}");
        let has_uid = self.wait_for_channel_uid(channel_id, Duration::from_secs(10)).await;
        eprintln!(
            "PROBE replay [{tag}]: camera uid for channel {channel_id}: {}",
            if has_uid { "present (sent in the requests)" } else { "absent (requests go without <uid>)" }
        );
        let Some(day) = self.probe_latest_day(channel_id, &format!("PROBE replay [{tag}]")).await else {
            eprintln!("PROBE replay [{tag}]: no recorded day found; done");
            return;
        };
        let files = match self.search_day(channel_id, day, RecordStream::Sub).await {
            Ok(files) => files,
            Err(e) => {
                eprintln!("PROBE replay [{tag}]: day search failed: {e}");
                return;
            }
        };
        eprintln!("PROBE replay [{tag}]: {} sub-stream files on {:04}-{:02}-{:02}", files.len(), day.year, day.month, day.day);
        for file in &files {
            eprintln!(
                "PROBE replay [{tag}]:   {} {} .. {} ({} s) size {:?} audio {} encrypted {} supportSub {:?}",
                file.name,
                civil(&file.start),
                civil(&file.end),
                file.duration_seconds(),
                file.size_bytes,
                file.has_audio,
                file.encrypted,
                file.support_sub,
            );
        }
        let Some(file) = probe_pick(&files).cloned() else {
            eprintln!("PROBE replay [{tag}]: no playable file; done");
            return;
        };
        let baseline = self
            .probe_open(&file, ReplayResume::at(file.start), 0, Duration::from_secs(15), tag)
            .await
            .and_then(|s| s.first_microseconds);
        let offset = (file.duration_seconds() / 2).min(600);
        if offset < 5 {
            eprintln!("PROBE replay [{tag}]: file too short for a seek inside it; done");
            return;
        }
        let inside = file.start.add_seconds(offset);
        let mut seek_channels = vec![0];
        if channel_id != 0 {
            seek_channels.push(channel_id);
        }
        let mut to_resume = None;
        for seek_channel in seek_channels {
            let status = self.probe_open(&file, ReplayResume::at(inside), seek_channel, Duration::from_secs(6), tag).await;
            let first = status.as_ref().and_then(|s| s.first_microseconds);
            if to_resume.is_none() {
                to_resume = status;
            }
            match (baseline, first) {
                (Some(base), Some(first)) => {
                    let delta = f64::from(first.wrapping_sub(base)) / 1e6;
                    // Between about 1.1 s before and about one GOP after
                    // the offset: the seek was honoured; about zero: the
                    // file played from its start.
                    let verdict = if (delta - offset as f64).abs() <= 6.0 {
                        "seek honoured"
                    } else if delta < 1.0 {
                        "seek NOT honoured"
                    } else {
                        "unclear"
                    };
                    eprintln!(
                        "PROBE replay [{tag}]: seek +{offset} s with channelId {seek_channel}: first frame {delta:.2} s after the start opening's first frame ({verdict})"
                    );
                }
                _ => eprintln!(
                    "PROBE replay [{tag}]: seek +{offset} s with channelId {seek_channel}: no first frame to compare (start {baseline:?}, inside {first:?})"
                ),
            }
        }
        if let Some(before) = to_resume {
            let resume = before.resume_point();
            eprintln!(
                "PROBE replay [{tag}]: resume point of the +{offset} s opening: seek {} (its last key frame {:?}), after footage time {:?} us",
                civil(&resume.seek_time),
                before.last_keyframe,
                resume.after_microseconds,
            );
            if before.delivered_to_end() {
                // As in the app: an opening that reached the file's end
                // goes on with the next file, never resumes this one.
                eprintln!("PROBE replay [{tag}]: resume skipped: that opening reached the file's end");
            } else if resume.seek_time < file.end {
                let after = self.probe_open(&file, resume, 0, Duration::from_secs(6), tag).await;
                match (resume.after_microseconds, after.as_ref().and_then(|s| s.first_microseconds)) {
                    (Some(last), Some(next)) => {
                        let gap = f64::from(next.wrapping_sub(last) as i32) / 1e6;
                        let verdict = if gap > 0.0 && gap <= 1.0 { "no gap" } else { "GAP" };
                        eprintln!(
                            "PROBE replay [{tag}]: resume: first frame handed on {next} us, {gap:.3} s after the last one before ({verdict})"
                        );
                    }
                    (last, next) => eprintln!("PROBE replay [{tag}]: resume: nothing to compare (before {last:?}, after {next:?})"),
                }
            }
        }
        eprintln!("PROBE replay [{tag}]: done");
    }

    /// One probe playback of `file` from `from`; returns its progress after
    /// the close.
    async fn probe_open(
        &mut self,
        file: &RecordingFile,
        from: ReplayResume,
        seek_channel: u8,
        listen: Duration,
        tag: &str,
    ) -> Option<ReplayStatus> {
        eprintln!(
            "PROBE replay [{tag}]: opening {} at {} (+{} s from its start {}; 1x, sub), seek channelId {seek_channel}, dropping through {:?} us",
            file.name,
            civil(&from.seek_time),
            from.seek_time.to_epoch_seconds() - file.start.to_epoch_seconds(),
            civil(&file.start),
            from.after_microseconds,
        );
        let opening = Instant::now();
        let stream = match self.start_replay_with(file, from, ReplaySpeed::X1, seek_channel, false, Some(tag)).await {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("PROBE replay [{tag}]: open failed: {e}");
                return None;
            }
        };
        let opened_after = opening.elapsed();
        let ReplayStream { mut units, progress, seq, .. } = stream;
        let (mut video_frames, mut keyframes, mut video_bytes) = (0u64, 0u64, 0u64);
        let mut first_frame_at: Option<Instant> = None;
        let counting = async {
            while let Some(item) = units.next().await {
                match item {
                    Ok(ReplayUnit::Audio(_)) => {}
                    Ok(ReplayUnit::Video(frame)) => {
                        first_frame_at.get_or_insert_with(Instant::now);
                        video_frames += 1;
                        keyframes += u64::from(frame.is_keyframe);
                        video_bytes += frame.data.len() as u64;
                    }
                    Err(e) => {
                        eprintln!("PROBE replay [{tag}]: stream error: {e}");
                        break;
                    }
                }
            }
        };
        let _ = tokio::time::timeout(listen, counting).await;
        let status = progress.status();
        if let Some(at) = first_frame_at {
            let wall = at.elapsed().as_secs_f64();
            let footage = status.footage_micros as f64 / 1e6;
            eprintln!(
                "PROBE replay [{tag}]: seq {seq}: open answered after {:.2} s, first frame {:.2} s later; first frame footage time {:?} us; since the first frame {wall:.1} s wall, {footage:.1} s footage (lead {:.1} s)",
                opened_after.as_secs_f64(),
                at.duration_since(opening + opened_after).as_secs_f64(),
                status.first_microseconds,
                footage - wall,
            );
        }
        eprintln!(
            "PROBE replay [{tag}]: seq {seq}, after {listen:?}: {video_frames} video frames ({keyframes} key, {video_bytes} B); {} data messages, {} B; {} data messages of other playbacks dropped; footage {:.1} s, position {} ({}; seek plus footage {}), zone offset {:?} s, silent for {:.1} s, delivered to end {}, at end {}, stalled {}",
            status.data_messages,
            status.bytes,
            status.foreign_messages,
            status.footage_micros as f64 / 1e6,
            civil(&status.position()),
            if status.is_anchored() { "from key-frame times" } else { "estimated" },
            civil(&status.estimated_position()),
            status.zone_offset_seconds(),
            status.silent_for.as_secs_f64(),
            status.delivered_to_end(),
            status.at_end(),
            status.stalled(),
        );
        drop(units);
        match self.stop_replay().await {
            Ok(()) => eprintln!("PROBE replay [{tag}]: close (7) answered with code 200"),
            Err(e) => eprintln!("PROBE replay [{tag}]: close (7): {e}"),
        }
        Some(progress.status())
    }
}

/// Hands one unit to the consumer. A full channel means the consumer is
/// behind (the device sends ahead of real time); the wait for it is marked
/// so that it is not taken for device silence. `false` once the units
/// have been dropped.
async fn send_unit(
    tx: &tokio::sync::mpsc::Sender<crate::Result<ReplayUnit>>,
    unit: ReplayUnit,
    progress: &ReplayProgress,
) -> bool {
    match tx.try_send(Ok(unit)) {
        Ok(()) => true,
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
        Err(tokio::sync::mpsc::error::TrySendError::Full(item)) => {
            progress.set_consumer_blocked(true);
            let sent = tx.send(item).await.is_ok();
            progress.set_consumer_blocked(false);
            sent
        }
    }
}

/// The recorded days of a calendar reply, as day numbers of `year`/`month`.
/// The reply is for the one channel asked about, so when it names a
/// different channel number its only entry is used.
fn month_days(calendar: &DayRecords, channel_id: u8, year: u16, month: u8) -> Vec<u8> {
    let records = &calendar.day_record_list.records;
    let channel = if records.len() == 1 && records.iter().all(|r| r.channel_id != channel_id) {
        records[0].channel_id
    } else {
        channel_id
    };
    let mut days: Vec<u8> = calendar
        .recorded_days(channel)
        .into_iter()
        .filter(|d| d.year == year && d.month == month)
        .map(|d| d.day)
        .collect();
    days.sort_unstable();
    days.dedup();
    days
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::crypto::EncryptionProtocol;
    use crate::transport::discovery::PeerHandle;
    use tokio::net::UdpSocket;

    /// A synthetic 16-character camera UID.
    const UID: &str = "TESTCAMUID000001";

    fn t(h: u8, m: u8, s: u8) -> CivilTime {
        CivilTime::new(2026, 10, 17, h, m, s)
    }

    fn file(name: &str, start: CivilTime, end: CivilTime) -> RecordingFile {
        RecordingFile {
            channel_id: 0,
            name: name.to_string(),
            id: RecordingId::new(format!("1-0-0-01-{name}")),
            stream: RecordStream::Sub,
            start,
            end,
            size_bytes: Some(1 << 20),
            support_sub: Some(1),
            encrypted: false,
            has_audio: true,
        }
    }

    // ---- event list ----------------------------------------------------------

    fn window(start: CivilTime, end: CivilTime, kind: &str) -> AlarmVideo {
        AlarmVideo {
            start_time: Some(start),
            end_time: Some(end),
            alarm_type: Some(kind.to_string()),
            ..Default::default()
        }
    }

    fn page(finished: u8, entries: Vec<AlarmVideo>) -> AlarmVideoInfo {
        AlarmVideoInfo {
            finished: Some(finished),
            alarm_video_list: Some(AlarmVideoList { entries }),
            ..Default::default()
        }
    }

    fn event(start: CivilTime, end: CivilTime, kinds: &[&str]) -> RecordedEvent {
        RecordedEvent { start, end, kinds: kinds.iter().map(|k| k.to_string()).collect() }
    }

    #[test]
    fn listed_windows_merge_into_events() {
        // As a Home Hub lists them: one window under two kinds (once
        // twice), an overlapping window, and unsorted entries.
        let listed = events_of_page(&page(
            0,
            vec![
                window(t(2, 11, 22), t(2, 11, 49), "md"),
                window(t(2, 11, 21), t(2, 11, 55), "dog_cat"),
                window(t(2, 11, 21), t(2, 11, 55), "people"),
                window(t(2, 11, 21), t(2, 11, 55), "dog_cat"),
                window(t(5, 3, 6), t(5, 3, 30), "md, people"),
            ],
        ));
        assert_eq!(listed.len(), 6, "a kind list is one window per kind");
        assert_eq!(
            merge_events(listed),
            vec![
                event(t(2, 11, 21), t(2, 11, 55), &["dog_cat", "people"]),
                event(t(2, 11, 22), t(2, 11, 49), &["md"]),
                event(t(5, 3, 6), t(5, 3, 30), &["md", "people"]),
            ]
        );
        assert!(merge_events(Vec::new()).is_empty());
    }

    #[test]
    fn unusable_windows_are_dropped_and_empty_ones_given_a_second() {
        let listed = events_of_page(&page(
            1,
            vec![
                window(t(1, 0, 0), t(0, 59, 0), "md"),
                window(t(1, 0, 0), t(1, 0, 0), "md"),
                AlarmVideo { start_time: Some(t(2, 0, 0)), alarm_type: Some("md".to_string()), ..Default::default() },
                AlarmVideo { start_time: Some(t(3, 0, 0)), end_time: Some(t(3, 0, 5)), ..Default::default() },
                window(t(4, 0, 0), t(4, 0, 5), " , "),
            ],
        ));
        assert_eq!(listed, vec![ListedEvent { start: t(1, 0, 0), end: t(1, 0, 1), kind: "md".to_string() }]);
    }

    #[test]
    fn event_pages_are_asked_for_until_the_last_one() {
        let full: Vec<AlarmVideo> = (0..50).map(|i| window(t(0, i, 0), t(0, i, 30), "md")).collect();
        let mut pages = EventPages::default();
        assert!(pages.take(&page(0, full.clone())), "not the last page: ask for the next");
        assert!(!pages.take(&page(1, full[..30].to_vec())), "the last page");
        assert_eq!((pages.pages, pages.entries, pages.listed.len()), (2, 80, 80));
        // A full page that says it is the last is the last.
        let mut pages = EventPages::default();
        assert!(!pages.take(&page(1, full.clone())));
        // A page listing nothing ends the search, whatever it says.
        let mut pages = EventPages::default();
        assert!(!pages.take(&page(0, Vec::new())));
        assert_eq!((pages.pages, pages.entries), (1, 0));
        // A page that does not say is not the last...
        let mut pages = EventPages::default();
        let unsaid = AlarmVideoInfo { alarm_video_list: Some(AlarmVideoList { entries: full.clone() }), ..Default::default() };
        assert!(pages.take(&unsaid));
        // ...and the page limit still ends the search.
        let mut pages = EventPages::default();
        for _ in 1..MAX_EVENT_PAGES {
            assert!(pages.take(&page(0, full.clone())));
        }
        assert!(!pages.take(&page(0, full.clone())));
        assert_eq!(pages.pages, MAX_EVENT_PAGES);
        assert_eq!(merge_events(pages.listed).len(), 50, "the same windows on every page merge into one set");
    }

    // ---- sequence numbers --------------------------------------------------

    #[test]
    fn seq_is_the_unix_time_plus_the_opens_so_far() {
        assert_eq!(replay_seq_for(1_792_348_477, 0, None), 1_792_348_477);
        assert_eq!(replay_seq_for(1_792_348_477, 3, Some(1_792_348_470)), 1_792_348_480);
    }

    #[test]
    fn seq_never_repeats_or_goes_back() {
        // Same second, or a clock that went back: still above the last one.
        assert_eq!(replay_seq_for(1_792_348_477, 1, Some(1_792_348_477)), 1_792_348_478);
        assert_eq!(replay_seq_for(1_792_348_000, 1, Some(1_792_348_477)), 1_792_348_478);
        let mut previous = None;
        for opened in 0..50 {
            let seq = replay_seq_for(1_792_348_477, opened, previous);
            assert!(previous.map_or(true, |p| seq > p));
            assert!(is_usable_replay_seq(seq));
            previous = Some(seq);
        }
    }

    #[test]
    fn seq_stays_usable_with_a_wrong_clock_or_a_fixed_class() {
        for now in [0u64, 1_000, u64::MAX, 3_000_000_000] {
            let seq = replay_seq_for(now, 0, None);
            assert!(is_usable_replay_seq(seq), "{now} gave {seq}");
        }
        // A time whose high half is the request class 0x6414 is skipped.
        let seq = replay_seq_for(0x6414_0005, 0, None);
        assert!(is_usable_replay_seq(seq));
        assert_eq!(seq, 0x6415_0000);
        // Data with that number are framed as playback data.
        assert_eq!(replay_seq(MSG_ID_REPLAY, (seq >> 16) as u16, seq as u16), Some(seq));
    }

    // ---- file list -----------------------------------------------------------

    #[test]
    fn file_entries_keep_what_playback_needs() {
        let entry = FileInfo {
            channel_id: Some(9),
            handle: Some(0),
            name: Some("1020261017071133".to_string()),
            id: Some(RecordingId::new("1-9-0-01260917041133-00000")),
            stream_type: Some("subStream".to_string()),
            contains_audio: Some(1),
            record_type: Some("none".to_string()),
            size_l: Some(10_485_760),
            size_h: Some(1),
            support_sub: Some(1),
            encrypt: Some(0),
            start_time: Some(t(7, 11, 33)),
            end_time: Some(t(7, 13, 2)),
            file_count: None,
        };
        let f = RecordingFile::from_entry(9, RecordStream::Main, &entry).unwrap();
        assert_eq!(f.channel_id, 9);
        assert_eq!(f.name, "1020261017071133");
        assert_eq!(f.id.as_str(), "1-9-0-01260917041133-00000");
        assert_eq!(f.stream, RecordStream::Sub, "the entry's own stream wins");
        assert_eq!((f.start, f.end), (t(7, 11, 33), t(7, 13, 2)));
        assert_eq!(f.size_bytes, Some((1 << 32) + 10_485_760));
        assert_eq!(f.support_sub, Some(1));
        assert!(f.has_audio && !f.encrypted);
        assert_eq!(f.duration_seconds(), 89);
        // The search handle entry of a start reply is not a file.
        let handle_only = FileInfo { channel_id: Some(0), handle: Some(0), ..Default::default() };
        assert!(RecordingFile::from_entry(0, RecordStream::Sub, &handle_only).is_none());
    }

    #[test]
    fn files_sort_by_start_and_repeat_no_name() {
        let a = file("0120261017082107", t(8, 21, 7), t(9, 21, 8));
        let b = file("0120261017092107", t(9, 21, 7), t(10, 21, 5));
        let mut files = vec![b.clone(), a.clone(), b.clone()];
        sort_and_dedup_files(&mut files);
        assert_eq!(files, vec![a, b]);
    }

    /// Two files that overlap by one second, then a gap, then a third.
    fn overlapping_day() -> Vec<RecordingFile> {
        vec![
            file("0120261017082107", t(8, 21, 7), t(9, 21, 8)),
            file("0120261017092107", t(9, 21, 7), t(10, 21, 5)),
            file("0120261017110000", t(11, 0, 0), t(11, 30, 0)),
        ]
    }

    #[test]
    fn a_time_in_an_overlap_opens_the_later_file() {
        let files = overlapping_day();
        assert_eq!(file_for_time(&files, t(8, 30, 0)), Some((0, t(8, 30, 0))));
        // 09:21:07 is in both: the later file, which starts there.
        assert_eq!(file_for_time(&files, t(9, 21, 7)), Some((1, t(9, 21, 7))));
        // The end of a file is not in it (half-open).
        assert_eq!(file_for_time(&files, t(9, 21, 8)), Some((1, t(9, 21, 8))));
        // In the gap and before the first file: the next file at its start.
        assert_eq!(file_for_time(&files, t(10, 40, 0)), Some((2, t(11, 0, 0))));
        assert_eq!(file_for_time(&files, t(0, 0, 0)), Some((0, t(8, 21, 7))));
        // After the last one: nothing.
        assert_eq!(file_for_time(&files, t(11, 30, 0)), None);
    }

    #[test]
    fn the_next_file_starts_after_the_overlap_and_never_reopens_the_ended_one() {
        let files = overlapping_day();
        // Overlap: the next file is opened at the ended file's end.
        assert_eq!(next_file(&files, 0), Some((1, t(9, 21, 8))));
        // Gap: at the next file's start.
        assert_eq!(next_file(&files, 1), Some((2, t(11, 0, 0))));
        assert_eq!(next_file(&files, 2), None);
        assert_eq!(next_file(&files, 7), None);

        // An entry lying inside the ended file is skipped, not opened at
        // (or past) its own end.
        let files = vec![
            file("a", t(8, 0, 0), t(9, 0, 0)),
            file("b", t(8, 59, 58), t(9, 0, 0)),
            file("c", t(8, 59, 59), t(10, 0, 0)),
        ];
        assert_eq!(next_file(&files, 0), Some((2, t(9, 0, 0))));
        // Abutting files: the next one at its own start.
        let files = vec![file("a", t(9, 59, 57), t(10, 30, 1)), file("b", t(10, 30, 1), t(11, 0, 3))];
        assert_eq!(next_file(&files, 0), Some((1, t(10, 30, 1))));
    }

    // ---- progress and end of file ------------------------------------------

    fn status(speed: ReplaySpeed, footage_s: u64, frames: u64, silent_for: Duration) -> ReplayStatus {
        ReplayStatus {
            seq: 1_792_348_477,
            speed,
            seek_time: t(9, 21, 7),
            skip_through_microseconds: None,
            file_start: t(9, 21, 7),
            file_end: t(10, 21, 5),
            info: None,
            first_microseconds: Some(4_000_000_000),
            last_microseconds: None,
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

    #[test]
    fn progress_accumulates_footage_across_the_clock_wrap() {
        let f = file("0120261017092107", t(9, 21, 7), t(10, 21, 5));
        let progress = ReplayProgress::new(&f, ReplayResume::at(t(9, 30, 0)), ReplaySpeed::X1, 1_792_348_477, None);
        progress.on_data(100);
        progress.on_frame(u32::MAX - 49_999, true);
        progress.on_frame(50_000, false); // wrapped: 100 ms later
        progress.on_frame(1_050_000, false);
        progress.on_frame(u32::MAX / 2, false); // a jump: not footage
        let s = progress.status();
        assert_eq!((s.frames, s.keyframes, s.data_messages, s.bytes), (4, 1, 1, 100));
        assert_eq!(s.footage_micros, 1_100_000);
        assert_eq!(s.position(), t(9, 30, 1));
        assert_eq!(s.position_of(50_000), Some(t(9, 30, 0)));
        assert_eq!(s.position_of(2_100_000), Some(t(9, 30, 2)));
        assert!(!s.ended);
        progress.mark_ended();
        assert!(progress.status().ended);
    }

    #[test]
    fn waiting_for_the_consumer_is_not_silence() {
        let f = file("0120261017092107", t(9, 21, 7), t(10, 21, 5));
        let progress = ReplayProgress::new(&f, ReplayResume::at(t(9, 21, 7)), ReplaySpeed::X16, 1, None);
        progress.set_consumer_blocked(true);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(progress.status().silent_for, Duration::ZERO);
        progress.set_consumer_blocked(false);
        assert!(progress.status().silent_for < Duration::from_secs(1));
    }

    #[test]
    fn end_of_file_needs_the_footage_at_the_end_and_a_pause_or_a_long_silence() {
        // The file runs 09:21:07..10:21:05 (3598 s); footage up to 2 s
        // before its end.
        let to_end = 3_596;
        // 4 s before the end: an I-frame-only 16x playback has delivered
        // everything, any other speed has not.
        assert!(status(ReplaySpeed::X16, 3_594, 400, Duration::ZERO).delivered_to_end());
        assert!(!status(ReplaySpeed::X8, 3_594, 400, Duration::ZERO).delivered_to_end());
        assert!(!status(ReplaySpeed::X1, 3_594, 400, Duration::from_secs(5)).at_end());
        // Delivered to the end, still within a GOP pause at 1x: not yet.
        let s = status(ReplaySpeed::X1, to_end, 400, Duration::from_secs(3));
        assert!(s.delivered_to_end());
        assert!(!s.at_end());
        // A pause longer than one GOP at 1x: the end.
        assert!(status(ReplaySpeed::X1, to_end, 400, Duration::from_secs(5)).at_end());
        // At 16x the GOPs come 16 times faster.
        assert!(status(ReplaySpeed::X16, to_end, 400, Duration::from_millis(1_100)).at_end());
        // Slow motion is delivered at 1x.
        assert!(!status(ReplaySpeed::Half, to_end, 400, Duration::from_secs(3)).at_end());
        // Mid-file: the usual 4 s pause at 1x is not the end...
        let mid = status(ReplaySpeed::X1, 600, 400, Duration::from_secs(4));
        assert!(!mid.delivered_to_end() && !mid.at_end());
        assert!(!mid.stalled());
        // ...and a long silence mid-file is a stall, never the end: the rest
        // of the file is still to come.
        let stall = status(ReplaySpeed::X1, 600, 400, END_SILENCE);
        assert!(stall.stalled() && !stall.at_end());
        assert!(status(ReplaySpeed::X1, 600, 400, Duration::from_secs(40)).stalled());
        assert!(!status(ReplaySpeed::X1, 600, 400, Duration::from_secs(40)).at_end());
        // Nothing received after the open, for long: the end.
        let nothing = status(ReplaySpeed::X1, 0, 0, END_SILENCE);
        assert!(nothing.at_end() && !nothing.stalled());
        assert!(!status(ReplaySpeed::X1, 0, 0, Duration::from_secs(2)).at_end());
        // Delivered to the end and silent: the end, not a stall.
        let ended = status(ReplaySpeed::X1, to_end, 400, END_SILENCE);
        assert!(ended.at_end() && !ended.stalled());
    }

    #[test]
    fn an_open_just_before_the_end_ends_after_its_one_frame() {
        // Opened 1 s before the file's end: one I-frame, then nothing.
        let f = file("0120261017092107", t(9, 21, 7), t(10, 21, 5));
        for speed in [ReplaySpeed::X1, ReplaySpeed::X16] {
            let progress = ReplayProgress::new(&f, ReplayResume::at(t(10, 21, 4)), speed, 1, None);
            progress.on_frame(4_000_000_000, true);
            let mut s = progress.status();
            assert!(s.delivered_to_end());
            s.silent_for = Duration::from_millis(500);
            assert!(!s.at_end(), "not before the settle time");
            s.silent_for = Duration::from_secs(5);
            assert!(s.at_end() && !s.stalled(), "{speed:?}");
            s.silent_for = Duration::from_secs(60);
            assert!(s.at_end() && !s.stalled(), "{speed:?}");
        }
    }

    // ---- key-frame times, position and resume ---------------------------------

    fn civil_utc(h: u8, m: u8, s: u8) -> CivilTime {
        CivilTime::new(2026, 10, 19, h, m, s)
    }

    /// Unix time of 2026-10-19 `h:m:s` UTC.
    fn unix(h: u8, m: u8, s: u8) -> u32 {
        civil_utc(h, m, s).to_epoch_seconds() as u32
    }

    /// An NVR file as the probe found it: listed 21:57:00..21:57:35
    /// device-local (UTC+3), 18:57:00..18:57:35 UTC in the info block.
    fn nvr_file() -> RecordingFile {
        file("0120261019215700", civil_utc(21, 57, 0), civil_utc(21, 57, 35))
    }

    fn nvr_info() -> InfoBlock {
        InfoBlock { width: 896, height: 512, start_utc: Some(civil_utc(18, 57, 0)), end_utc: Some(civil_utc(18, 57, 35)) }
    }

    /// Footage times of two key frames of that file, 18:57:19 and 18:57:23.
    const KEY_19_US: u32 = 1_968_952_017;
    const KEY_23_US: u32 = 1_972_800_279;

    /// A key frame (with its time, if any) and the delta frames after it at
    /// 100 ms steps up to (not including) `until`.
    fn feed_gop(progress: &ReplayProgress, key_us: u32, time: Option<u32>, until: u32) {
        progress.on_keyframe(key_us, time);
        progress.on_frame(key_us, true);
        let mut us = key_us.wrapping_add(100_000);
        while (until.wrapping_sub(us) as i32) > 0 {
            progress.on_frame(us, false);
            us = us.wrapping_add(100_000);
        }
    }

    #[test]
    fn the_zone_offset_is_the_devices_own_to_the_quarter_hour() {
        let status = |file_start: CivilTime, start_utc: Option<CivilTime>| ReplayStatus {
            file_start,
            info: Some(InfoBlock { start_utc, ..nvr_info() }),
            ..status(ReplaySpeed::X1, 0, 0, Duration::ZERO)
        };
        assert_eq!(status(civil_utc(21, 57, 0), Some(civil_utc(18, 57, 0))).zone_offset_seconds(), Some(3 * 3_600));
        // A second or two between the listing and the info block.
        assert_eq!(status(civil_utc(21, 57, 0), Some(civil_utc(18, 57, 2))).zone_offset_seconds(), Some(3 * 3_600));
        assert_eq!(status(civil_utc(21, 57, 0), Some(civil_utc(18, 56, 59))).zone_offset_seconds(), Some(3 * 3_600));
        // Quarter-hour zones and zones west of UTC.
        assert_eq!(status(civil_utc(18, 57, 0), Some(civil_utc(13, 12, 0))).zone_offset_seconds(), Some(20_700));
        assert_eq!(status(civil_utc(8, 0, 0), Some(civil_utc(13, 0, 0))).zone_offset_seconds(), Some(-5 * 3_600));
        // No zone is 15 h off; no info block, or one without dates: unknown.
        assert_eq!(status(civil_utc(23, 0, 0), Some(civil_utc(8, 0, 0))).zone_offset_seconds(), None);
        assert_eq!(status(civil_utc(21, 57, 0), None).zone_offset_seconds(), None);
        assert_eq!(ReplayStatus { info: None, ..status(civil_utc(21, 57, 0), None) }.zone_offset_seconds(), None);
    }

    #[test]
    fn the_position_follows_the_key_frames_own_time_not_the_seek_time() {
        // Seek to 21:57:17; the device starts at the key frame of 18:57:19
        // UTC (21:57:19 local), 2 s later, as it did on the NVR.
        let progress = ReplayProgress::new(&nvr_file(), ReplayResume::at(civil_utc(21, 57, 17)), ReplaySpeed::X1, 1, Some(nvr_info()));
        feed_gop(&progress, KEY_19_US, Some(unix(18, 57, 19)), KEY_23_US);
        let s = progress.status();
        assert!(s.is_anchored());
        assert_eq!(s.zone_offset_seconds(), Some(10_800));
        // 3.8 s of footage after 21:57:19.
        assert_eq!(s.position(), civil_utc(21, 57, 22));
        assert_eq!(s.estimated_position(), civil_utc(21, 57, 20), "seek plus footage lags by the 2 s");
        feed_gop(&progress, KEY_23_US, Some(unix(18, 57, 23)), KEY_23_US + 2_050_000);
        let s = progress.status();
        assert_eq!(s.anchor, Some(FootageAnchor { microseconds: KEY_23_US, unix_micros: i64::from(unix(18, 57, 23)) * 1_000_000 }));
        assert_eq!(s.position(), civil_utc(21, 57, 25));
        assert_eq!(s.estimated_position(), civil_utc(21, 57, 22));
        // A frame shown before the latest key frame: placed backwards from it.
        assert_eq!(s.position_of(KEY_19_US + 500_000), Some(civil_utc(21, 57, 19)));
        assert_eq!(s.position_of(KEY_23_US - 100_000), Some(civil_utc(21, 57, 22)));
        assert_eq!(s.position_of(KEY_23_US), Some(civil_utc(21, 57, 23)));
    }

    #[test]
    fn the_anchor_keeps_its_finer_estimate_until_a_key_frame_disagrees() {
        let progress = ReplayProgress::new(&nvr_file(), ReplayResume::at(civil_utc(21, 57, 0)), ReplaySpeed::X1, 1, Some(nvr_info()));
        let t0 = i64::from(unix(18, 57, 0)) * 1_000_000;
        progress.on_keyframe(1_000_000, Some(unix(18, 57, 0)));
        // 4.6 s later the header says :04: the running estimate (:04.6)
        // agrees and is kept, finer than the whole second.
        progress.on_keyframe(5_600_000, Some(unix(18, 57, 4)));
        assert_eq!(progress.status().anchor.unwrap().unix_micros, t0 + 4_600_000);
        // 3.9 s later the header says :09, past the estimate (:08.5): the
        // estimate moves up to it.
        progress.on_keyframe(9_500_000, Some(unix(18, 57, 9)));
        assert_eq!(progress.status().anchor.unwrap().unix_micros, t0 + 9_000_000);
        // A footage clock that ran ahead by 2 s: set back to the header.
        progress.on_keyframe(15_500_000, Some(unix(18, 57, 13)));
        assert_eq!(progress.status().anchor, Some(FootageAnchor { microseconds: 15_500_000, unix_micros: t0 + 13_000_000 }));
    }

    #[test]
    fn anchoring_works_across_the_footage_clock_wrap() {
        let progress = ReplayProgress::new(&nvr_file(), ReplayResume::at(civil_utc(21, 57, 10)), ReplaySpeed::X1, 1, Some(nvr_info()));
        let key = u32::MAX - 499_999; // 0.5 s before the wrap
        feed_gop(&progress, key, Some(unix(18, 57, 10)), 1_500_000);
        let s = progress.status();
        assert_eq!(s.last_microseconds, Some(1_400_000));
        assert_eq!(s.position(), civil_utc(21, 57, 11), "1.9 s after the key frame");
        assert_eq!(s.position_of(key), Some(civil_utc(21, 57, 10)));
        // The next key frame, 3.5 s on, agrees with the estimate.
        progress.on_keyframe(3_000_000, Some(unix(18, 57, 13)));
        let anchor = progress.status().anchor.unwrap();
        assert_eq!(anchor.unix_micros, i64::from(unix(18, 57, 13)) * 1_000_000 + 500_000);
        assert_eq!(anchor.unix_micros_of(key), i64::from(unix(18, 57, 10)) * 1_000_000);
    }

    #[test]
    fn an_implausible_or_missing_key_frame_time_falls_back_to_the_estimate() {
        for (info, time) in [
            (Some(nvr_info()), Some(0)),                     // not a time
            (Some(nvr_info()), Some(unix(19, 57, 19))),      // an hour off the file
            (Some(nvr_info()), None),                        // no extra header
            (None, Some(unix(18, 57, 19))),                  // nothing to check it against
            (Some(InfoBlock { start_utc: None, end_utc: None, ..nvr_info() }), Some(unix(18, 57, 19))),
        ] {
            let progress = ReplayProgress::new(&nvr_file(), ReplayResume::at(civil_utc(21, 57, 17)), ReplaySpeed::X1, 1, info);
            feed_gop(&progress, KEY_19_US, time, KEY_23_US);
            let s = progress.status();
            assert!(!s.is_anchored(), "{info:?} {time:?}");
            assert_eq!(s.last_keyframe, Some(ReplayKeyframe { microseconds: KEY_19_US, unix_seconds: None }));
            assert_eq!(s.position(), civil_utc(21, 57, 20));
            assert_eq!(s.position_of(KEY_19_US), Some(civil_utc(21, 57, 17)));
        }
        // A key frame a little outside the info block's span is still believed.
        let progress = ReplayProgress::new(&nvr_file(), ReplayResume::at(civil_utc(21, 57, 17)), ReplaySpeed::X1, 1, Some(nvr_info()));
        progress.on_keyframe(KEY_19_US, Some(unix(18, 57, 50)));
        assert!(progress.status().is_anchored());
    }

    #[test]
    fn after_a_seek_inside_the_file_its_end_is_reached_not_a_stall() {
        // Opened at 10:00:00 inside 09:21:07..10:21:05. The device started
        // at a key frame 4 s later; footage up to the listed end gives an
        // estimate 4 s short of it.
        let inside = |footage_s: u64, silent_for: Duration| ReplayStatus {
            seek_time: t(10, 0, 0),
            ..status(ReplaySpeed::X1, footage_s, 400, silent_for)
        };
        let to_end = 1_261; // 10:00:00 + 1261 s = 10:21:01
        assert_eq!(inside(to_end, Duration::ZERO).position(), t(10, 21, 1));
        assert!(inside(to_end, Duration::ZERO).delivered_to_end());
        let ended = inside(to_end, END_SILENCE);
        assert!(ended.at_end() && !ended.stalled());
        assert!(inside(to_end, Duration::from_secs(5)).at_end());
        // Far from the end it is still a stall.
        let stall = inside(600, END_SILENCE);
        assert!(stall.stalled() && !stall.at_end());
        // Opened at the file's start, the estimate is exact: 4 s short of
        // the end is not the end.
        let from_start = status(ReplaySpeed::X1, 3_594, 400, END_SILENCE);
        assert!(!from_start.delivered_to_end() && from_start.stalled());
        // With key-frame times the position is exact, and the tolerance
        // is not widened.
        let progress = ReplayProgress::new(&nvr_file(), ReplayResume::at(civil_utc(21, 57, 17)), ReplaySpeed::X1, 1, Some(nvr_info()));
        feed_gop(&progress, KEY_19_US, Some(unix(18, 57, 19)), KEY_19_US + 12_050_000);
        let s = progress.status();
        assert_eq!(s.position(), civil_utc(21, 57, 31));
        assert!(!s.delivered_to_end(), "4 s short of 21:57:35");
        feed_gop(&progress, KEY_19_US + 12_100_000, Some(unix(18, 57, 31)), KEY_19_US + 14_050_000);
        assert_eq!(progress.status().position(), civil_utc(21, 57, 33));
        assert!(progress.status().delivered_to_end());
    }

    #[test]
    fn the_resume_point_is_the_latest_key_frames_own_time() {
        let progress = ReplayProgress::new(&nvr_file(), ReplayResume::at(civil_utc(21, 57, 17)), ReplaySpeed::X1, 1, Some(nvr_info()));
        // Nothing received yet: where it was opened.
        assert_eq!(progress.status().resume_point(), ReplayResume::at(civil_utc(21, 57, 17)));
        feed_gop(&progress, KEY_19_US, Some(unix(18, 57, 19)), KEY_23_US);
        // By the footage clock the next key frame lies at :22.85, and its
        // header says :22 (truncated): the anchor keeps :22.85, while the
        // resume point is the key frame's own time, :22.
        progress.on_keyframe(KEY_23_US, Some(unix(18, 57, 22)));
        progress.on_frame(KEY_23_US, true);
        progress.on_frame(KEY_23_US + 100_000, false);
        let s = progress.status();
        assert_eq!(s.anchor.unwrap().unix_micros, i64::from(unix(18, 57, 22)) * 1_000_000 + 848_262);
        assert_eq!(s.position(), civil_utc(21, 57, 22));
        assert_eq!(
            s.resume_point(),
            ReplayResume { seek_time: civil_utc(21, 57, 22), after_microseconds: Some(KEY_23_US + 100_000) }
        );

        // Without key-frame times: an estimate of the key frame's time that
        // is at or before it (seek plus 3.8 s of footage, less a second).
        let progress = ReplayProgress::new(&nvr_file(), ReplayResume::at(civil_utc(21, 57, 17)), ReplaySpeed::X1, 1, None);
        feed_gop(&progress, KEY_19_US, None, KEY_23_US);
        feed_gop(&progress, KEY_23_US, None, KEY_23_US + 300_000);
        let resume = progress.status().resume_point();
        assert_eq!(resume, ReplayResume { seek_time: civil_utc(21, 57, 19), after_microseconds: Some(KEY_23_US + 200_000) });
        assert!(resume.seek_time <= civil_utc(21, 57, 23));
    }

    #[test]
    fn local_micros_agree_with_the_position_and_keep_the_sub_second_part() {
        let progress = ReplayProgress::new(&nvr_file(), ReplayResume::at(civil_utc(21, 57, 17)), ReplaySpeed::X1, 1, Some(nvr_info()));
        assert_eq!(progress.status().local_micros_of(KEY_19_US), None, "not anchored before a key frame");
        feed_gop(&progress, KEY_19_US, Some(unix(18, 57, 19)), KEY_23_US);
        // The next key frame by the footage clock lies at :22.848262; its
        // header says :22, so the anchor keeps the finer estimate.
        progress.on_keyframe(KEY_23_US, Some(unix(18, 57, 22)));
        progress.on_frame(KEY_23_US, true);
        let s = progress.status();
        let local = |h, m, sec| civil_utc(h, m, sec).to_epoch_seconds() * 1_000_000;
        assert_eq!(s.local_micros_of(KEY_23_US), Some(local(21, 57, 22) + 848_262));
        assert_eq!(s.local_micros_of(KEY_19_US), Some(local(21, 57, 19)));
        // The same second as `position`, for the latest and an earlier frame.
        for us in [KEY_23_US, KEY_23_US - 100_000, KEY_19_US + 500_000] {
            let micros = s.local_micros_of(us).unwrap();
            assert_eq!(Some(CivilTime::from_epoch_seconds(micros.div_euclid(1_000_000))), s.position_of(us));
        }
        assert_eq!(CivilTime::from_epoch_seconds(s.local_micros_of(KEY_23_US).unwrap() / 1_000_000), s.position());
        // Out of the anchor's reach, or without the info block's zone: none.
        assert_eq!(s.local_micros_of(KEY_23_US.wrapping_add(MAX_ANCHOR_REACH_MICROS + 1)), None);
        assert_eq!(ReplayStatus { info: None, ..s }.local_micros_of(KEY_23_US), None);
    }

    #[test]
    fn a_resumed_opening_that_only_repeated_so_far_resumes_the_same_way() {
        let from = ReplayResume { seek_time: civil_utc(21, 57, 19), after_microseconds: Some(KEY_19_US + 700_000) };
        let progress = ReplayProgress::new(&nvr_file(), from, ReplaySpeed::X1, 1, None);
        // Its key frame came, as a repeat (not handed on).
        progress.on_keyframe(KEY_19_US, None);
        let s = progress.status();
        assert_eq!((s.frames, s.skip_through_microseconds), (0, Some(KEY_19_US + 700_000)));
        assert_eq!(s.resume_point(), from);
    }

    #[test]
    fn a_resumed_opening_drops_what_it_repeats() {
        let seq = 1_792_436_385;
        let after = u32::MAX - 150_000; // the threshold just before the wrap
        let mut reader = ReplayReader::new(seq, 20, Some(after), None);
        let audio = aac_unit;
        let mut kept = Vec::new();
        for unit in [
            iframe_unit(after.wrapping_sub(2_000_000), &[1], true),
            audio(&[0xff, 1]),
            iframe_unit(after.wrapping_sub(100_000), &[2], false),
            iframe_unit(after, &[3], false),
            audio(&[0xff, 2]),
            iframe_unit(after.wrapping_add(100_000), &[4], false),
            audio(&[0xff, 3]),
            // Once past the threshold nothing more is dropped.
            iframe_unit(after.wrapping_add(200_000), &[5], false),
        ] {
            for (unit, _) in reader.feed(replay_data(seq, unit)) {
                if !reader.is_repeat(&unit) {
                    kept.push(unit);
                }
            }
        }
        assert!(matches!(kept.as_slice(), [
            BcMediaMessage::Video(a), BcMediaMessage::Audio(sound), BcMediaMessage::Video(b)
        ] if a.microseconds == after.wrapping_add(100_000) && sound == &vec![0xff, 3] && b.microseconds == 49_999));
        // Without a threshold no video frame is a repeat, and audio is handed
        // on from the first video frame.
        let mut plain = ReplayReader::new(seq, 20, None, None);
        assert!(!plain.is_repeat(&BcMediaMessage::Video(frame_at(1))));
        assert!(!plain.is_repeat(&BcMediaMessage::Audio(vec![1])));
    }

    #[test]
    fn audio_ahead_of_an_openings_first_video_frame_is_dropped() {
        // An opening in the middle of a file: a burst of audio, then the
        // first key frame and the audio that belongs to the picture.
        let mut reader = ReplayReader::new(7, 20, None, None);
        let dropped: Vec<bool> = [
            BcMediaMessage::Audio(vec![1]),
            BcMediaMessage::Audio(vec![2]),
            BcMediaMessage::Info { width: 640, height: 360 },
            BcMediaMessage::Audio(vec![3]),
            BcMediaMessage::Video(frame_at(1_000_000)),
            BcMediaMessage::Audio(vec![4]),
            BcMediaMessage::Video(frame_at(1_100_000)),
            BcMediaMessage::Audio(vec![5]),
        ]
        .iter()
        .map(|unit| reader.is_repeat(unit))
        .collect();
        assert_eq!(dropped, [true, true, false, true, false, false, false, false]);
        // The same when the opening resumes an earlier one: audio before the
        // first new video frame is a repeat, audio before the first video
        // frame at all is the burst.
        let mut resumed = ReplayReader::new(7, 20, Some(500_000), None);
        assert!(resumed.is_repeat(&BcMediaMessage::Audio(vec![1])));
        assert!(resumed.is_repeat(&BcMediaMessage::Video(frame_at(400_000))));
        assert!(resumed.is_repeat(&BcMediaMessage::Audio(vec![2])));
        assert!(!resumed.is_repeat(&BcMediaMessage::Video(frame_at(600_000))));
        assert!(!resumed.is_repeat(&BcMediaMessage::Audio(vec![3])));
    }

    fn frame_at(microseconds: u32) -> VideoFrame {
        VideoFrame { video_type: VideoType::H264, is_keyframe: true, microseconds, data: vec![0, 0, 0, 1, 0x65] }
    }

    #[test]
    fn speeds_map_to_the_play_speed_tag() {
        let tags: Vec<Option<u32>> = ReplaySpeed::ALL.iter().map(|s| s.play_speed_tag()).collect();
        assert_eq!(tags, vec![None, None, Some(1), Some(2), Some(4), Some(8), Some(16)]);
        assert_eq!(ReplaySpeed::Quarter.device_factor(), 1.0);
        assert_eq!(ReplaySpeed::X8.device_factor(), 8.0);
        assert!(ReplaySpeed::X1.plays_audio());
        assert!(!ReplaySpeed::X2.plays_audio() && !ReplaySpeed::Half.plays_audio());
    }

    // ---- wire helpers for the fake device ------------------------------------

    fn iframe_unit(microseconds: u32, data: &[u8], key: bool) -> Vec<u8> {
        let magic: u32 = if key { 0x63643030 } else { 0x63643130 };
        let mut unit = Vec::new();
        unit.extend_from_slice(&magic.to_le_bytes());
        unit.extend_from_slice(b"H264");
        unit.extend_from_slice(&(data.len() as u32).to_le_bytes());
        unit.extend_from_slice(&0u32.to_le_bytes());
        unit.extend_from_slice(&microseconds.to_le_bytes());
        unit.extend_from_slice(&0u32.to_le_bytes());
        unit.extend_from_slice(data);
        unit.extend(std::iter::repeat(0u8).take((8 - data.len() % 8) % 8));
        unit
    }

    /// A key frame whose 8-byte extra header starts with `unix` (as an NVR
    /// sends them).
    fn keyframe_with_time(microseconds: u32, unix: u32, data: &[u8]) -> Vec<u8> {
        let mut unit = iframe_unit(microseconds, data, true);
        unit[12..16].copy_from_slice(&8u32.to_le_bytes());
        let mut extra = unix.to_le_bytes().to_vec();
        extra.extend_from_slice(&[0x15, 0x01, 0xc3, 0x00]);
        unit.splice(24..24, extra);
        unit
    }

    /// The next unit of a stream as a video frame (panics on anything else).
    fn video(item: Option<crate::Result<ReplayUnit>>) -> VideoFrame {
        match item.expect("a unit").expect("no error") {
            ReplayUnit::Video(frame) => frame,
            ReplayUnit::Audio(_) => panic!("a video frame was expected, not audio"),
        }
    }

    fn aac_unit(bytes: &[u8]) -> Vec<u8> {
        let mut unit = 0x62773530u32.to_le_bytes().to_vec();
        unit.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        unit.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        unit.extend_from_slice(bytes);
        unit.resize(unit.len() + (8 - bytes.len() % 8) % 8, 0);
        unit
    }

    fn info_block() -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(b"1001");
        info.extend_from_slice(&32u32.to_le_bytes());
        info.extend_from_slice(&640u32.to_le_bytes());
        info.extend_from_slice(&360u32.to_le_bytes());
        info.extend_from_slice(&[0x01, 0x0a]);
        info.extend_from_slice(&[0x7e, 0x0a, 0x11, 0x06, 0x15, 0x07]);
        info.extend_from_slice(&[0x7e, 0x0a, 0x11, 0x07, 0x15, 0x05]);
        info.extend_from_slice(&[0, 0]);
        info
    }

    fn binary_extension() -> Vec<u8> {
        Extension { version: XML_VERSION.to_string(), binary_data: Some(1), ..Default::default() }.to_bytes()
    }

    fn device_message(msg_id: u32, msg_num: u16, class: u16, code: u16, extension: Option<Vec<u8>>, payload: Option<Vec<u8>>) -> Bc {
        Bc {
            meta: BcMeta { msg_id, channel_id: 0, stream_type: 0, msg_num, response_code: code, class },
            body: BcBody::Modern(ModernMsg { extension_xml: extension, payload }),
        }
    }

    fn reply(msg_id: u32, msg_num: u16, payload: Option<Vec<u8>>) -> Bc {
        device_message(msg_id, msg_num, 0x0000, 200, None, payload)
    }

    fn replay_data(seq: u32, unit: Vec<u8>) -> Bc {
        device_message(MSG_ID_REPLAY, 0, (seq >> 16) as u16, seq as u16, Some(binary_extension()), Some(unit))
    }

    fn body_of(bc: &Bc) -> Vec<u8> {
        payload_of(bc).expect("a request body").to_vec()
    }

    fn assert_no_extension(bc: &Bc) {
        let BcBody::Modern(ModernMsg { extension_xml, .. }) = &bc.body else { panic!("not a modern message") };
        assert!(extension_xml.is_none(), "message {} must carry no Extension", bc.meta.msg_id);
    }

    /// A client and the device end of a loopback BCUDP connection, both
    /// unencrypted (no login).
    async fn client_and_device() -> (ReolinkClient, BcConnection) {
        let client_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let device_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let client_addr = client_socket.local_addr().unwrap();
        let device_addr = device_socket.local_addr().unwrap();
        let device = BcConnection::new(
            device_socket,
            PeerHandle { addr: client_addr, local_connection_id: 2, remote_connection_id: 1, nonce: None, is_direct: false },
        );
        let client = ReolinkClient::from_connection(
            BcConnection::new(
                client_socket,
                PeerHandle { addr: device_addr, local_connection_id: 1, remote_connection_id: 2, nonce: None, is_direct: false },
            ),
            EncryptionProtocol::Unencrypted,
        );
        (client, device)
    }

    const PLAIN: EncryptionProtocol = EncryptionProtocol::Unencrypted;

    fn page_xml(entries: std::ops::Range<u32>) -> Vec<u8> {
        let day = CivilTime::start_of_day(2026, 10, 17);
        let files = entries
            .rev() // out of order on purpose
            .map(|i| {
                let start = day.add_seconds(i64::from(i) * 600);
                FileInfo {
                    channel_id: Some(2),
                    handle: Some(0),
                    name: Some(format!(
                        "03{:04}{:02}{:02}{:02}{:02}{:02}",
                        start.year, start.month, start.day, start.hour, start.minute, start.second
                    )),
                    id: Some(RecordingId::new(format!("1-2-0-01-{i:05}"))),
                    stream_type: Some("subStream".to_string()),
                    contains_audio: Some(1),
                    record_type: Some("none".to_string()),
                    size_l: Some(524_288),
                    size_h: Some(0),
                    support_sub: Some(1),
                    encrypt: Some(0),
                    start_time: Some(start),
                    end_time: Some(start.add_seconds(600)),
                    file_count: None,
                }
            })
            .collect();
        BcXml { file_info_list: Some(FileInfoList { version: XML_VERSION.to_string(), files }), ..Default::default() }
            .to_bytes()
    }

    #[tokio::test]
    async fn search_day_pages_while_full_and_always_ends_the_search() {
        let (mut client, mut device) = client_and_device().await;
        let fake = tokio::spawn(async move {
            let start = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(start.meta.msg_id, MSG_ID_FILE_SEARCH_START);
            assert_no_extension(&start);
            let request = FileSearchStart::from_bytes(&body_of(&start)).unwrap();
            assert_eq!(request, FileSearchStart::whole_day(2, None, RecordStream::Sub, 2026, 10, 17));
            let handle = FileInfoList {
                version: XML_VERSION.to_string(),
                files: vec![FileInfo { channel_id: Some(2), handle: Some(7), ..Default::default() }],
            };
            let handle_xml = BcXml { file_info_list: Some(handle), ..Default::default() }.to_bytes();
            device.send_bc(&reply(MSG_ID_FILE_SEARCH_START, start.meta.msg_num, Some(handle_xml)), &PLAIN).await.unwrap();

            // 40 + 40 + 36 entries, the last page repeating one file; the
            // second reply does not echo the request's msg_num.
            for (entries, echo) in [(0..40, true), (40..80, false), (79..115, true)] {
                let next = device.recv_bc(&PLAIN).await.unwrap();
                assert_eq!(next.meta.msg_id, MSG_ID_FILE_SEARCH_NEXT);
                assert_no_extension(&next);
                assert_eq!(FileSearchPage::from_bytes(&body_of(&next)).unwrap(), FileSearchPage::new(2, None, 7));
                let msg_num = if echo { next.meta.msg_num } else { 0 };
                device.send_bc(&reply(MSG_ID_FILE_SEARCH_NEXT, msg_num, Some(page_xml(entries))), &PLAIN).await.unwrap();
            }
            let end = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(end.meta.msg_id, MSG_ID_FILE_SEARCH_END);
            assert_no_extension(&end);
            assert_eq!(FileSearchPage::from_bytes(&body_of(&end)).unwrap(), FileSearchPage::new(2, None, 7));
            device.send_bc(&reply(MSG_ID_FILE_SEARCH_END, end.meta.msg_num, None), &PLAIN).await.unwrap();
        });

        let files = client.search_day(2, CivilTime::new(2026, 10, 17, 13, 5, 0), RecordStream::Sub).await.unwrap();
        fake.await.unwrap();
        assert_eq!(files.len(), 115);
        assert!(files.windows(2).all(|w| w[0].start < w[1].start));
        assert_eq!(files[0].name, "0320261017000000");
        assert_eq!(files[0].channel_id, 2);
        assert_eq!(files[0].support_sub, Some(1));
        assert_eq!(files[114].start, CivilTime::new(2026, 10, 17, 19, 0, 0));
    }

    #[tokio::test]
    async fn search_month_lists_the_recorded_days() {
        let (mut client, mut device) = client_and_device().await;
        let fake = tokio::spawn(async move {
            let request = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(request.meta.msg_id, MSG_ID_RECORD_CALENDAR);
            assert_no_extension(&request);
            let mut calendar = BcXml::from_bytes(&body_of(&request)).unwrap().day_records.unwrap();
            assert_eq!(calendar, DayRecords::month(1, None, 2026, 10));
            calendar.day_record_list.records[0].day_type_list = Some(DayTypeList {
                days: [17u32, 9, 10]
                    .into_iter()
                    .map(|index| DayType { index, kind: "normal".to_string() })
                    .collect(),
            });
            let xml = BcXml { day_records: Some(calendar), ..Default::default() }.to_bytes();
            device.send_bc(&reply(MSG_ID_RECORD_CALENDAR, request.meta.msg_num, Some(xml)), &PLAIN).await.unwrap();
        });
        assert_eq!(client.search_month(1, 2026, 10).await.unwrap(), vec![10, 11, 18]);
        fake.await.unwrap();
    }

    #[test]
    fn a_calendar_naming_another_channel_number_is_still_read() {
        let mut calendar = DayRecords::month(0, None, 2026, 9);
        calendar.day_record_list.records[0].day_type_list =
            Some(DayTypeList { days: vec![DayType { index: 1, kind: "normal".to_string() }] });
        assert_eq!(month_days(&calendar, 5, 2026, 9), vec![2]);
        calendar.day_record_list.records[0].day_type_list = Some(DayTypeList::default());
        assert!(month_days(&calendar, 0, 2026, 9).is_empty());
    }

    fn channel_list_push() -> Bc {
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<ChannelInfoList version=\"1.1\">\n<ChannelInfo>\n<channelId>0</channelId>\n<state>connect</state>\n<uid>{UID}</uid>\n<streamSupport>mainStream,subStream</streamSupport>\n</ChannelInfo>\n</ChannelInfoList>\n</body>\n"
        );
        reply(MSG_ID_CHANNEL_INFO, 0, Some(xml.into_bytes()))
    }

    #[tokio::test]
    async fn start_replay_seeks_opens_plays_its_own_data_and_closes() {
        let (mut client, mut device) = client_and_device().await;
        let mut updates = client.take_channel_updates().unwrap();
        let file = RecordedFileFixture::hub();
        let seek_at = file.start.add_seconds(60);
        let expected_file = file.clone();

        let fake = tokio::spawn(async move {
            let file = expected_file;
            // Seek: channelId 0, our seek time, a usable sequence number.
            let seek = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(seek.meta.msg_id, MSG_ID_REPLAY_SEEK);
            assert_no_extension(&seek);
            let body = BcXml::from_bytes(&body_of(&seek)).unwrap().replay_seek.unwrap();
            assert_eq!((body.channel_id, body.seek_time), (0, seek_at));
            let seq = body.seq;
            assert!(is_usable_replay_seq(seq));
            // The channel list (with the camera UID) arrives before the answer.
            device.send_bc(&channel_list_push(), &PLAIN).await.unwrap();
            device.send_bc(&reply(MSG_ID_REPLAY_SEEK, seek.meta.msg_num, None), &PLAIN).await.unwrap();

            // Open by Id, with the UID, at 1x on the sub stream.
            let open = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(open.meta.msg_id, MSG_ID_REPLAY);
            assert_no_extension(&open);
            let request = ReplayOpen::from_bytes(&body_of(&open)).unwrap();
            assert_eq!(request, ReplayOpen::new(0, file.id.clone(), Some(CameraUid::new(UID)), RecordStream::Sub, Some(1)));
            // Data of an earlier playback still trailing in, then the info
            // block, then this playback's data (under msg_num 0).
            device.send_bc(&replay_data(seq - 1, iframe_unit(5, &[7, 7, 7], true)), &PLAIN).await.unwrap();
            let info = device_message(MSG_ID_REPLAY, open.meta.msg_num, 0x0000, 200, Some(binary_extension()), Some(info_block()));
            device.send_bc(&info, &PLAIN).await.unwrap();
            device.send_bc(&replay_data(seq, iframe_unit(1_000_000, &[0, 0, 0, 1, 0x67], true)), &PLAIN).await.unwrap();
            device.send_bc(&replay_data(seq - 1, iframe_unit(6, &[7, 7], false)), &PLAIN).await.unwrap();
            device.send_bc(&replay_data(seq, iframe_unit(1_100_000, &[9, 9], false)), &PLAIN).await.unwrap();

            // Close: channel, name and stream only.
            let close = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(close.meta.msg_id, MSG_ID_REPLAY_CLOSE);
            assert_no_extension(&close);
            let close_body = body_of(&close);
            let text = String::from_utf8(close_body.clone()).unwrap();
            assert!(!text.contains("<uid>") && !text.contains("<Id>"), "{text}");
            assert_eq!(ReplayClose::from_bytes(&close_body).unwrap(), ReplayClose::new(0, file.name.clone(), RecordStream::Sub));
            assert_eq!(close_body.len(), 213);
            // A tail of the closed playback, then the answer.
            device.send_bc(&replay_data(seq, iframe_unit(1_200_000, &[1], false)), &PLAIN).await.unwrap();
            device.send_bc(&reply(MSG_ID_REPLAY_CLOSE, close.meta.msg_num, None), &PLAIN).await.unwrap();
            seq
        });

        let mut stream = client.start_replay(&file, seek_at, ReplaySpeed::X1).await.unwrap();
        let info = stream.info.expect("the info block");
        assert_eq!((info.width, info.height), (640, 360));
        assert_eq!(info.start_utc, Some(CivilTime::new(2026, 10, 17, 6, 21, 7)));
        let first = video(stream.units.next().await);
        assert_eq!((first.microseconds, first.is_keyframe, first.data.as_slice()), (1_000_000, true, &[0, 0, 0, 1, 0x67][..]));
        let second = video(stream.units.next().await);
        assert_eq!((second.microseconds, second.is_keyframe), (1_100_000, false));
        let status = stream.progress.status();
        assert_eq!((status.frames, status.footage_micros, status.position()), (2, 100_000, seek_at));
        assert_eq!(status.seq, stream.seq);

        // The channel list was passed on, and its UID kept for the requests.
        let Some(DeviceUpdate::Channels(channels)) = updates.recv().await else { panic!("expected the channel list") };
        assert_eq!(channels[0].uid.as_ref().map(CameraUid::as_str), Some(UID));
        assert!(!format!("{channels:?}").contains(UID));
        assert_eq!(client.channel_uid(0).map(|u| u.as_str().to_string()), Some(UID.to_string()));

        // A search is refused while the playback runs.
        assert!(client.search_month(0, 2026, 10).await.is_err());

        client.stop_replay().await.unwrap();
        assert!(client.replay_progress().is_none());
        let seq = fake.await.unwrap();
        assert_eq!(seq, stream.seq);
        assert!(stream.progress.status().ended);
    }

    #[tokio::test]
    async fn a_refused_open_is_an_error_and_closes_the_playback() {
        let (mut client, mut device) = client_and_device().await;
        let file = RecordedFileFixture::hub();
        let fake = tokio::spawn(async move {
            let seek = device.recv_bc(&PLAIN).await.unwrap();
            device.send_bc(&reply(MSG_ID_REPLAY_SEEK, seek.meta.msg_num, None), &PLAIN).await.unwrap();
            let open = device.recv_bc(&PLAIN).await.unwrap();
            let refusal = device_message(MSG_ID_REPLAY, open.meta.msg_num, 0x0000, 431, None, None);
            device.send_bc(&refusal, &PLAIN).await.unwrap();
            let close = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(close.meta.msg_id, MSG_ID_REPLAY_CLOSE);
        });
        let error = client.start_replay(&file, file.start, ReplaySpeed::X16).await.err().expect("refused");
        assert!(error.to_string().contains("431"), "{error}");
        fake.await.unwrap();
        assert!(client.replay_progress().is_none());
    }

    /// The device side of an open: answers the seek, then sends `before`,
    /// then the info block. Returns the sequence number.
    async fn answer_open(device: &mut BcConnection, before: Vec<Bc>) -> u32 {
        let seek = device.recv_bc(&PLAIN).await.unwrap();
        assert_eq!(seek.meta.msg_id, MSG_ID_REPLAY_SEEK);
        let seq = BcXml::from_bytes(&body_of(&seek)).unwrap().replay_seek.unwrap().seq;
        device.send_bc(&reply(MSG_ID_REPLAY_SEEK, seek.meta.msg_num, None), &PLAIN).await.unwrap();
        let open = device.recv_bc(&PLAIN).await.unwrap();
        assert_eq!(open.meta.msg_id, MSG_ID_REPLAY);
        for bc in before {
            device.send_bc(&bc, &PLAIN).await.unwrap();
        }
        let info = device_message(MSG_ID_REPLAY, open.meta.msg_num, 0x0000, 200, Some(binary_extension()), Some(info_block()));
        device.send_bc(&info, &PLAIN).await.unwrap();
        seq
    }

    #[tokio::test]
    async fn only_the_info_block_counts_as_the_open_answer() {
        let (mut client, mut device) = client_and_device().await;
        let file = RecordedFileFixture::hub();
        let fake = tokio::spawn(async move {
            // Two code-200 messages without a sequence number that are not
            // the info block: header only, and a payload of another shape.
            let header_only = device_message(MSG_ID_REPLAY, 0, 0x0000, 200, None, None);
            let other = device_message(MSG_ID_REPLAY, 0, 0x0000, 200, Some(binary_extension()), Some(vec![0x55; 12]));
            let seq = answer_open(&mut device, vec![header_only, other]).await;
            device.send_bc(&replay_data(seq, iframe_unit(1_000_000, &[1, 2], true)), &PLAIN).await.unwrap();
            let close = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(close.meta.msg_id, MSG_ID_REPLAY_CLOSE);
            device.send_bc(&reply(MSG_ID_REPLAY_CLOSE, close.meta.msg_num, None), &PLAIN).await.unwrap();
        });
        let mut stream = client.start_replay(&file, file.start, ReplaySpeed::X1).await.unwrap();
        let info = stream.info.expect("the info block, not the earlier code-200 messages");
        assert_eq!((info.width, info.height), (640, 360));
        let first = video(stream.units.next().await);
        assert_eq!(first.microseconds, 1_000_000);
        client.stop_replay().await.unwrap();
        fake.await.unwrap();
    }

    #[tokio::test]
    async fn a_resumed_playback_seeks_to_the_key_frame_and_hands_on_only_what_is_new() {
        let (mut client, mut device) = client_and_device().await;
        let mut audio = client.take_audio().unwrap();
        // 09:21:07..10:21:05 device-local; the info block says 06:21:07 UTC.
        let file = RecordedFileFixture::hub();
        let key_time = CivilTime::new(2026, 10, 17, 6, 22, 7).to_epoch_seconds() as u32;
        let from = ReplayResume { seek_time: t(9, 22, 7), after_microseconds: Some(1_200_000) };
        let fake = tokio::spawn(async move {
            let seek = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(seek.meta.msg_id, MSG_ID_REPLAY_SEEK);
            let body = BcXml::from_bytes(&body_of(&seek)).unwrap().replay_seek.unwrap();
            assert_eq!((body.channel_id, body.seek_time), (0, t(9, 22, 7)));
            let seq = body.seq;
            device.send_bc(&reply(MSG_ID_REPLAY_SEEK, seek.meta.msg_num, None), &PLAIN).await.unwrap();
            let open = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(open.meta.msg_id, MSG_ID_REPLAY);
            // Data of the closed playback trail in around the open answer.
            device.send_bc(&replay_data(seq - 1, iframe_unit(9, &[9], false)), &PLAIN).await.unwrap();
            let info = device_message(MSG_ID_REPLAY, open.meta.msg_num, 0x0000, 200, Some(binary_extension()), Some(info_block()));
            device.send_bc(&info, &PLAIN).await.unwrap();
            device.send_bc(&replay_data(seq - 1, iframe_unit(9, &[9], false)), &PLAIN).await.unwrap();
            // The key frame at the seek time and what follows: repeats up
            // to 1.2 s, then new footage.
            for unit in [
                keyframe_with_time(1_000_000, key_time, &[1]),
                aac_unit(&[0xff, 1]),
                iframe_unit(1_100_000, &[2], false),
                iframe_unit(1_200_000, &[3], false),
                iframe_unit(1_300_000, &[4], false),
                aac_unit(&[0xff, 2]),
            ] {
                device.send_bc(&replay_data(seq, unit), &PLAIN).await.unwrap();
            }
            let close = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(close.meta.msg_id, MSG_ID_REPLAY_CLOSE);
            device.send_bc(&reply(MSG_ID_REPLAY_CLOSE, close.meta.msg_num, None), &PLAIN).await.unwrap();
        });

        let mut stream = client.resume_replay(&file, from, ReplaySpeed::X1).await.unwrap();
        let first = video(stream.units.next().await);
        assert_eq!((first.microseconds, first.is_keyframe, first.data.as_slice()), (1_300_000, false, &[4][..]));
        // The repeated audio is dropped; the new one follows its frame on
        // the same stream, never on the live audio channel.
        assert_eq!(stream.units.next().await.unwrap().unwrap(), ReplayUnit::Audio(vec![0xff, 2]));
        let status = stream.progress.status();
        assert_eq!((status.frames, status.first_microseconds), (1, Some(1_300_000)));
        assert_eq!(status.skip_through_microseconds, Some(1_200_000));
        assert_eq!(status.last_keyframe, Some(ReplayKeyframe { microseconds: 1_000_000, unix_seconds: Some(key_time) }));
        assert_eq!(status.foreign_messages, 2);
        assert!(status.is_anchored());
        assert_eq!(status.zone_offset_seconds(), Some(10_800));
        assert_eq!(status.position(), t(9, 22, 7));
        assert_eq!(status.resume_point(), ReplayResume { seek_time: t(9, 22, 7), after_microseconds: Some(1_300_000) });
        client.stop_replay().await.unwrap();
        fake.await.unwrap();
        assert!(audio.try_recv().is_err());
    }

    #[tokio::test]
    async fn data_of_a_closed_playback_during_the_seek_wait_are_counted() {
        let (mut client, mut device) = client_and_device().await;
        let file = RecordedFileFixture::hub();
        let fake = tokio::spawn(async move {
            let seek = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(seek.meta.msg_id, MSG_ID_REPLAY_SEEK);
            let seq = BcXml::from_bytes(&body_of(&seek)).unwrap().replay_seek.unwrap().seq;
            // Two messages of the closed playback before the seek's answer...
            device.send_bc(&replay_data(seq - 1, iframe_unit(5, &[7], true)), &PLAIN).await.unwrap();
            device.send_bc(&replay_data(seq - 1, iframe_unit(6, &[7], false)), &PLAIN).await.unwrap();
            device.send_bc(&reply(MSG_ID_REPLAY_SEEK, seek.meta.msg_num, None), &PLAIN).await.unwrap();
            let open = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(open.meta.msg_id, MSG_ID_REPLAY);
            // ...and one more before the open's answer.
            device.send_bc(&replay_data(seq - 1, iframe_unit(7, &[7], false)), &PLAIN).await.unwrap();
            let info = device_message(MSG_ID_REPLAY, open.meta.msg_num, 0x0000, 200, Some(binary_extension()), Some(info_block()));
            device.send_bc(&info, &PLAIN).await.unwrap();
            device.send_bc(&replay_data(seq, iframe_unit(1_000_000, &[1, 2], true)), &PLAIN).await.unwrap();
            let close = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(close.meta.msg_id, MSG_ID_REPLAY_CLOSE);
            device.send_bc(&reply(MSG_ID_REPLAY_CLOSE, close.meta.msg_num, None), &PLAIN).await.unwrap();
        });
        let mut stream = client.start_replay(&file, file.start, ReplaySpeed::X1).await.unwrap();
        let first = video(stream.units.next().await);
        assert_eq!(first.microseconds, 1_000_000);
        assert_eq!(stream.progress.status().foreign_messages, 3, "two during the seek wait, one during the open's");
        client.stop_replay().await.unwrap();
        fake.await.unwrap();
    }

    #[tokio::test]
    async fn a_playback_whose_frames_were_dropped_is_closed_before_a_search() {
        let (mut client, mut device) = client_and_device().await;
        let file = RecordedFileFixture::hub();
        let fake = tokio::spawn(async move {
            answer_open(&mut device, Vec::new()).await;
            // The close comes before the search.
            let close = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(close.meta.msg_id, MSG_ID_REPLAY_CLOSE);
            device.send_bc(&reply(MSG_ID_REPLAY_CLOSE, close.meta.msg_num, None), &PLAIN).await.unwrap();
            let calendar = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(calendar.meta.msg_id, MSG_ID_RECORD_CALENDAR);
            device.send_bc(&reply(MSG_ID_RECORD_CALENDAR, calendar.meta.msg_num, None), &PLAIN).await.unwrap();
        });
        let stream = client.start_replay(&file, file.start, ReplaySpeed::X1).await.unwrap();
        let progress = stream.progress.clone();
        drop(stream);
        // The reader notices within one read slice.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client.replay_task.as_ref().is_some_and(|t| t.is_finished()) {
            assert!(Instant::now() < deadline, "the reader should stop once its frames are dropped");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(progress.status().ended);
        assert_eq!(client.search_month(0, 2026, 10).await.unwrap(), Vec::<u8>::new());
        assert!(client.replay_progress().is_none());
        fake.await.unwrap();
    }

    #[tokio::test]
    async fn live_video_after_a_playback_waits_briefly_for_the_close_and_gets_the_live_bound() {
        let (mut client, mut device) = client_and_device().await;
        let file = RecordedFileFixture::hub();
        let fake = tokio::spawn(async move {
            let seq = answer_open(&mut device, Vec::new()).await;
            // The close is never answered; the playback's data trail in.
            let close = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(close.meta.msg_id, MSG_ID_REPLAY_CLOSE);
            device.send_bc(&replay_data(seq, iframe_unit(2_000_000, &[3], false)), &PLAIN).await.unwrap();
            let preview = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(preview.meta.msg_id, MSG_ID_VIDEO);
            device.send_bc(&reply(MSG_ID_VIDEO, preview.meta.msg_num, None), &PLAIN).await.unwrap();
        });
        let _stream = client.start_replay(&file, file.start, ReplaySpeed::X1).await.unwrap();
        assert!(!client.connection.lock().await.has_live_buffering());
        let started = Instant::now();
        let _live = client.start_video(0, StreamProfile::Sub).await.unwrap();
        let waited = started.elapsed();
        assert!(waited >= LIVE_SWITCH_CLOSE_WAIT && waited < REPLY_TIMEOUT, "waited {waited:?}");
        assert!(client.connection.lock().await.has_live_buffering(), "no replay bound may linger into live video");
        assert!(client.replay_progress().is_none());
        fake.await.unwrap();
    }

    #[tokio::test]
    async fn an_unanswered_event_search_is_given_up_within_its_own_wait_and_not_asked_again() {
        let (mut client, mut device) = client_and_device().await;
        let fake = tokio::spawn(async move {
            let start = device.recv_bc(&PLAIN).await.unwrap();
            assert_eq!(start.meta.msg_id, MSG_ID_EVENT_SEARCH_START);
            // Never answered. Nothing more may be asked afterwards.
            let more = tokio::time::timeout(Duration::from_millis(500), device.recv_bc(&PLAIN)).await;
            assert!(more.is_err(), "a second event search was sent: {more:?}");
        });
        let started = Instant::now();
        let first = client.search_events(0, t(0, 0, 0), RecordStream::Sub).await;
        let waited = started.elapsed();
        assert!(matches!(first, Err(Error::ReplyTimeout)), "{first:?}");
        assert!(waited >= EVENT_SEARCH_REPLY_WAIT && waited < REPLY_TIMEOUT, "waited {waited:?}");
        let started = Instant::now();
        assert_eq!(client.search_events(0, t(0, 0, 0), RecordStream::Sub).await.unwrap(), Vec::new());
        assert!(started.elapsed() < Duration::from_millis(200), "listed nothing without asking");
        fake.await.unwrap();
    }

    #[tokio::test]
    async fn a_seek_at_the_end_of_the_file_is_refused_without_asking_the_device() {
        let (mut client, _device) = client_and_device().await;
        let file = RecordedFileFixture::hub();
        assert!(client.start_replay(&file, file.end, ReplaySpeed::X1).await.is_err());
        assert!(client.start_replay(&file, file.end.add_seconds(5), ReplaySpeed::X1).await.is_err());
    }

    #[tokio::test]
    async fn searches_are_refused_while_a_stream_reads_the_connection() {
        let (mut client, _device) = client_and_device().await;
        client.video_task = Some(tokio::spawn(std::future::pending()));
        assert!(client.search_day(0, t(0, 0, 0), RecordStream::Sub).await.is_err());
        client.video_task.take().unwrap().abort();
        client.replay_task = Some(tokio::spawn(std::future::pending()));
        assert!(client.search_month(0, 2026, 10).await.is_err());
        client.replay_task.take().unwrap().abort();
    }

    #[test]
    fn the_reader_keeps_its_own_data_whatever_their_msg_num() {
        let seq = 1_792_348_477;
        let mut reader = ReplayReader::new(seq, 42, None, None);
        assert!(!reader.is_ours(&replay_data(seq + 1, iframe_unit(1, &[1], true))));
        assert!(!reader.is_ours(&reply(MSG_ID_REPLAY_CLOSE, 42, None)));
        let ours = replay_data(seq, iframe_unit(1, &[1], true));
        assert!(reader.is_ours(&ours));
        assert_eq!(reader.msg_num_mismatches, 1, "msg_num 0 against 42 is counted, not dropped");
        let units = reader.feed(ours);
        assert!(matches!(units.as_slice(), [(BcMediaMessage::Video(f), None)] if f.microseconds == 1));
        // A unit split over two messages is reassembled.
        let unit = iframe_unit(2, &[5; 40], false);
        let (a, b) = unit.split_at(30);
        assert!(reader.feed(replay_data(seq, a.to_vec())).is_empty());
        let units = reader.feed(replay_data(seq, b.to_vec()));
        assert!(matches!(units.as_slice(), [(BcMediaMessage::Video(f), None)] if f.data == vec![5; 40]));
    }

    #[test]
    fn the_probe_opens_a_finished_file_and_looks_back_one_month() {
        let a = file("a", t(8, 0, 0), t(9, 0, 0));
        let b = file("b", t(9, 0, 0), t(10, 0, 0));
        assert_eq!(probe_pick(&[]), None);
        assert_eq!(probe_pick(std::slice::from_ref(&a)), Some(&a));
        assert_eq!(probe_pick(&[a.clone(), b]), Some(&a), "the newest is never opened when there is another");
        // Short files before the newest are passed over for a long one...
        let short = file("s", t(10, 0, 0), t(10, 0, 13));
        let newest = file("n", t(10, 1, 0), t(10, 1, 30));
        assert_eq!(probe_pick(&[a.clone(), short.clone(), newest.clone()]), Some(&a));
        // ...unless there is none.
        assert_eq!(probe_pick(&[short.clone(), newest]), Some(&short));
        assert_eq!(current_and_previous_month(CivilTime::new(2026, 1, 5, 0, 0, 0)), [(2026, 1), (2025, 12)]);
        assert_eq!(current_and_previous_month(t(0, 0, 0)), [(2026, 10), (2026, 9)]);
    }

    /// A Home Hub sub-stream file whose `Id` ends with the camera UID.
    struct RecordedFileFixture;

    impl RecordedFileFixture {
        fn hub() -> RecordingFile {
            RecordingFile {
                id: RecordingId::new(format!("1-0-01260917062107-00000-{UID}")),
                ..file("0120261017092107", t(9, 21, 7), t(10, 21, 5))
            }
        }
    }
}
