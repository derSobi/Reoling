//! A recording download on a device's network thread.
//!
//! The Playback page asks for a range of one channel's recordings
//! (`DownloadRequest`, through `DeviceLink::download`). The device's
//! thread closes the playback it may be running (a download reads the
//! connection itself, as a search does, so nothing else can be open),
//! resolves the range against the day's recordings, reads the data from
//! the device as fast as the path carries them and writes them to an MP4
//! file (`Mp4Writer`). The page hears `DownloadEvent`s: the start with the
//! file's name and the device's size estimate, progress a few times a
//! second, and the end. Every event names the page's playback session the
//! request came from, so that a page that has moved on (another day or
//! device) drops them; the download itself runs to its end, or until it
//! is cancelled. Playback and live-video commands that arrive meanwhile
//! wait in `bridge.rs` until it is over.

use crate::ui::bridge::DeviceEvent;
use crate::ui::playback_driver::PlaybackDriver;
use crate::ui::recording_file::{download_file_name, range_of, unused_path, Mp4Writer};
use reoling::{CivilTime, DownloadCancel, DownloadUnit, RecordStream, RecordingFile, ReolinkClient};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

/// How often progress goes to the page, at most.
const REPORT_EVERY: Duration = Duration::from_millis(250);

/// How long the client is given to stop the download's reader once the
/// data have ended (after a cancel the device stops within a second).
const REAP_WAIT: Duration = Duration::from_secs(10);

/// What the page asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRequest {
    /// The page's playback session; every event of the download names it.
    pub session: u64,
    pub channel: u8,
    /// Midnight of the page's day: whose recordings the range is resolved
    /// against (searched anew when the stream's are not loaded).
    pub day: CivilTime,
    /// Device-local.
    pub start: CivilTime,
    /// Seconds from `start`; `None`: the whole recording that holds it.
    pub duration: Option<i64>,
    pub stream: RecordStream,
    /// The folder the file goes into.
    pub dir: PathBuf,
    /// For the file's name.
    pub device: String,
    pub camera: String,
}

/// What the page hears about a download.
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadEvent {
    /// The device accepted the request: the file's name, the device's size
    /// estimate (`None` when it gave none) and the range asked for.
    Started { session: u64, name: String, estimate: Option<u64>, start: CivilTime, end: CivilTime },
    /// Bytes received so far (a few times a second).
    Progress { session: u64, bytes: u64, estimate: Option<u64> },
    Finished { session: u64, outcome: DownloadOutcome },
}

#[derive(Debug, Clone, PartialEq)]
pub enum DownloadOutcome {
    Saved { name: String, path: PathBuf, bytes: u64 },
    /// By the user; the partial file is gone.
    Cancelled,
    /// Why not (a sentence for the user); a partial file is gone.
    Failed(String),
}

impl DownloadEvent {
    pub fn session(&self) -> u64 {
        match self {
            DownloadEvent::Started { session, .. }
            | DownloadEvent::Progress { session, .. }
            | DownloadEvent::Finished { session, .. } => *session,
        }
    }
}

/// The range as the device is asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub start: CivilTime,
    pub end: CivilTime,
    pub duration_secs: i64,
    /// A recording in the range has sound (what the file's audio track
    /// waits for).
    pub expect_audio: bool,
}

/// The range of `files` (the day's, sorted) a request for `start` and
/// `duration` means, or why there is nothing to download.
pub(crate) fn resolve(files: &[RecordingFile], start: CivilTime, duration: Option<i64>) -> Result<Resolved, String> {
    let spans: Vec<(CivilTime, CivilTime)> = files.iter().map(|f| (f.start, f.end)).collect();
    let (start, end) = range_of(&spans, start, duration)?;
    let expect_audio = files.iter().any(|f| f.start < end && start < f.end && f.has_audio);
    Ok(Resolved { start, end, duration_secs: end.to_epoch_seconds() - start.to_epoch_seconds(), expect_audio })
}

/// How a download's data ended.
pub(crate) enum Ended {
    /// The device's end marker came: the file is completed.
    Complete,
    Cancelled,
    Failed(String),
}

type Events = async_channel::Sender<DeviceEvent>;

/// A running download: its units, the file being written and what has
/// been told to the page.
pub(crate) struct DownloadJob {
    session: u64,
    name: String,
    path: PathBuf,
    units: ReceiverStream<reoling::Result<DownloadUnit>>,
    cancel: DownloadCancel,
    writer: Option<Mp4Writer>,
    estimate: Option<u64>,
    bytes: u64,
    reported_at: Instant,
    /// The file could not be written: the data are read to their end all
    /// the same (the device stops at the cancel), then the file is removed.
    failure: Option<String>,
    /// The page was told that playback waits for the download.
    told_waiting: bool,
}

impl DownloadJob {
    /// Starts `request`'s download; the page hears `Started`, or
    /// `Finished` with why not (then there is no job).
    pub(crate) async fn start(
        request: DownloadRequest,
        driver: &mut PlaybackDriver,
        client: &mut ReolinkClient,
        tx: &Events,
    ) -> Option<Self> {
        let session = request.session;
        match Self::open(request, driver, client, tx).await {
            Ok(job) => Some(job),
            Err(message) => {
                let outcome = DownloadOutcome::Failed(message);
                let _ = tx.send(DeviceEvent::Download(DownloadEvent::Finished { session, outcome })).await;
                None
            }
        }
    }

    async fn open(
        request: DownloadRequest,
        driver: &mut PlaybackDriver,
        client: &mut ReolinkClient,
        tx: &Events,
    ) -> Result<Self, String> {
        // The download reads the connection itself: nothing may be open.
        driver.pause_for_download(client, tx).await;
        let files = match driver.loaded_files(request.channel, request.day, request.stream) {
            Some(files) => files,
            None => client
                .search_day(request.channel, request.day, request.stream)
                .await
                .map_err(|e| format!("Could not list the recordings: {e}"))?,
        };
        let resolved = resolve(&files, request.start, request.duration)?;
        std::fs::create_dir_all(&request.dir).map_err(|e| format!("Could not create the folder: {e}"))?;
        let wanted = download_file_name(&request.device, &request.camera, resolved.start, resolved.duration_secs);
        let path = unused_path(&request.dir, &wanted);
        let name = path.file_name().map_or(wanted, |n| n.to_string_lossy().to_string());
        let stream = match client.download_range(request.channel, request.stream, resolved.start, resolved.end).await {
            Ok(stream) => stream,
            Err(e) => {
                let _ = client.stop_download_within(REAP_WAIT).await;
                return Err(format!("Could not start the download: {e}"));
            }
        };
        let estimate = stream.estimate.filter(|&e| e > 0);
        let started = DownloadEvent::Started {
            session: request.session,
            name: name.clone(),
            estimate,
            start: resolved.start,
            end: resolved.end,
        };
        let _ = tx.send(DeviceEvent::Download(started)).await;
        Ok(DownloadJob {
            session: request.session,
            name,
            path: path.clone(),
            units: stream.units,
            cancel: stream.cancel,
            writer: Some(Mp4Writer::new(path, resolved.expect_audio)),
            estimate,
            bytes: 0,
            reported_at: Instant::now(),
            failure: None,
            told_waiting: false,
        })
    }

    /// Whether the page has been told once that playback waits for the
    /// download (sets it as told).
    pub(crate) fn first_wait(&mut self) -> bool {
        !std::mem::replace(&mut self.told_waiting, true)
    }

    pub(crate) async fn next_unit(&mut self) -> Option<reoling::Result<DownloadUnit>> {
        self.units.next().await
    }

    /// One unit (or the end, or an error) of the download's data: written
    /// to the file, progress passed on. `Some` once the data have ended.
    pub(crate) async fn on_unit(&mut self, item: Option<reoling::Result<DownloadUnit>>, tx: &Events) -> Option<Ended> {
        match item {
            Some(Ok(DownloadUnit::Done(summary))) => Some(match self.failure.take() {
                Some(failure) => Ended::Failed(failure),
                None if summary.complete => Ended::Complete,
                None if summary.cancelled => Ended::Cancelled,
                None => Ended::Failed("The device ended the download early".to_string()),
            }),
            Some(Ok(DownloadUnit::Progress(progress))) => {
                self.bytes = progress.bytes;
                if self.reported_at.elapsed() >= REPORT_EVERY {
                    self.reported_at = Instant::now();
                    let event = DownloadEvent::Progress { session: self.session, bytes: self.bytes, estimate: self.estimate };
                    let _ = tx.send(DeviceEvent::Download(event)).await;
                }
                None
            }
            Some(Ok(unit)) => {
                if self.failure.is_none() {
                    if let Some(writer) = self.writer.as_mut() {
                        if let Err(e) = writer.push(&unit) {
                            self.failure = Some(format!("Could not write the file: {e}"));
                            self.cancel.cancel();
                        }
                    }
                }
                None
            }
            Some(Err(e)) => Some(Ended::Failed(format!("The download broke off: {e}"))),
            None => Some(match self.failure.take() {
                Some(failure) => Ended::Failed(failure),
                None => Ended::Failed("The download ended before its end marker".to_string()),
            }),
        }
    }

    /// Asks the device to stop (the data end with a cancelled `Done`).
    pub(crate) fn cancel(&self) {
        self.cancel.cancel();
    }

    /// The data have ended: the client's reader is stopped (so that the
    /// connection is free again), the file completed or removed, and the
    /// page told. Completing the file waits for the muxer (up to a few
    /// seconds for a long range).
    pub(crate) async fn finish(mut self, ended: Ended, client: &mut ReolinkClient, tx: &Events) {
        let _ = client.stop_download_within(REAP_WAIT).await;
        let writer = self.writer.take();
        let outcome = match ended {
            Ended::Complete => match writer.map(Mp4Writer::finish) {
                Some(Ok(_)) => {
                    let bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(self.bytes);
                    DownloadOutcome::Saved { name: self.name.clone(), path: self.path.clone(), bytes }
                }
                Some(Err(e)) => DownloadOutcome::Failed(format!("Could not write the file: {e}")),
                None => DownloadOutcome::Failed("Nothing was written".to_string()),
            },
            Ended::Cancelled => {
                if let Some(writer) = writer {
                    writer.discard();
                }
                DownloadOutcome::Cancelled
            }
            Ended::Failed(message) => {
                if let Some(writer) = writer {
                    writer.discard();
                }
                DownloadOutcome::Failed(message)
            }
        };
        let _ = tx.send(DeviceEvent::Download(DownloadEvent::Finished { session: self.session, outcome })).await;
    }

    /// The connection is going away: the partial file is removed.
    pub(crate) fn abandon(mut self) {
        if let Some(writer) = self.writer.take() {
            writer.discard();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reoling::RecordingId;

    fn t(h: u8, m: u8, s: u8) -> CivilTime {
        CivilTime::new(2026, 10, 19, h, m, s)
    }

    fn file(start: CivilTime, secs: i64, has_audio: bool) -> RecordingFile {
        RecordingFile {
            channel_id: 0,
            name: format!("01{:04}{:02}{:02}{:02}{:02}{:02}", start.year, start.month, start.day, start.hour, start.minute, start.second),
            id: RecordingId::new("x"),
            stream: RecordStream::Sub,
            start,
            end: start.add_seconds(secs),
            size_bytes: None,
            support_sub: None,
            encrypted: false,
            has_audio,
        }
    }

    #[test]
    fn a_request_resolves_to_the_range_and_whether_sound_is_expected() {
        let files = [file(t(14, 0, 0), 600, false), file(t(14, 10, 0), 600, true), file(t(16, 0, 0), 300, true)];
        // A minute inside the silent file.
        let r = resolve(&files, t(14, 2, 0), Some(60)).unwrap();
        assert_eq!((r.start, r.end, r.duration_secs, r.expect_audio), (t(14, 2, 0), t(14, 3, 0), 60, false));
        // Across the boundary into the file with sound: sound is expected.
        let r = resolve(&files, t(14, 9, 30), Some(60)).unwrap();
        assert_eq!((r.start, r.end, r.expect_audio), (t(14, 9, 30), t(14, 10, 30), true));
        // The whole recording that holds the start.
        let r = resolve(&files, t(14, 15, 0), None).unwrap();
        assert_eq!((r.start, r.end, r.duration_secs, r.expect_audio), (t(14, 10, 0), t(14, 20, 0), 600, true));
        // A start in a gap: the next recording.
        let r = resolve(&files, t(15, 0, 0), None).unwrap();
        assert_eq!((r.start, r.duration_secs), (t(16, 0, 0), 300));
        assert_eq!(resolve(&files, t(15, 0, 0), Some(60)).unwrap_err(), "No recording between 15:00:00 and 15:01:00");
        assert_eq!(resolve(&files, t(17, 0, 0), None).unwrap_err(), "Nothing recorded after 17:00:00");
        assert_eq!(resolve(&[], t(14, 0, 0), Some(60)).unwrap_err(), "No recordings on this day");
    }

    #[test]
    fn every_event_names_its_session() {
        let started =
            DownloadEvent::Started { session: 7, name: "a.mp4".into(), estimate: Some(1), start: t(1, 0, 0), end: t(1, 1, 0) };
        assert_eq!(started.session(), 7);
        assert_eq!(DownloadEvent::Progress { session: 8, bytes: 1, estimate: None }.session(), 8);
        assert_eq!(DownloadEvent::Finished { session: 9, outcome: DownloadOutcome::Cancelled }.session(), 9);
    }
}
