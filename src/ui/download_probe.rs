//! Diagnostic-only (`REOLING_PROBE_DOWNLOAD=<channel>`): a scripted
//! download session on one channel of every device the app connects to,
//! reported on stderr with lines that start with "PROBE download [tag]",
//! `tag` naming the device's model (never its UID).
//!
//! It reads the calendar of the current month (or the one before),
//! searches the sub stream of the latest recorded day, picks the newest
//! file of at least a minute that has a successor (else the newest long
//! one, else the newest) and downloads, to MP4 files in the directory
//! `REOLING_PROBE_DOWNLOAD_DIR` names (else the app's recordings folder):
//! (a) 30 s from the file's start; (b) 20 s around its end, into the next
//! file, when there is one (whether a second info block or another code
//! marks the boundary); (c) the whole file, cancelled after 2 s (what the
//! device does with an early end request). Every MP4 is
//! finished by [`Mp4Writer`]; the cancelled one is discarded, as the app
//! would.

use crate::ui::recording_file::{download_file_name, AudioExpectation, Mp4Writer};
use reoling::{CivilTime, DeviceIdentity, DownloadSummary, DownloadUnit, RecordStream, RecordingFile, ReolinkClient};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio_stream::StreamExt;

/// The shortest file the probe prefers.
const MIN_FILE_SECS: i64 = 60;

/// How long one download may take in all before the probe gives it up.
const DOWNLOAD_WAIT: Duration = Duration::from_secs(150);

/// The channel `REOLING_PROBE_DOWNLOAD` asks the probe to run on.
pub fn probe_channel() -> Option<u8> {
    let value = std::env::var("REOLING_PROBE_DOWNLOAD").ok()?;
    match value.trim().parse() {
        Ok(channel_id) => Some(channel_id),
        Err(_) => {
            eprintln!("PROBE download: REOLING_PROBE_DOWNLOAD must be a channel number, got {value:?}");
            None
        }
    }
}

fn civil(t: &CivilTime) -> String {
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// How soon after a file's end its successor must start for the two to
/// count as adjoining (a range across the boundary then holds footage of
/// both).
const ADJOINING_SECS: i64 = 10;

/// Which file of a sorted day list the probe downloads from: the newest
/// of at least `MIN_FILE_SECS` whose successor adjoins it, else the newest
/// such file, else the newest. The index into `files`.
fn pick(files: &[RecordingFile]) -> Option<usize> {
    let long: Vec<usize> = (0..files.len()).filter(|&i| files[i].duration_seconds() >= MIN_FILE_SECS).collect();
    let adjoins = |i: usize| {
        files.get(i + 1).is_some_and(|next| next.start.to_epoch_seconds() - files[i].end.to_epoch_seconds() <= ADJOINING_SECS)
    };
    long.iter().rev().find(|&&i| adjoins(i)).or(long.last()).copied().or(files.len().checked_sub(1))
}

/// Where the MP4 files go: `REOLING_PROBE_DOWNLOAD_DIR`, else the app's
/// recordings folder (the one chosen in the settings, else
/// `~/Videos/Reoling`), created on demand.
fn output_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("REOLING_PROBE_DOWNLOAD_DIR") {
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir).ok()?;
        return Some(dir);
    }
    let settings = crate::ui::settings::Settings::load();
    crate::ui::settings::media_dir(settings.recording_dir.as_deref(), gtk4::glib::UserDirectory::Videos)
}

/// Runs the probe on `client` (just logged in) for `channel_id`.
pub async fn run(client: &mut ReolinkClient, channel_id: u8, identity: &DeviceIdentity) {
    let tag = identity.model.as_deref().map(str::trim).filter(|m| !m.is_empty()).unwrap_or("device").to_string();
    let model = tag.chars().map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' }).collect::<String>();
    let p = |line: String| eprintln!("PROBE download [{tag}]: {line}");
    p(format!("channel {channel_id}"));
    let has_uid = client.wait_for_channel_uid(channel_id, Duration::from_secs(10)).await;
    p(format!("camera uid for channel {channel_id}: {}", if has_uid { "present" } else { "absent" }));
    let Some(dir) = output_dir() else {
        p("no output directory; done".to_string());
        return;
    };

    // The latest recorded day: this month, else the one before.
    let now = CivilTime::from_epoch_seconds(unix_now());
    let previous = if now.month <= 1 { (now.year - 1, 12) } else { (now.year, now.month - 1) };
    let mut day = None;
    for (year, month) in [(now.year, now.month), previous] {
        match client.search_month(channel_id, year, month).await {
            Ok(days) => {
                p(format!("calendar {year}-{month:02}: days with recordings {days:?}"));
                if let Some(&d) = days.last() {
                    day = Some(CivilTime::start_of_day(year, month, d));
                    break;
                }
            }
            Err(e) => p(format!("calendar {year}-{month:02} failed: {e}")),
        }
    }
    let Some(day) = day else {
        p("no recorded day found; done".to_string());
        return;
    };
    let files = match client.search_day(channel_id, day, RecordStream::Sub).await {
        Ok(files) => files,
        Err(e) => {
            p(format!("day search failed: {e}; done"));
            return;
        }
    };
    p(format!("{} sub-stream files on {}", files.len(), civil(&day)));
    for file in &files {
        p(format!(
            "  {} {} .. {} ({} s) size {:?} audio {}",
            file.name,
            civil(&file.start),
            civil(&file.end),
            file.duration_seconds(),
            file.size_bytes,
            file.has_audio
        ));
    }
    let Some(index) = pick(&files) else {
        p("no file to download; done".to_string());
        return;
    };
    let file = files[index].clone();
    let next = files.get(index + 1).cloned();
    p(format!(
        "picked {} ({} s), next file {}",
        file.name,
        file.duration_seconds(),
        next.as_ref().map_or("none".to_string(), |n| format!("{} ({} .. {})", n.name, civil(&n.start), civil(&n.end)))
    ));

    // (a) 30 s from the file's start.
    let end = file.start.add_seconds(30).min(file.end);
    let path = dir.join(format!("download-{model}-1.mp4"));
    p(format!("(a) {} s from the file's start -> {}", end.to_epoch_seconds() - file.start.to_epoch_seconds(), path.display()));
    download(client, channel_id, &file, file.start, end, &path, None, &tag).await;

    // (b) 20 s around the boundary into the next file.
    match &next {
        Some(next) if next.start <= file.end.add_seconds(ADJOINING_SECS) => {
            let start = file.end.add_seconds(-10).max(file.start);
            let end = file.end.add_seconds(10).min(next.end);
            let path = dir.join(format!("download-{model}-2.mp4"));
            p(format!(
                "(b) {} .. {} across the boundary at {} (next starts {}) -> {}",
                civil(&start),
                civil(&end),
                civil(&file.end),
                civil(&next.start),
                path.display()
            ));
            download(client, channel_id, &file, start, end, &path, None, &tag).await;
        }
        Some(next) => p(format!("(b) skipped: the next file starts {} s after this one ends", next.start.to_epoch_seconds() - file.end.to_epoch_seconds())),
        None => p("(b) skipped: no next file".to_string()),
    }

    // (c) the whole file (up to ten minutes; a minute of sub stream came
    // in under two seconds on both devices), cancelled after 2 s.
    let end = file.start.add_seconds(600).min(file.end);
    let path = dir.join(format!("download-{model}-3.mp4"));
    p(format!("(c) {} s from the file's start, cancelled after 2 s -> {}", end.to_epoch_seconds() - file.start.to_epoch_seconds(), path.display()));
    download(client, channel_id, &file, file.start, end, &path, Some(Duration::from_secs(2)), &tag).await;
    p("done".to_string());
    // The name the app would give the first range, for the record.
    p(format!(
        "app file name for (a): {}",
        download_file_name(identity.name.as_deref().unwrap_or("device"), &format!("ch{}", channel_id), file.start, 30)
    ));
}

/// One download of `start..end` of `file`'s channel and stream into
/// `path`, cancelled after `cancel_after` if given; reports what came.
#[allow(clippy::too_many_arguments)]
async fn download(
    client: &mut ReolinkClient,
    channel_id: u8,
    file: &RecordingFile,
    start: CivilTime,
    end: CivilTime,
    path: &std::path::Path,
    cancel_after: Option<Duration>,
    tag: &str,
) -> Option<DownloadSummary> {
    let p = |line: String| eprintln!("PROBE download [{tag}]: {line}");
    let began = Instant::now();
    let mut stream = match client.download_range_probed(channel_id, file.stream, start, end, tag).await {
        Ok(stream) => stream,
        Err(e) => {
            p(format!("download failed to start after {:.2} s: {e}", began.elapsed().as_secs_f64()));
            let _ = client.stop_download().await;
            return None;
        }
    };
    p(format!(
        "answered after {:.2} s: estimate {:?} B, file count {:?}",
        began.elapsed().as_secs_f64(),
        stream.estimate,
        stream.file_count
    ));
    let mut writer = Mp4Writer::new(path.to_path_buf(), AudioExpectation { first: file.has_audio, later: false });
    let (mut info_blocks, mut first_data_at, mut cancelled_at, mut progress_lines) = (0u64, None::<Instant>, None::<Instant>, 0u32);
    let mut outcome: Option<DownloadSummary> = None;
    let mut failed = false;
    let cancel = stream.cancel.clone();
    let reading = async {
        loop {
            let next = match cancel_after {
                Some(after) if cancelled_at.is_none() => {
                    let left = after.saturating_sub(began.elapsed());
                    match tokio::time::timeout(left, stream.units.next()).await {
                        Ok(next) => next,
                        Err(_) => {
                            cancel.cancel();
                            cancelled_at = Some(Instant::now());
                            p(format!("cancelled after {:.2} s", began.elapsed().as_secs_f64()));
                            continue;
                        }
                    }
                }
                _ => stream.units.next().await,
            };
            match next {
                Some(Ok(unit)) => {
                    match &unit {
                        DownloadUnit::Info(info) => {
                            info_blocks += 1;
                            first_data_at.get_or_insert_with(Instant::now);
                            p(format!(
                                "info block #{info_blocks} after {:.2} s: {}x{} file {} .. {} UTC",
                                began.elapsed().as_secs_f64(),
                                info.width,
                                info.height,
                                info.start_utc.as_ref().map_or("-".to_string(), civil),
                                info.end_utc.as_ref().map_or("-".to_string(), civil),
                            ));
                        }
                        DownloadUnit::Video(_) | DownloadUnit::Audio(_) => {
                            first_data_at.get_or_insert_with(Instant::now);
                        }
                        DownloadUnit::Progress(progress) => {
                            progress_lines += 1;
                            if progress_lines <= 3 || progress_lines % 10 == 0 {
                                p(format!(
                                    "progress after {:.2} s: {} B{}",
                                    began.elapsed().as_secs_f64(),
                                    progress.bytes,
                                    progress.fraction().map_or(String::new(), |f| format!(" ({:.0} % of the estimate)", f * 100.0)),
                                ));
                            }
                        }
                        DownloadUnit::Done(summary) => {
                            outcome = Some(summary.clone());
                            break;
                        }
                    }
                    if let Err(e) = writer.push(&unit) {
                        p(format!("the file writer failed: {e}"));
                        failed = true;
                        cancel.cancel();
                    }
                }
                Some(Err(e)) => {
                    p(format!("stream error after {:.2} s: {e}", began.elapsed().as_secs_f64()));
                    failed = true;
                    break;
                }
                None => break,
            }
        }
    };
    if tokio::time::timeout(DOWNLOAD_WAIT, reading).await.is_err() {
        p(format!("gave the download up after {DOWNLOAD_WAIT:?}"));
        failed = true;
    }
    let stats = writer.stats();
    let wall = began.elapsed().as_secs_f64();
    let footage = stats.duration_us as f64 / 1e6;
    match &outcome {
        Some(s) => p(format!(
            "{} after {wall:.2} s: {} chunks, {} B{}, {} file(s), {} info block(s), {} video frames ({} key), {} audio units, codes other than 200/300/331 {:?}, end marker {}, end reply {:?}; after the end request {} chunks ({} B); footage {footage:.1} s ({} frames and {} audio units written, {} dropped before the key frame, {} repeated at a boundary, {} other)",
            if s.complete { "complete" } else if s.cancelled { "cancelled" } else { "ended" },
            s.chunks,
            s.bytes,
            stream.estimate.map_or(String::new(), |e| format!(" of {e} estimated ({:.0} %)", s.bytes as f64 * 100.0 / e.max(1) as f64)),
            s.files,
            s.info_blocks,
            s.video_frames,
            s.keyframes,
            s.audio_units,
            s.other_codes,
            s.end_marker,
            s.end_reply,
            s.chunks_after_cancel,
            s.bytes_after_cancel,
            stats.video_frames,
            stats.audio_units,
            stats.dropped_before_keyframe,
            stats.dropped_repeats,
            stats.dropped_other,
        )),
        None => p(format!("no summary after {wall:.2} s (failed {failed})")),
    }
    if let (Some(first), Some(s)) = (first_data_at, &outcome) {
        let transfer = first.elapsed().as_secs_f64().max(0.001);
        p(format!("rate: {} B in {transfer:.2} s = {:.0} KB/s", s.bytes, s.bytes as f64 / transfer / 1000.0));
    }
    let complete = outcome.as_ref().is_some_and(|s| s.complete) && !failed;
    if complete {
        match writer.finish() {
            Ok(stats) => {
                let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                p(format!(
                    "wrote {} ({size} B): {} frames ({} key), {} audio units, {:.2} s of footage",
                    path.display(),
                    stats.video_frames,
                    stats.keyframes,
                    stats.audio_units,
                    stats.duration_us as f64 / 1e6
                ));
            }
            Err(e) => p(format!("finishing {} failed: {e}", path.display())),
        }
    } else {
        writer.discard();
        p(format!("discarded {} (not complete)", path.display()));
    }
    // The client is left clean for the next request.
    let stopped = client.stop_download().await;
    p(format!(
        "stop_download after {:.2} s: {}",
        began.elapsed().as_secs_f64(),
        stopped.as_ref().map_or("no reader".to_string(), |s| format!(
            "reader stopped (cancelled {}, end marker {}, {} chunks, {} after the end request)",
            s.cancelled, s.end_marker, s.chunks, s.chunks_after_cancel
        ))
    ));
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use reoling::RecordingId;

    fn file(name: &str, start_s: i64, secs: i64) -> RecordingFile {
        let start = CivilTime::from_epoch_seconds(1_792_300_000 + start_s);
        RecordingFile {
            channel_id: 0,
            name: name.to_string(),
            id: RecordingId::new(name),
            stream: RecordStream::Sub,
            start,
            end: start.add_seconds(secs),
            size_bytes: None,
            support_sub: Some(1),
            encrypted: false,
            has_audio: true,
        }
    }

    #[test]
    fn the_probe_prefers_a_long_file_with_an_adjoining_successor() {
        assert_eq!(pick(&[]), None);
        let files = vec![file("a", 0, 600), file("b", 600, 20), file("c", 620, 600), file("d", 1220, 30)];
        assert_eq!(pick(&files), Some(2), "the newest long file whose next file adjoins it");
        let files = vec![file("a", 0, 600), file("b", 600, 600), file("c", 5000, 600), file("d", 5600, 30)];
        assert_eq!(pick(&files), Some(2));
        let files = vec![file("a", 0, 600), file("b", 600, 600), file("c", 5000, 600), file("d", 9000, 30)];
        assert_eq!(pick(&files), Some(0), "c's successor is far away; a's adjoins");
        let files = vec![file("a", 0, 10), file("b", 10, 600)];
        assert_eq!(pick(&files), Some(1), "the only long file, even without a successor");
        let files = vec![file("a", 0, 10), file("b", 10, 20)];
        assert_eq!(pick(&files), Some(1), "no long file: the newest");
    }
}
