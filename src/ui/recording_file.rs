//! Recorded footage written to an MP4 file: the units of a download
//! (`DownloadUnit`) go in as they come and the file is finished when the
//! download is. GStreamer only, no GTK.
//!
//! The file's name is reserved (as an empty file) when the download starts,
//! see [`reserve_path`], so that two downloads never share a file; the
//! pipeline is `appsrc ! h264parse/h265parse ! qtmux ! filesink`, with
//! `appsrc ! aacparse` into the same muxer when the footage has sound. It
//! is built at the first key frame, once it is known whether sound comes
//! (see [`AudioExpectation`], `AUDIO_DECISION_FRAMES` and
//! `LATER_AUDIO_PENDING_BUDGET_BYTES`): the muxer's tracks are fixed at
//! that point, and an audio track is only ever made once a unit for it has
//! come, since the muxer writes nothing at all while one of its tracks has
//! yet to receive a buffer. Timestamps come from the camera's footage
//! clock (see [`WriterClock`]).

use gstreamer::prelude::*;
use gstreamer_app::AppSrc;
use reoling::{CivilTime, DownloadUnit, VideoFrame, VideoType};
use std::path::{Path, PathBuf};

/// A footage-time step between consecutive frames of one file above this
/// is not believed (consecutive frames lie 40–100 ms apart on every model
/// seen): the frame is placed one usual step after the one before. The
/// step across a file boundary is never believed, whatever its size: a
/// gap between two files is cut out, not frozen over (see
/// [`WriterClock::at_boundary`]).
const MAX_PLAUSIBLE_STEP_US: u64 = 2_000_000;

/// The step used before any was measured: 25 fps.
const DEFAULT_STEP_US: u64 = 40_000;

/// Audio runs on its units' own durations, one starting where the one
/// before ended; once their count has fallen this far behind the latest
/// frame's time, the next unit starts at that time again (as the
/// playback's sound does). Ahead of the picture it is left to run on, so
/// that its track never goes back in time.
const AUDIO_REANCHOR_US: u64 = 250_000;

/// A file whose first recording is listed with sound gets its audio track
/// once a unit came; without one among this many video frames after the
/// first key frame (about a second of footage) it is written without
/// sound. The devices send one or two units per frame. (When a later
/// recording of the range has sound, the file waits longer, see
/// [`AudioExpectation::later`].)
const AUDIO_DECISION_FRAMES: u64 = 16;

/// How much footage a file waits with for the sound of a later recording
/// (see [`AudioExpectation::later`]): the frames before the first audio
/// unit are held by the writer, in memory, since the muxer cannot be given
/// an audio track before a unit for it has come (it would write nothing
/// until one did) and cannot be given one later. Once the frames held
/// reach this many bytes (`PENDING_UNIT_OVERHEAD` counted per unit) the
/// file is started without sound, and the
/// sound that then comes is left out. A minute of a main stream, several
/// of a sub stream.
const LATER_AUDIO_PENDING_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// What one held unit costs the budget beyond its bytes: its entry and the
/// allocation of its data. Counted so that a flood of tiny units cannot
/// hold more memory than the budget says.
const PENDING_UNIT_OVERHEAD: u64 = (std::mem::size_of::<Stamped>() + 16) as u64;

/// How long finishing waits for the muxer to write the file's end.
const FINISH_WAIT: gstreamer::ClockTime = gstreamer::ClockTime::from_seconds(15);

/// What a source holds before it says it has enough (the sources never
/// block, see `MAX_QUEUE_BUDGET_BYTES` for the hard limit).
const MAX_QUEUED_BYTES: u64 = 64 * 1024 * 1024;

/// The most the sources together may hold waiting for the muxer. The
/// device sends a download as fast as the path carries it, and the
/// sources never block the thread that feeds them (the device's); when
/// the storage cannot keep up, what waits grows at the difference of the
/// two rates. Past this the writer fails rather than hold the rest of the
/// download in memory (a whole recording can run to several GB).
const MAX_QUEUE_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// What the file list says about the sound of a download's range.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AudioExpectation {
    /// The recording the data start in has sound: the file waits for a
    /// first audio unit (see `AUDIO_DECISION_FRAMES`) before it is started.
    pub first: bool,
    /// A later recording of the range has sound: the file waits for a
    /// first audio unit past `AUDIO_DECISION_FRAMES`, holding the frames
    /// that come meanwhile, up to `LATER_AUDIO_PENDING_BUDGET_BYTES` of
    /// them (the muxer's tracks cannot be added to later).
    pub later: bool,
}

/// Timestamps for the file: the first frame at 0, then the frames' own
/// footage-time steps (wrap-safe), an implausible one, or the one across a
/// file boundary, replaced by the last usual one; audio contiguous from
/// its own durations, re-anchored to the picture when it falls behind.
#[derive(Debug, Default, Clone, Copy)]
pub struct WriterClock {
    last_raw: Option<u32>,
    last_step_us: u64,
    /// PTS of the latest video frame.
    video_us: Option<u64>,
    /// Where the next audio unit starts.
    audio_next_us: Option<u64>,
    /// A file boundary lies between the last frame and the next: the
    /// next frame's step is one usual step, however far the footage clock
    /// moved.
    at_boundary: bool,
}

impl WriterClock {
    /// PTS (microseconds) for a video frame with footage time `raw`.
    pub fn stamp_video(&mut self, raw: u32) -> u64 {
        let pts = match (self.last_raw, self.video_us) {
            (Some(last), Some(at)) => {
                // Backwards is a huge step: implausible like a jump ahead.
                let step = raw.wrapping_sub(last) as u64;
                let step = if self.at_boundary || step == 0 || step > MAX_PLAUSIBLE_STEP_US {
                    self.usual_step_us()
                } else {
                    self.last_step_us = step;
                    step
                };
                at + step
            }
            _ => 0,
        };
        self.at_boundary = false;
        self.last_raw = Some(raw);
        self.video_us = Some(pts);
        pts
    }

    /// A file boundary was passed: the next frame follows the last one at
    /// the usual step, whether the files adjoin or a gap lies between them
    /// (the file holds only what was recorded).
    pub fn at_boundary(&mut self) {
        self.at_boundary = true;
    }

    fn usual_step_us(&self) -> u64 {
        if self.last_step_us > 0 {
            self.last_step_us
        } else {
            DEFAULT_STEP_US
        }
    }

    /// PTS (microseconds) for an audio unit `duration_us` long. `None`
    /// before the first video frame: nothing to follow yet.
    pub fn stamp_audio(&mut self, duration_us: u64) -> Option<u64> {
        let video = self.video_us?;
        let at = match self.audio_next_us {
            Some(next) if next + AUDIO_REANCHOR_US >= video => next,
            _ => video,
        };
        self.audio_next_us = Some(at + duration_us);
        Some(at)
    }

    /// PTS of the latest video frame: the footage written so far.
    pub fn duration_us(&self) -> u64 {
        self.video_us.unwrap_or(0)
    }

    /// How long a frame is taken to last: the latest usual step between
    /// frames (the muxer needs a duration for the last frame, which has no
    /// successor to measure it from).
    pub fn frame_duration_us(&self) -> u64 {
        self.usual_step_us()
    }
}

/// Drops what the next file of a range repeats. Adjoining files overlap by
/// a couple of seconds, and a download sends the overlap twice: the last
/// GOP of one file, then the next file from its start, whose first key
/// frame has the same footage time (the camera's clock runs on across the
/// files). From a boundary on, video at or before the last footage time
/// written is a repeat and left out, with its audio, until a frame lies
/// after it; a footage time more than `BOUNDARY_REACH_US` away either way
/// is another clock (nothing is left out then).
#[derive(Debug, Default, Clone, Copy)]
struct BoundarySkip {
    /// The footage time to get past, while skipping.
    through: Option<u32>,
}

/// How far from the boundary's footage time a frame still counts as
/// belonging to the same clock.
const BOUNDARY_REACH_US: u32 = 10_000_000;

impl BoundarySkip {
    /// A file boundary was reached; `last_us` is the footage time of the
    /// last video frame written (`None`: nothing yet, nothing to repeat).
    fn at_boundary(&mut self, last_us: Option<u32>) {
        self.through = last_us;
    }

    /// Whether a video frame with footage time `us` is a repeat.
    fn skips_video(&mut self, us: u32) -> bool {
        let Some(through) = self.through else { return false };
        // Signed, so that a time just past the clock's wrap is ahead; its
        // unsigned magnitude, so that no sign is taken of a jump of
        // exactly half the clock (which `abs` could not represent).
        let ahead = us.wrapping_sub(through) as i32;
        if ahead > 0 || ahead.unsigned_abs() > BOUNDARY_REACH_US {
            self.through = None;
            return false;
        }
        true
    }

    /// Whether audio is being skipped (with the repeated video).
    fn skips_audio(&self) -> bool {
        self.through.is_some()
    }
}

/// Whether the file can be started: the codec is known (a key frame came)
/// and the sound question is settled: a unit came, none is expected, or
/// the wait for one is over. With sound expected from a later recording
/// that wait is over once what is held reaches its budget
/// (`pending_full`); otherwise after `AUDIO_DECISION_FRAMES` frames.
fn ready_to_build(
    keyframe_seen: bool,
    expects: AudioExpectation,
    audio_seen: bool,
    frames_since_keyframe: u64,
    pending_full: bool,
) -> bool {
    if !keyframe_seen {
        return false;
    }
    if audio_seen {
        return true;
    }
    if expects.later {
        return pending_full;
    }
    !expects.first || frames_since_keyframe >= AUDIO_DECISION_FRAMES
}

/// What a writer put into (or left out of) its file.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WriterStats {
    pub video_frames: u64,
    pub keyframes: u64,
    pub audio_units: u64,
    /// Video frames and audio units ahead of the first key frame (nothing
    /// is playable before one).
    pub dropped_before_keyframe: u64,
    /// Audio units that came once the file had been started without sound,
    /// and video frames of another codec than the first key frame's.
    pub dropped_other: u64,
    /// Video frames and audio units a next file of the range repeated
    /// (see `BoundarySkip`).
    pub dropped_repeats: u64,
    /// The footage written: PTS of the last video frame.
    pub duration_us: u64,
}

/// A unit with its timestamp, waiting for the pipeline.
enum Stamped {
    Video { data: Vec<u8>, pts_us: u64, duration_us: u64 },
    Audio { data: Vec<u8>, pts_us: u64, duration_us: u64 },
}

struct Built {
    pipeline: gstreamer::Pipeline,
    video: AppSrc,
    audio: Option<AppSrc>,
}

impl Built {
    /// What the sources hold that the muxer has not taken yet (a counter
    /// of the sources', read without waiting).
    fn queued_bytes(&self) -> u64 {
        self.video.current_level_bytes() + self.audio.as_ref().map_or(0, AppSrc::current_level_bytes)
    }
}

/// Writes a download's units to an MP4 file at `path`, a name reserved
/// for this download (see [`reserve_path`]): the file is empty until the
/// first key frame, from which on the muxer writes into it (see the module
/// doc). [`Mp4Writer::finish`] completes the file, [`Mp4Writer::discard`]
/// removes it, and so does a failure; the writer only ever removes its
/// own file.
pub struct Mp4Writer {
    path: PathBuf,
    expects: AudioExpectation,
    clock: WriterClock,
    built: Option<Built>,
    pending: Vec<Stamped>,
    /// Bytes held in `pending`, `PENDING_UNIT_OVERHEAD` counted per unit.
    pending_bytes: u64,
    /// The most `pending` may hold while waiting for the sound of a later
    /// recording (`LATER_AUDIO_PENDING_BUDGET_BYTES`).
    pending_budget: u64,
    video_type: Option<VideoType>,
    frames_since_keyframe: u64,
    audio_seen: bool,
    boundary: BoundarySkip,
    /// Footage time of the last video frame written.
    last_video_us: Option<u32>,
    stats: WriterStats,
    /// Why the file could not be written (the disk full, the file gone):
    /// the pipeline is stopped and the file removed, nothing more is taken.
    failed: Option<String>,
    /// The most the sources may hold together (`MAX_QUEUE_BUDGET_BYTES`).
    queue_budget: u64,
    /// The pipeline is started paused, so that nothing past the preroll is
    /// taken from the sources: the shape of a destination that has stalled.
    #[cfg(test)]
    start_paused: bool,
}

impl Mp4Writer {
    /// A writer for `path` (reserved with [`reserve_path`]); `expects` says
    /// what the file list promises about sound, which decides whether and
    /// how long the file waits for a first audio unit.
    pub fn new(path: PathBuf, expects: AudioExpectation) -> Self {
        Mp4Writer {
            path,
            expects,
            clock: WriterClock::default(),
            built: None,
            pending: Vec::new(),
            pending_bytes: 0,
            pending_budget: LATER_AUDIO_PENDING_BUDGET_BYTES,
            video_type: None,
            frames_since_keyframe: 0,
            audio_seen: false,
            boundary: BoundarySkip::default(),
            last_video_us: None,
            stats: WriterStats::default(),
            failed: None,
            queue_budget: MAX_QUEUE_BUDGET_BYTES,
            #[cfg(test)]
            start_paused: false,
        }
    }

    #[cfg(test)]
    fn with_queue_budget(mut self, bytes: u64) -> Self {
        self.queue_budget = bytes;
        self
    }

    #[cfg(test)]
    fn with_pending_budget(mut self, bytes: u64) -> Self {
        self.pending_budget = bytes;
        self
    }

    pub fn stats(&self) -> WriterStats {
        self.stats
    }

    /// Takes one unit of the download in (progress and the summary are not
    /// the file's business; an info block after the first marks a file
    /// boundary, see `BoundarySkip`). `Err` when the pipeline could not be
    /// built or the file can no longer be written (the disk full, the
    /// file gone): the file is removed then, the writer takes nothing more
    /// and should be discarded.
    pub fn push(&mut self, unit: &DownloadUnit) -> Result<(), String> {
        if let Some(failed) = &self.failed {
            return Err(failed.clone());
        }
        match unit {
            DownloadUnit::Info(_) if self.last_video_us.is_some() => {
                self.boundary.at_boundary(self.last_video_us);
                self.clock.at_boundary();
                Ok(())
            }
            DownloadUnit::Video(frame) => self.push_video(frame),
            DownloadUnit::Audio(aac) => self.push_audio(aac),
            _ => Ok(()),
        }
    }

    /// `pushed` (a buffer handed to the pipeline) and the pipeline's bus
    /// looked at: the writer is given up on an error from either, with the
    /// file removed, and the error returned.
    fn check(&mut self, pushed: Result<(), String>) -> Result<(), String> {
        let error = pushed.err().or_else(|| self.built.as_ref().and_then(|b| posted_error(&b.pipeline)));
        match error {
            Some(error) => {
                self.give_up(&error);
                Err(error)
            }
            None => Ok(()),
        }
    }

    /// Stops the pipeline and removes the file: `why` is what every call
    /// from now on returns.
    fn give_up(&mut self, why: &str) {
        if let Some(built) = self.built.take() {
            let _ = built.pipeline.set_state(gstreamer::State::Null);
        }
        self.pending.clear();
        self.pending_bytes = 0;
        remove_quietly(&self.path);
        self.failed = Some(why.to_string());
    }

    /// Whether `len` more bytes fit the sources' budget: `Err` (with how
    /// much waits) when the storage has fallen that far behind the
    /// download.
    fn room_for(&self, built: &Built, len: usize) -> Result<(), String> {
        let queued = built.queued_bytes();
        if queued + len as u64 > self.queue_budget {
            return Err(format!(
                "writing the file failed: the storage cannot keep up with the download ({} waiting)",
                size_text(queued)
            ));
        }
        Ok(())
    }

    fn push_video(&mut self, frame: &VideoFrame) -> Result<(), String> {
        if frame.data.is_empty() {
            // No frame at all: nothing for the muxer, and neither to be
            // held nor counted as the first key frame.
            self.stats.dropped_other += 1;
            return Ok(());
        }
        if self.boundary.skips_video(frame.microseconds) {
            self.stats.dropped_repeats += 1;
            return Ok(());
        }
        match self.video_type {
            None if frame.is_keyframe => self.video_type = Some(frame.video_type),
            None => {
                self.stats.dropped_before_keyframe += 1;
                return Ok(());
            }
            Some(codec) if codec != frame.video_type => {
                self.stats.dropped_other += 1;
                return Ok(());
            }
            Some(_) => {}
        }
        self.frames_since_keyframe += 1;
        self.stats.video_frames += 1;
        self.stats.keyframes += u64::from(frame.is_keyframe);
        self.last_video_us = Some(frame.microseconds);
        let pts_us = self.clock.stamp_video(frame.microseconds);
        let duration_us = self.clock.frame_duration_us();
        self.stats.duration_us = self.clock.duration_us();
        match &self.built {
            Some(built) => {
                let pushed = self
                    .room_for(built, frame.data.len())
                    .and_then(|_| push_buffer(&built.video, frame.data.clone(), pts_us, duration_us));
                self.check(pushed)
            }
            None => {
                self.pending_bytes += frame.data.len() as u64 + PENDING_UNIT_OVERHEAD;
                self.pending.push(Stamped::Video { data: frame.data.clone(), pts_us, duration_us });
                self.build_if_ready()
            }
        }
    }

    fn push_audio(&mut self, aac: &[u8]) -> Result<(), String> {
        if self.boundary.skips_audio() {
            self.stats.dropped_repeats += 1;
            return Ok(());
        }
        let duration_us = crate::ui::video_view::adts_unit_duration_us(aac);
        let aac = &adts_as_mpeg4(aac);
        let Some(pts_us) = self.clock.stamp_audio(duration_us) else {
            self.stats.dropped_before_keyframe += 1;
            return Ok(());
        };
        match &self.built {
            Some(built @ Built { audio: Some(audio), .. }) => {
                self.stats.audio_units += 1;
                let pushed =
                    self.room_for(built, aac.len()).and_then(|_| push_buffer(audio, aac.to_vec(), pts_us, duration_us));
                self.check(pushed)
            }
            Some(Built { audio: None, .. }) => {
                self.stats.dropped_other += 1;
                Ok(())
            }
            None => {
                self.audio_seen = true;
                self.pending_bytes += aac.len() as u64 + PENDING_UNIT_OVERHEAD;
                self.pending.push(Stamped::Audio { data: aac.to_vec(), pts_us, duration_us });
                self.build_if_ready()
            }
        }
    }

    /// Starts the file once the sound question is settled (see
    /// `ready_to_build`), with an audio track only when a unit for it
    /// waits in `pending`.
    fn build_if_ready(&mut self) -> Result<(), String> {
        if self.built.is_none()
            && ready_to_build(
                self.video_type.is_some(),
                self.expects,
                self.audio_seen,
                self.frames_since_keyframe,
                self.pending_bytes >= self.pending_budget,
            )
        {
            self.build(self.audio_seen)?;
        }
        Ok(())
    }

    /// The state the pipeline is started in: playing, or paused in tests
    /// that model a stalled destination.
    fn start_state(&self) -> gstreamer::State {
        #[cfg(test)]
        if self.start_paused {
            return gstreamer::State::Paused;
        }
        gstreamer::State::Playing
    }

    /// Builds the pipeline (with an audio track if `with_audio`) and feeds
    /// it what waited. On failure the writer is given up (see `check`).
    fn build(&mut self, with_audio: bool) -> Result<(), String> {
        let codec = self.video_type.ok_or("no key frame yet")?;
        let built = match build_pipeline(&self.path, codec, with_audio, self.start_state()) {
            Ok(built) => built,
            Err(e) => {
                self.give_up(&e);
                return Err(e);
            }
        };
        let mut pushed = Ok(());
        for unit in self.pending.drain(..) {
            let one = match unit {
                Stamped::Video { data, pts_us, duration_us } => push_buffer(&built.video, data, pts_us, duration_us),
                Stamped::Audio { data, pts_us, duration_us } => match &built.audio {
                    Some(audio) => {
                        self.stats.audio_units += 1;
                        push_buffer(audio, data, pts_us, duration_us)
                    }
                    None => {
                        self.stats.dropped_other += 1;
                        Ok(())
                    }
                },
            };
            pushed = pushed.and(one);
        }
        self.pending_bytes = 0;
        self.built = Some(built);
        self.check(pushed)
    }

    /// Completes the file: what still waits is written, the streams are
    /// ended and the muxer's end of the file waited for (it blocks for up
    /// to `FINISH_WAIT`). A file without a single key frame is not written;
    /// one still waiting for sound (see [`AudioExpectation::later`]) is
    /// written without it. On failure the file is removed.
    pub fn finish(mut self) -> Result<WriterStats, String> {
        if let Some(failed) = self.failed.take() {
            remove_quietly(&self.path);
            return Err(failed);
        }
        if self.built.is_none() {
            if self.video_type.is_none() {
                remove_quietly(&self.path);
                return Err("no video frame came".to_string());
            }
            self.build(self.audio_seen)?;
        }
        let built = self.built.take().expect("built above");
        let _ = built.video.end_of_stream();
        if let Some(audio) = &built.audio {
            let _ = audio.end_of_stream();
        }
        let outcome = match built.pipeline.bus() {
            Some(bus) => match bus.timed_pop_filtered(FINISH_WAIT, &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error]) {
                Some(message) => match message.view() {
                    gstreamer::MessageView::Error(e) => Err(error_text(&e)),
                    _ => Ok(()),
                },
                None => Err("the file was not finished in time".to_string()),
            },
            None => Err("the pipeline has no bus".to_string()),
        };
        let _ = built.pipeline.set_state(gstreamer::State::Null);
        match outcome {
            Ok(()) => Ok(self.stats),
            Err(e) => {
                remove_quietly(&self.path);
                Err(e)
            }
        }
    }

    /// Stops writing and removes the file (a cancelled or failed download).
    pub fn discard(mut self) {
        if let Some(built) = self.built.take() {
            let _ = built.pipeline.set_state(gstreamer::State::Null);
        }
        remove_quietly(&self.path);
    }
}

/// `unit` with every ADTS frame header marked MPEG-4: some cameras (a
/// Home Hub Pro's RLC-1224A) set the header's MPEG-2 bit, and the muxer
/// takes no MPEG-2 audio, while the AAC LC frames themselves are the same
/// either way. A unit that does not walk as ADTS frames is left as it is.
fn adts_as_mpeg4(unit: &[u8]) -> Vec<u8> {
    let mut out = unit.to_vec();
    let mut at = 0usize;
    while at < out.len() {
        let Some(header) = out.get(at..at + 7) else { return unit.to_vec() };
        let sync = header[0] == 0xff && header[1] & 0xf6 == 0xf0;
        let length = (usize::from(header[3] & 0x03) << 11) | (usize::from(header[4]) << 3) | usize::from(header[5] >> 5);
        if !sync || length < 7 || at + length > out.len() {
            return unit.to_vec();
        }
        out[at + 1] &= !0x08;
        at += length;
    }
    out
}

fn remove_quietly(path: &Path) {
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
}

/// "writing the file failed: <error> (<element>: <details>)".
fn error_text(e: &gstreamer::message::Error) -> String {
    format!(
        "writing the file failed: {} ({}: {})",
        e.error(),
        e.src().map_or("?".to_string(), |s| s.name().to_string()),
        e.debug().map_or(String::new(), |d| d.to_string()),
    )
}

/// The error the pipeline posted, if one is waiting on its bus (the file
/// could not be opened or written: the cause is only told there).
fn posted_error(pipeline: &gstreamer::Pipeline) -> Option<String> {
    let message = pipeline.bus()?.pop_filtered(&[gstreamer::MessageType::Error])?;
    match message.view() {
        gstreamer::MessageView::Error(e) => Some(error_text(&e)),
        _ => None,
    }
}

/// Hands `data` to `appsrc` with its times; `Err` when the source no
/// longer takes buffers (the pipeline stopped).
fn push_buffer(appsrc: &AppSrc, data: Vec<u8>, pts_us: u64, duration_us: u64) -> Result<(), String> {
    let mut buffer = gstreamer::Buffer::from_mut_slice(data);
    {
        let b = buffer.get_mut().expect("a new buffer");
        let at = gstreamer::ClockTime::from_useconds(pts_us);
        b.set_pts(at);
        b.set_dts(at);
        b.set_duration(gstreamer::ClockTime::from_useconds(duration_us));
    }
    match appsrc.push_buffer(buffer) {
        Ok(_) => Ok(()),
        Err(flow) => Err(format!("writing the file failed: the pipeline takes no more data ({flow:?})")),
    }
}

fn make(name: &str) -> Result<gstreamer::Element, String> {
    gstreamer::ElementFactory::make(name).build().map_err(|e| format!("{name} is missing: {e}"))
}

fn source(caps: gstreamer::Caps) -> Result<AppSrc, String> {
    let appsrc = make("appsrc")?.downcast::<AppSrc>().map_err(|_| "appsrc is not an AppSrc".to_string())?;
    appsrc.set_caps(Some(&caps));
    appsrc.set_format(gstreamer::Format::Time);
    appsrc.set_is_live(false);
    appsrc.set_block(false);
    appsrc.set_max_bytes(MAX_QUEUED_BYTES);
    Ok(appsrc)
}

fn build_pipeline(path: &Path, codec: VideoType, with_audio: bool, state: gstreamer::State) -> Result<Built, String> {
    let (parse, media) = match codec {
        VideoType::H264 => ("h264parse", "video/x-h264"),
        VideoType::H265 => ("h265parse", "video/x-h265"),
    };
    let video = source(gstreamer::Caps::builder(media).field("stream-format", "byte-stream").field("alignment", "au").build())?;
    let parser = make(parse)?;
    let mux = make("qtmux")?;
    let file = gstreamer::ElementFactory::make("filesink")
        .property("location", path.to_string_lossy().to_string())
        .build()
        .map_err(|e| format!("filesink is missing: {e}"))?;
    let pipeline = gstreamer::Pipeline::new();
    pipeline.add_many([video.upcast_ref(), &parser, &mux, &file]).map_err(|e| e.to_string())?;
    gstreamer::Element::link_many([video.upcast_ref(), &parser, &mux, &file]).map_err(|e| e.to_string())?;
    let audio = if with_audio {
        let audio = source(
            gstreamer::Caps::builder("audio/mpeg").field("mpegversion", 4i32).field("stream-format", "adts").build(),
        )?;
        let aac = make("aacparse")?;
        pipeline.add_many([audio.upcast_ref(), &aac]).map_err(|e| e.to_string())?;
        gstreamer::Element::link_many([audio.upcast_ref(), &aac, &mux]).map_err(|e| e.to_string())?;
        Some(audio)
    } else {
        None
    };
    if let Err(e) = pipeline.set_state(state) {
        // The cause (the folder not writable, the path gone) is posted on
        // the bus; the state change itself only says that it failed.
        let why = posted_error(&pipeline).unwrap_or_else(|| format!("could not start writing: {e}"));
        let _ = pipeline.set_state(gstreamer::State::Null);
        return Err(why);
    }
    Ok(Built { pipeline, video, audio })
}

/// A name safe for a file: letters, digits and `-` kept, anything else
/// `_`; `fallback` for an empty one.
fn safe_name(name: &str, fallback: &str) -> String {
    let safe: String = name.trim().chars().map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' }).collect();
    if safe.is_empty() {
        fallback.to_string()
    } else {
        safe
    }
}

/// `<device>-<camera>-<YYYYMMDD>-<HHMMSS>-<duration>s.mp4` for a range of
/// `duration_secs` from `start` (device-local).
pub fn download_file_name(device: &str, camera: &str, start: CivilTime, duration_secs: i64) -> String {
    format!(
        "{}-{}-{:04}{:02}{:02}-{:02}{:02}{:02}-{}s.mp4",
        safe_name(device, "device"),
        safe_name(camera, "camera"),
        start.year,
        start.month,
        start.day,
        start.hour,
        start.minute,
        start.second,
        duration_secs.max(0),
    )
}

/// `hh:mm:ss` of a device-local time.
pub fn clock_text(t: CivilTime) -> String {
    format!("{:02}:{:02}:{:02}", t.hour, t.minute, t.second)
}

/// The range a download asks the device for, from the day's recordings as
/// half-open device-local spans (sorted by start). With a `duration`, the
/// range runs that long from `start` and must overlap a recording (the
/// device sends only what was recorded in it, across file boundaries
/// included). Without one it is the whole recording that holds `start`,
/// or the next one when `start` lies in a gap, as a seek there plays it.
/// `Err` says why there is nothing to download.
pub fn range_of(
    spans: &[(CivilTime, CivilTime)],
    start: CivilTime,
    duration: Option<i64>,
) -> Result<(CivilTime, CivilTime), String> {
    if spans.is_empty() {
        return Err("No recordings on this day".to_string());
    }
    match duration {
        Some(seconds) => {
            let end = start.add_seconds(seconds.max(1));
            if spans.iter().any(|&(s, e)| s < end && start < e) {
                Ok((start, end))
            } else {
                Err(format!("No recording between {} and {}", clock_text(start), clock_text(end)))
            }
        }
        None => {
            let holding = spans.iter().rposition(|&(s, e)| s <= start && start < e);
            let next = || spans.iter().position(|&(s, _)| s > start);
            match holding.or_else(next) {
                Some(index) => Ok(spans[index]),
                None => Err(format!("Nothing recorded after {}", clock_text(start))),
            }
        }
    }
}

/// Reserves a file for a download: `dir/name`, or, when that file exists,
/// `dir/<stem>-2.<ext>`, `-3`…, created empty so that the name is taken
/// from this moment on. A download never writes over an earlier file, and
/// two downloads started together (two devices asking for the same name)
/// never share one: the second gets the next number, even while the first
/// file is still empty. `Err` when no file can be made there (the folder
/// not writable, or not a folder); the caller removes the file when the
/// download does not start.
pub fn reserve_path(dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    let candidates = std::iter::once(name.to_string()).chain((2..).map(|n| format!("{stem}-{n}{ext}")));
    for candidate in candidates {
        let path = dir.join(candidate);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!("a free name among infinitely many")
}

/// "512 B", "1.5 KB", "12.3 MB", "1.07 GB": a byte count as the user
/// reads it.
pub fn size_text(bytes: u64) -> String {
    const KB: f64 = 1000.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.2} GB", b / (KB * KB * KB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_frame_is_at_zero_and_steps_follow_the_footage_clock() {
        let mut clock = WriterClock::default();
        assert_eq!(clock.stamp_video(4_000_000_000), 0);
        assert_eq!(clock.stamp_video(4_000_100_000), 100_000);
        assert_eq!(clock.stamp_video(4_000_140_000), 140_000);
        assert_eq!(clock.duration_us(), 140_000);
        // Across the clock's wrap.
        let mut clock = WriterClock::default();
        assert_eq!(clock.stamp_video(u32::MAX - 49_999), 0);
        assert_eq!(clock.stamp_video(50_000), 100_000);
        assert_eq!(clock.stamp_video(150_000), 200_000);
    }

    #[test]
    fn a_jump_or_a_step_back_is_one_usual_step() {
        let mut clock = WriterClock::default();
        clock.stamp_video(1_000_000);
        clock.stamp_video(1_040_000);
        // A jump of an hour (another clock): one 40 ms step.
        assert_eq!(clock.stamp_video(3_601_040_000), 80_000);
        // Backwards, and the same time twice: one step each.
        assert_eq!(clock.stamp_video(3_601_000_000), 120_000);
        assert_eq!(clock.stamp_video(3_601_000_000), 160_000);
        // Inside a file, a step of 3 s is not believed either; one of
        // 1.5 s still is.
        assert_eq!(clock.stamp_video(3_604_000_000), 200_000);
        assert_eq!(clock.stamp_video(3_605_500_000), 1_700_000);
        // Before any step was measured, the default one.
        let mut fresh = WriterClock::default();
        assert_eq!(fresh.frame_duration_us(), DEFAULT_STEP_US);
        fresh.stamp_video(5);
        assert_eq!(fresh.stamp_video(5), DEFAULT_STEP_US);
        fresh.stamp_video(100_005);
        assert_eq!(fresh.frame_duration_us(), 100_000);
    }

    #[test]
    fn a_gap_between_two_files_is_cut_out_whatever_its_length() {
        let mut clock = WriterClock::default();
        clock.stamp_video(1_000_000);
        assert_eq!(clock.stamp_video(1_100_000), 100_000);
        // An 8 s gap between the files (as an NVR records them when its
        // camera paused): one usual step, not 8 s of a frozen picture.
        clock.at_boundary();
        assert_eq!(clock.stamp_video(9_170_000), 200_000);
        // The boundary is behind: the steps are believed again.
        assert_eq!(clock.stamp_video(9_270_000), 300_000);
        // Adjoining files: the step across the boundary is a usual one
        // anyway.
        clock.at_boundary();
        assert_eq!(clock.stamp_video(9_370_000), 400_000);
        assert_eq!(clock.frame_duration_us(), 100_000);
        // A boundary before any frame changes nothing: the first is at 0.
        let mut fresh = WriterClock::default();
        fresh.at_boundary();
        assert_eq!(fresh.stamp_video(77), 0);
        assert_eq!(fresh.stamp_video(177), 100);
    }

    #[test]
    fn audio_is_contiguous_and_catches_up_with_the_picture() {
        let mut clock = WriterClock::default();
        assert_eq!(clock.stamp_audio(64_000), None, "nothing before the first frame");
        clock.stamp_video(1_000_000);
        assert_eq!(clock.stamp_audio(64_000), Some(0));
        assert_eq!(clock.stamp_audio(64_000), Some(64_000));
        clock.stamp_video(1_100_000);
        // Within reach of the picture: contiguous.
        assert_eq!(clock.stamp_audio(64_000), Some(128_000));
        // Far behind it (a lost stretch): starts again at the picture.
        clock.stamp_video(2_100_000);
        assert_eq!(clock.stamp_audio(64_000), Some(1_100_000));
        // Ahead of it: left to run on, never back in time.
        for _ in 0..10 {
            clock.stamp_audio(64_000);
        }
        assert_eq!(clock.stamp_audio(64_000), Some(1_100_000 + 11 * 64_000));
    }

    #[test]
    fn the_file_starts_once_the_codec_and_the_sound_question_are_known() {
        let sound = |first, later| AudioExpectation { first, later };
        assert!(!ready_to_build(false, sound(true, false), true, 100, true), "no key frame yet");
        assert!(!ready_to_build(false, sound(false, true), false, 100, true), "no key frame yet");
        assert!(ready_to_build(true, sound(false, false), false, 1, false), "no sound expected");
        assert!(ready_to_build(true, sound(true, false), true, 1, false), "sound came");
        assert!(!ready_to_build(true, sound(true, false), false, AUDIO_DECISION_FRAMES - 1, false));
        assert!(ready_to_build(true, sound(true, false), false, AUDIO_DECISION_FRAMES, false), "none came in a second: without sound");
        // Sound from a later recording: the file waits past the second,
        // whatever the first recording's frames bring, until a unit comes
        // or what is held reaches its budget.
        assert!(!ready_to_build(true, sound(false, true), false, 1, false));
        assert!(!ready_to_build(true, sound(false, true), false, 100 * AUDIO_DECISION_FRAMES, false));
        assert!(!ready_to_build(true, sound(true, true), false, 100 * AUDIO_DECISION_FRAMES, false));
        assert!(ready_to_build(true, sound(false, true), true, 1, false), "sound came");
        assert!(ready_to_build(true, sound(false, true), false, 1, true), "the budget is reached: without sound");
        assert!(ready_to_build(true, sound(true, true), false, 1, true));
    }

    fn silent() -> AudioExpectation {
        AudioExpectation::default()
    }

    fn with_sound() -> AudioExpectation {
        AudioExpectation { first: true, later: false }
    }

    /// Whether every element the writer and its check need is installed
    /// (the encoders are test-only).
    fn elements_available(names: &[&str]) -> bool {
        names.iter().all(|n| gstreamer::ElementFactory::find(n).is_some())
    }

    /// Runs `pipeline` (a description ending in an appsink named `out`)
    /// to its end and collects the buffers' bytes, PTS (µs) and whether
    /// each is a key unit.
    fn collect(pipeline: &str) -> Vec<(Vec<u8>, u64, bool)> {
        collect_samples(pipeline).into_iter().map(|(bytes, pts, _, key)| (bytes, pts, key)).collect()
    }

    /// `collect` with each buffer's running time (µs) as well: a track
    /// that starts late in the file comes out of the demuxer in a segment
    /// of its own, whose base carries the delay while the PTS start at 0.
    fn collect_samples(pipeline: &str) -> Vec<(Vec<u8>, u64, u64, bool)> {
        let pipeline = gstreamer::parse::launch(pipeline).unwrap().downcast::<gstreamer::Pipeline>().unwrap();
        let sink = pipeline.by_name("out").unwrap().downcast::<gstreamer_app::AppSink>().unwrap();
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        let mut out = Vec::new();
        while let Ok(sample) = sink.pull_sample() {
            let buffer = sample.buffer().unwrap();
            let key = !buffer.flags().contains(gstreamer::BufferFlags::DELTA_UNIT);
            let pts = buffer.pts().map_or(0, |t| t.useconds());
            let running = sample
                .segment()
                .and_then(|s| s.downcast_ref::<gstreamer::ClockTime>())
                .and_then(|s| s.to_running_time(buffer.pts()))
                .map_or(pts, |t| t.useconds());
            out.push((buffer.map_readable().unwrap().as_slice().to_vec(), pts, running, key));
        }
        pipeline.set_state(gstreamer::State::Null).unwrap();
        out
    }

    #[test]
    fn a_file_with_sound_is_written_and_decodes_again() {
        let _ = gstreamer::init();
        if !elements_available(&["x264enc", "voaacenc", "qtmux", "qtdemux", "avdec_h264", "avdec_aac"]) {
            eprintln!("skipped: the encoders or decoders for the check are not installed");
            return;
        }
        let video = collect(
            "videotestsrc num-buffers=25 ! video/x-raw,width=64,height=48,framerate=25/1 ! x264enc tune=zerolatency key-int-max=10 \
             ! video/x-h264,stream-format=byte-stream,alignment=au ! appsink name=out sync=false",
        );
        let audio = collect(
            "audiotestsrc num-buffers=20 samplesperbuffer=1024 ! audio/x-raw,rate=16000,channels=1 ! voaacenc \
             ! aacparse ! audio/mpeg,stream-format=adts ! appsink name=out sync=false",
        );
        assert!(video.len() == 25 && audio.len() >= 15, "{} video, {} audio", video.len(), audio.len());
        // Marked MPEG-2 in their ADTS headers, as a Home Hub Pro's camera
        // sends them (the muxer refuses those unmarked).
        let audio: Vec<(Vec<u8>, u64, bool)> = audio
            .into_iter()
            .map(|(mut unit, pts, key)| {
                unit[1] |= 0x08;
                (unit, pts, key)
            })
            .collect();
        let dir = std::env::temp_dir().join(format!("reoling-writer-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.mp4");
        let mut writer = Mp4Writer::new(path.clone(), with_sound());
        // The footage clock starts near its wrap; audio interleaved, one
        // unit per frame, and one audio unit ahead of the first key frame.
        let base = u32::MAX - 200_000;
        writer.push(&DownloadUnit::Audio(audio[0].0.clone())).unwrap();
        let mut audio = audio.into_iter().skip(1);
        for (data, pts, key) in &video {
            let frame = VideoFrame {
                video_type: VideoType::H264,
                is_keyframe: *key,
                microseconds: base.wrapping_add(*pts as u32),
                data: data.clone(),
            };
            writer.push(&DownloadUnit::Video(frame)).unwrap();
            if let Some((aac, _, _)) = audio.next() {
                writer.push(&DownloadUnit::Audio(aac)).unwrap();
            }
        }
        // A file boundary: the next file repeats the last key frame's GOP
        // (its first frame at the same footage time) and then goes on.
        let last_pts = video.last().unwrap().1;
        writer.push(&DownloadUnit::Info(reoling::InfoBlock { width: 64, height: 48, start_utc: None, end_utc: None })).unwrap();
        writer
            .push(&DownloadUnit::Video(VideoFrame {
                video_type: VideoType::H264,
                is_keyframe: true,
                microseconds: base.wrapping_add(last_pts as u32),
                data: video[20].0.clone(),
            }))
            .unwrap();
        writer.push(&DownloadUnit::Audio(video[0].0.clone())).unwrap();
        // The next file's new footage starts 8 s on (a gap between the
        // files): it follows at the usual step.
        writer
            .push(&DownloadUnit::Video(VideoFrame {
                video_type: VideoType::H264,
                is_keyframe: true,
                microseconds: base.wrapping_add(last_pts as u32 + 8_000_000),
                data: video[20].0.clone(),
            }))
            .unwrap();
        let stats = writer.finish().unwrap();
        assert_eq!(stats.dropped_repeats, 2, "the repeated frame and its audio");
        assert_eq!((stats.video_frames, stats.dropped_before_keyframe, stats.dropped_other), (26, 1, 0));
        assert!(stats.audio_units >= 15);
        assert_eq!(stats.duration_us, 25 * 40_000, "the gap is cut out");
        assert!(std::fs::metadata(&path).unwrap().len() > 1000);

        let decoded = collect(&format!(
            "filesrc location={} ! qtdemux name=d d.video_0 ! h264parse ! avdec_h264 ! appsink name=out sync=false",
            path.display()
        ));
        assert_eq!(decoded.len(), 26, "every frame decodes again");
        assert_eq!(decoded.last().unwrap().1, 25 * 40_000, "the last frame keeps its time");
        let sound = collect(&format!(
            "filesrc location={} ! qtdemux name=d d.audio_0 ! aacparse ! avdec_aac ! appsink name=out sync=false",
            path.display()
        ));
        assert!(sound.len() >= 15, "the sound decodes again ({} buffers)", sound.len());

        // A discarded file is gone; a file without a key frame is not written.
        let mut discarded = Mp4Writer::new(dir.join("discarded.mp4"), silent());
        discarded.push(&DownloadUnit::Video(VideoFrame { video_type: VideoType::H264, is_keyframe: true, microseconds: 1, data: video[0].0.clone() })).unwrap();
        discarded.discard();
        assert!(!dir.join("discarded.mp4").exists());
        let empty = Mp4Writer::new(dir.join("empty.mp4"), silent());
        assert!(empty.finish().is_err());
        assert!(!dir.join("empty.mp4").exists());

        // A file that cannot be opened fails at the first key frame, with
        // the cause, and takes nothing more.
        let mut unwritable = Mp4Writer::new(dir.join("missing-folder").join("x.mp4"), silent());
        let frame = |n: usize| {
            DownloadUnit::Video(VideoFrame { video_type: VideoType::H264, is_keyframe: n == 0, microseconds: n as u32, data: video[n].0.clone() })
        };
        let error = unwritable.push(&frame(0)).unwrap_err();
        assert!(error.contains("writing the file failed"), "{error}");
        assert!(error.contains("filesink") && error.contains("x.mp4"), "names the cause: {error}");
        assert_eq!(unwritable.push(&frame(1)), Err(error.clone()), "every later unit gets the same answer");
        assert_eq!(unwritable.finish(), Err(error));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_writer_given_up_takes_nothing_more() {
        let _ = gstreamer::init();
        let dir = std::env::temp_dir().join(format!("reoling-writer-given-up-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut writer = Mp4Writer::new(dir.join("x.mp4"), silent());
        writer.video_type = Some(VideoType::H264);
        writer.give_up("gone");
        assert_eq!(writer.push(&DownloadUnit::Audio(vec![1, 2, 3])), Err("gone".to_string()));
        assert_eq!(writer.check(Ok(())), Ok(()), "nothing to report without a pipeline");
        assert_eq!(writer.finish(), Err("gone".to_string()));
        assert!(!dir.join("x.mp4").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Encoded footage for the writer tests: `frames` H.264 frames (a key
    /// frame every ten, the first one included) and `units` AAC units as
    /// ADTS; `None` when the encoders or the decoders for the check are
    /// not installed.
    fn test_footage(frames: usize, units: usize) -> Option<(Vec<(Vec<u8>, u64, bool)>, Vec<Vec<u8>>)> {
        let _ = gstreamer::init();
        if !elements_available(&["x264enc", "voaacenc", "qtmux", "qtdemux", "avdec_h264", "avdec_aac"]) {
            eprintln!("skipped: the encoders or decoders for the check are not installed");
            return None;
        }
        let video = collect(&format!(
            "videotestsrc num-buffers={frames} ! video/x-raw,width=64,height=48,framerate=25/1 ! x264enc tune=zerolatency key-int-max=10 \
             ! video/x-h264,stream-format=byte-stream,alignment=au ! appsink name=out sync=false"
        ));
        assert_eq!(video.len(), frames);
        assert!(video[0].2, "the first frame is a key frame");
        let audio: Vec<Vec<u8>> = if units == 0 {
            Vec::new()
        } else {
            // The encoder holds a few buffers back.
            collect(&format!(
                "audiotestsrc num-buffers={} samplesperbuffer=1024 ! audio/x-raw,rate=16000,channels=1 ! voaacenc \
                 ! aacparse ! audio/mpeg,stream-format=adts ! appsink name=out sync=false",
                units + 8
            ))
            .into_iter()
            .map(|(unit, _, _)| unit)
            .take(units)
            .collect()
        };
        assert_eq!(audio.len(), units);
        Some((video, audio))
    }

    /// Frame `n` of `video` as a download unit with footage time `us`.
    fn frame_unit(video: &[(Vec<u8>, u64, bool)], n: usize, us: u32) -> DownloadUnit {
        let (data, _, key) = &video[n];
        DownloadUnit::Video(VideoFrame { video_type: VideoType::H264, is_keyframe: *key, microseconds: us, data: data.clone() })
    }

    fn info_unit() -> DownloadUnit {
        DownloadUnit::Info(reoling::InfoBlock { width: 64, height: 48, start_utc: None, end_utc: None })
    }

    /// A fresh, empty folder for one test.
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("reoling-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn two_downloads_wanting_the_same_name_keep_apart() {
        let Some((video, _)) = test_footage(25, 0) else { return };
        let dir = test_dir("writer-two-names");
        // Two devices with the same names ask for the same range: each
        // download reserves its own file before a byte comes.
        let a = reserve_path(&dir, "clip.mp4").unwrap();
        let b = reserve_path(&dir, "clip.mp4").unwrap();
        assert_ne!(a, b);
        let mut writer_a = Mp4Writer::new(a.clone(), silent());
        let mut writer_b = Mp4Writer::new(b.clone(), silent());
        for n in 0..25 {
            writer_b.push(&frame_unit(&video, n, 1_000_000 + n as u32 * 40_000)).unwrap();
        }
        // A's download never brings a key frame and is cancelled: only A's
        // own (empty) file goes, B's is untouched.
        writer_a.push(&DownloadUnit::Audio(vec![0xff, 0xf1, 0x60, 0x40, 0x01, 0x3f, 0xfc])).unwrap();
        assert_eq!(writer_a.stats().dropped_before_keyframe, 1);
        writer_a.discard();
        assert!(!a.exists() && b.exists());
        let stats = writer_b.finish().unwrap();
        assert_eq!(stats.video_frames, 25);
        assert!(stats.keyframes >= 3);
        let decoded = collect(&format!(
            "filesrc location={} ! qtdemux name=d d.video_0 ! h264parse ! avdec_h264 ! appsink name=out sync=false",
            b.display()
        ));
        assert_eq!(decoded.len(), 25, "B's file decodes");
        // A download that ends without a key frame removes its own
        // reservation, which becomes free again; a finished file never is.
        let c = reserve_path(&dir, "clip.mp4").unwrap();
        assert_eq!(c, a, "A's name is free again");
        assert_eq!(Mp4Writer::new(c.clone(), silent()).finish(), Err("no video frame came".to_string()));
        assert!(!c.exists() && b.exists());
        assert_eq!(reserve_path(&dir, "clip-2.mp4").unwrap(), dir.join("clip-2-2.mp4"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sound_from_a_later_recording_is_kept_when_the_first_has_none() {
        let Some((video, audio)) = test_footage(50, 30) else { return };
        let dir = test_dir("writer-later-sound");
        let path = dir.join("later-sound.mp4");
        // The list says: the recording the range starts in is silent, a
        // later one has sound.
        let mut writer = Mp4Writer::new(path.clone(), AudioExpectation { first: false, later: true });
        writer.push(&info_unit()).unwrap();
        let at = |n: usize| 1_000_000 + n as u32 * 40_000;
        // Twenty silent frames (well past `AUDIO_DECISION_FRAMES`): held by
        // the writer, since the muxer would write nothing with an audio
        // track that has no unit yet, and cannot be given one later...
        for n in 0..20 {
            writer.push(&frame_unit(&video, n, at(n))).unwrap();
        }
        assert!(writer.built.is_none(), "the file waits for the sound");
        assert_eq!(writer.pending.len(), 20);
        let held: u64 = video[..20].iter().map(|(data, _, _)| data.len() as u64).sum();
        assert_eq!(writer.pending_bytes, held + 20 * PENDING_UNIT_OVERHEAD);
        // ...then the boundary into the recording with sound, a unit per
        // frame: the first unit starts the file, with its audio track, and
        // the muxer takes what was held.
        writer.push(&info_unit()).unwrap();
        writer.push(&frame_unit(&video, 20, at(20))).unwrap();
        assert!(writer.built.is_none());
        writer.push(&DownloadUnit::Audio(audio[0].clone())).unwrap();
        assert!(writer.built.as_ref().is_some_and(|b| b.audio.is_some()), "the first audio unit starts the file, with sound");
        assert!(writer.pending.is_empty() && writer.pending_bytes == 0);
        // (The audio parser passes a unit on once the next one's header has
        // come, so the muxer sees the sound from the second unit on.)
        writer.push(&frame_unit(&video, 21, at(21))).unwrap();
        writer.push(&DownloadUnit::Audio(audio[1].clone())).unwrap();
        let built = writer.built.as_ref().unwrap();
        assert!(drains_within(built, std::time::Duration::from_secs(5)), "the muxer takes the held frames");
        for n in 22..50 {
            writer.push(&frame_unit(&video, n, at(n))).unwrap();
            writer.push(&DownloadUnit::Audio(audio[n - 20].clone())).unwrap();
        }
        let stats = writer.finish().unwrap();
        assert_eq!((stats.video_frames, stats.audio_units, stats.dropped_other, stats.dropped_repeats), (50, 30, 0, 0));
        assert_eq!(stats.duration_us, 49 * 40_000);
        let decoded = collect(&format!(
            "filesrc location={} ! qtdemux name=d d.video_0 ! h264parse ! avdec_h264 ! appsink name=out sync=false",
            path.display()
        ));
        assert_eq!(decoded.len(), 50, "every frame decodes again");
        assert_eq!(decoded.last().unwrap().1, 49 * 40_000);
        let sound = collect_samples(&format!(
            "filesrc location={} ! qtdemux name=d d.audio_0 ! aacparse ! avdec_aac ! appsink name=out sync=false",
            path.display()
        ));
        assert!(sound.len() >= 25, "the later recording's sound decodes again ({} buffers)", sound.len());
        let starts_at = sound[0].2 as i64;
        assert!((starts_at - 20 * 40_000).abs() <= 64_000, "the sound starts with the recording that has it ({starts_at} µs)");

        // Sound promised for later that never comes (the list wrong, or the
        // download ending early): the file is written without sound.
        let path = dir.join("promised-sound.mp4");
        let mut writer = Mp4Writer::new(path.clone(), AudioExpectation { first: false, later: true });
        for n in 0..20 {
            writer.push(&frame_unit(&video, n, at(n))).unwrap();
        }
        assert!(writer.built.is_none(), "still waiting for the sound");
        let stats = writer.finish().unwrap();
        assert_eq!((stats.video_frames, stats.audio_units), (20, 0));
        let decoded = collect(&format!(
            "filesrc location={} ! qtdemux name=d d.video_0 ! h264parse ! avdec_h264 ! appsink name=out sync=false",
            path.display()
        ));
        assert_eq!(decoded.len(), 20, "the silent file decodes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whether `built`'s sources are emptied by the muxer within `wait`.
    fn drains_within(built: &Built, wait: std::time::Duration) -> bool {
        let began = std::time::Instant::now();
        while built.queued_bytes() > 0 {
            if began.elapsed() > wait {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        true
    }

    #[test]
    fn the_wait_for_a_later_recordings_sound_is_bounded() {
        let Some((video, audio)) = test_footage(40, 5) else { return };
        let dir = test_dir("writer-later-sound-budget");
        let path = dir.join("budget.mp4");
        // A budget of ten frames (their bytes and what holding each costs):
        // the tenth frame reaches it.
        let bytes: u64 = video[..10].iter().map(|(data, _, _)| data.len() as u64).sum();
        let budget = bytes + 10 * PENDING_UNIT_OVERHEAD;
        let mut writer = Mp4Writer::new(path.clone(), AudioExpectation { first: false, later: true }).with_pending_budget(budget);
        let at = |n: usize| 1_000_000 + n as u32 * 40_000;
        for n in 0..9 {
            writer.push(&frame_unit(&video, n, at(n))).unwrap();
        }
        assert!(writer.built.is_none() && writer.pending.len() == 9, "held, under the budget");
        writer.push(&frame_unit(&video, 9, at(9))).unwrap();
        let built = writer.built.as_ref().expect("the budget is reached: the file starts");
        assert!(built.audio.is_none(), "without sound");
        assert!(writer.pending.is_empty() && writer.pending_bytes == 0);
        assert!(drains_within(built, std::time::Duration::from_secs(5)), "the muxer takes the frames");
        // The sound that comes after that is left out; the video is kept.
        for n in 10..40 {
            writer.push(&frame_unit(&video, n, at(n))).unwrap();
            if n >= 35 {
                writer.push(&DownloadUnit::Audio(audio[n - 35].clone())).unwrap();
            }
        }
        let stats = writer.finish().unwrap();
        assert_eq!((stats.video_frames, stats.audio_units, stats.dropped_other), (40, 0, 5));
        let decoded = collect(&format!(
            "filesrc location={} ! qtdemux name=d d.video_0 ! h264parse ! avdec_h264 ! appsink name=out sync=false",
            path.display()
        ));
        assert_eq!(decoded.len(), 40, "every frame decodes again");
        // The budget as shipped holds a minute of a main stream.
        assert_eq!(Mp4Writer::new(dir.join("x.mp4"), silent()).pending_budget, 64 * 1024 * 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tiny_and_empty_units_cannot_hold_more_than_the_budget_says() {
        let Some((video, _)) = test_footage(1, 0) else { return };
        let dir = test_dir("writer-tiny-units");
        let path = dir.join("tiny.mp4");
        let tiny = |n: u32, data: Vec<u8>| {
            DownloadUnit::Video(VideoFrame {
                video_type: VideoType::H264,
                is_keyframe: false,
                microseconds: 1_000_000 + n * 40_000,
                data,
            })
        };
        // The key frame and two one-byte units, waiting for a later
        // recording's sound: each held unit counts its entry, not only its
        // bytes...
        let key_bytes = video[0].0.len() as u64;
        let budget = key_bytes + 2 + 3 * PENDING_UNIT_OVERHEAD;
        let mut writer =
            Mp4Writer::new(path.clone(), AudioExpectation { first: false, later: true }).with_pending_budget(budget);
        writer.start_paused = true;
        writer.push(&frame_unit(&video, 0, 1_000_000)).unwrap();
        writer.push(&tiny(1, vec![0])).unwrap();
        assert_eq!(writer.pending_bytes, key_bytes + 1 + 2 * PENDING_UNIT_OVERHEAD);
        // ...an empty unit is no frame at all and is not held...
        writer.push(&tiny(2, Vec::new())).unwrap();
        assert_eq!((writer.pending.len(), writer.stats.dropped_other), (2, 1));
        assert!(writer.built.is_none(), "under the budget: still waiting for the sound");
        // ...and the third held unit reaches the budget: the file starts
        // without sound, however few bytes the units carried.
        writer.push(&tiny(3, vec![0])).unwrap();
        assert!(writer.built.as_ref().is_some_and(|b| b.audio.is_none()));
        assert!(writer.pending.is_empty() && writer.pending_bytes == 0);
        writer.discard();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_writer_fails_instead_of_holding_what_the_storage_cannot_take() {
        let Some((video, _)) = test_footage(25, 0) else { return };
        let dir = test_dir("writer-stalled-storage");
        let path = dir.join("stalled.mp4");
        // A destination that takes nothing (the pipeline stays paused past
        // its preroll) and a small budget: the pushes go through until the
        // sources hold more than the budget, then the writer fails and
        // removes the file rather than queue the rest of the download.
        let mut writer = Mp4Writer::new(path.clone(), silent()).with_queue_budget(1024);
        writer.start_paused = true;
        let mut outcomes = Vec::new();
        for n in 0..25 {
            let pushed = writer.push(&frame_unit(&video, n, 1_000_000 + n as u32 * 40_000));
            let failed = pushed.is_err();
            outcomes.push(pushed);
            if failed {
                break;
            }
        }
        assert!(outcomes[0].is_ok(), "the key frame starts the file");
        let error = outcomes.last().unwrap().clone().expect_err("the budget is passed within a second of footage");
        assert!(error.contains("the storage cannot keep up with the download"), "{error}");
        assert!(!path.exists());
        assert_eq!(writer.push(&frame_unit(&video, 1, 2_000_000)), Err(error.clone()), "nothing more is taken");
        assert_eq!(writer.finish(), Err(error));
        assert!(!path.exists());
        // The budget as shipped is far above what a few frames hold.
        let path = dir.join("plenty.mp4");
        let mut writer = Mp4Writer::new(path.clone(), silent());
        writer.start_paused = true;
        for n in 0..25 {
            writer.push(&frame_unit(&video, n, 1_000_000 + n as u32 * 40_000)).unwrap();
        }
        assert!(writer.built.as_ref().unwrap().queued_bytes() > 0, "the paused pipeline holds what was pushed");
        writer.discard();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_next_files_repeated_gop_is_left_out() {
        let mut skip = BoundarySkip::default();
        assert!(!skip.skips_video(5), "no boundary yet");
        // The boundary after a frame at 2_328_927_038: the next file's key
        // frame repeats that time, its GOP follows, then new footage.
        skip.at_boundary(Some(2_328_927_038));
        assert!(skip.skips_audio());
        assert!(skip.skips_video(2_328_927_038));
        assert!(skip.skips_video(2_328_927_038 - 100_000), "a frame before the last one: a repeat too");
        assert!(skip.skips_audio());
        assert!(!skip.skips_video(2_329_027_038));
        assert!(!skip.skips_audio(), "the skip ends with the first new frame");
        assert!(!skip.skips_video(2_328_927_038), "and stays ended");
        // Another clock (far away): nothing is left out.
        skip.at_boundary(Some(1_000_000_000));
        assert!(!skip.skips_video(500_000_000));
        assert!(!skip.skips_audio());
        // Across the wrap: the frame just after the boundary is new.
        skip.at_boundary(Some(u32::MAX - 10));
        assert!(skip.skips_video(u32::MAX - 10));
        assert!(!skip.skips_video(100_000));
        // No video before the boundary: nothing to repeat.
        skip.at_boundary(None);
        assert!(!skip.skips_video(7) && !skip.skips_audio());
    }

    #[test]
    fn adts_headers_are_marked_mpeg4_for_the_muxer() {
        // Two frames of 9 bytes (a 7-byte header and 2 bytes of data).
        let frame = |id: u8| vec![0xff, 0xf1 | id, 0x60, 0x40, 0x01, 0x3f, 0xfc, 0xaa, 0xbb];
        let unit = [frame(0x08), frame(0x08)].concat();
        assert_eq!(adts_as_mpeg4(&unit), [frame(0), frame(0)].concat());
        assert_eq!(adts_as_mpeg4(&[frame(0), frame(0)].concat()), [frame(0), frame(0)].concat());
        // Not ADTS, or cut short: left alone.
        assert_eq!(adts_as_mpeg4(&[1, 2, 3]), vec![1, 2, 3]);
        let short = &unit[..12];
        assert_eq!(adts_as_mpeg4(short), short.to_vec());
    }

    #[test]
    fn a_range_runs_from_the_start_or_covers_the_whole_recording() {
        let t = |h: u8, m: u8, s: u8| CivilTime::new(2026, 10, 19, h, m, s);
        let spans = [(t(14, 0, 0), t(14, 10, 0)), (t(14, 10, 0), t(14, 20, 0)), (t(16, 0, 0), t(16, 5, 0))];
        // A fixed length from the start, across a boundary or into a gap.
        assert_eq!(range_of(&spans, t(14, 9, 30), Some(60)), Ok((t(14, 9, 30), t(14, 10, 30))));
        assert_eq!(range_of(&spans, t(14, 19, 50), Some(300)), Ok((t(14, 19, 50), t(14, 24, 50))));
        // A start in a gap that reaches the next recording.
        assert_eq!(range_of(&spans, t(15, 59, 30), Some(60)), Ok((t(15, 59, 30), t(16, 0, 30))));
        assert_eq!(
            range_of(&spans, t(15, 0, 0), Some(60)),
            Err("No recording between 15:00:00 and 15:01:00".to_string())
        );
        assert_eq!(range_of(&spans, t(16, 5, 0), Some(30)).unwrap_err(), "No recording between 16:05:00 and 16:05:30");
        assert_eq!(range_of(&spans, t(14, 5, 0), Some(0)), Ok((t(14, 5, 0), t(14, 5, 1))), "never empty");
        // The whole recording: the one holding the start, else the next.
        assert_eq!(range_of(&spans, t(14, 15, 0), None), Ok(spans[1]));
        assert_eq!(range_of(&spans, t(14, 10, 0), None), Ok(spans[1]), "the start of a file is in it");
        assert_eq!(range_of(&spans, t(15, 0, 0), None), Ok(spans[2]));
        assert_eq!(range_of(&spans, t(17, 0, 0), None), Err("Nothing recorded after 17:00:00".to_string()));
        // Overlapping files: the later one, as a seek there opens it.
        let overlapping = [(t(14, 0, 0), t(14, 10, 2)), (t(14, 10, 0), t(14, 20, 0))];
        assert_eq!(range_of(&overlapping, t(14, 10, 1), None), Ok(overlapping[1]));
        assert_eq!(range_of(&[], t(14, 0, 0), None), Err("No recordings on this day".to_string()));
        assert_eq!(range_of(&[], t(14, 0, 0), Some(60)), Err("No recordings on this day".to_string()));
    }

    #[test]
    fn a_download_never_writes_over_an_earlier_file_nor_shares_one_with_another() {
        let dir = std::env::temp_dir().join(format!("reoling-reserve-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The name is taken at once: an empty file holds it.
        assert_eq!(reserve_path(&dir, "clip.mp4").unwrap(), dir.join("clip.mp4"));
        assert_eq!(std::fs::metadata(dir.join("clip.mp4")).unwrap().len(), 0);
        // A second download wanting the same name while the first has not
        // written a byte yet gets its own file.
        assert_eq!(reserve_path(&dir, "clip.mp4").unwrap(), dir.join("clip-2.mp4"));
        // An earlier download's finished file is never written over.
        std::fs::write(dir.join("clip.mp4"), b"x").unwrap();
        assert_eq!(reserve_path(&dir, "clip.mp4").unwrap(), dir.join("clip-3.mp4"));
        std::fs::write(dir.join("noext"), b"x").unwrap();
        assert_eq!(reserve_path(&dir, "noext").unwrap(), dir.join("noext-2"));
        assert_eq!(reserve_path(&dir, ".hidden").unwrap(), dir.join(".hidden"));
        assert_eq!(reserve_path(&dir, ".hidden").unwrap(), dir.join(".hidden-2"));
        // No file can be made where the folder is a file, or missing.
        assert!(reserve_path(&dir.join("noext"), "clip.mp4").is_err());
        assert!(reserve_path(&dir.join("missing"), "clip.mp4").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sizes_read_in_bytes_kilobytes_megabytes_and_gigabytes() {
        assert_eq!(size_text(0), "0 B");
        assert_eq!(size_text(999), "999 B");
        assert_eq!(size_text(1_500), "1.5 KB");
        assert_eq!(size_text(935_344), "935.3 KB");
        assert_eq!(size_text(4_462_205), "4.5 MB");
        assert_eq!(size_text(15_728_640), "15.7 MB");
        assert_eq!(size_text(1_070_000_000), "1.07 GB");
        assert_eq!(clock_text(CivilTime::new(2026, 10, 19, 7, 5, 3)), "07:05:03");
    }

    #[test]
    fn file_names_carry_the_device_the_camera_the_start_and_the_length() {
        let start = CivilTime::new(2026, 10, 17, 0, 48, 5);
        assert_eq!(download_file_name("RLN36 NVR", "Camera 1", start, 68), "RLN36_NVR-Camera_1-20261017-004805-68s.mp4");
        assert_eq!(download_file_name("  ", "", start, -3), "device-camera-20261017-004805-0s.mp4");
        assert_eq!(download_file_name("Home-Hub/Pro", "Tür", start, 30), "Home-Hub_Pro-Tür-20261017-004805-30s.mp4");
    }
}
