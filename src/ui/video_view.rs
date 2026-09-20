use gstreamer::prelude::*;
use gstreamer_app::AppSrc;
use gtk4::Picture;
use reoling::{VideoFrame, VideoType};
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

/// The largest step between two consecutive `frame.microseconds` values
/// worth believing. Past it, the value is a clock that jumped (a real,
/// recurring, camera-side timestamp-reporting artifact on this device
/// family — see [[project-video-stutter-investigation]]) rather than a
/// real gap in captured frames. `leolink` (a sibling Qt/libmpv Reolink
/// client, `github.com/tombueng/leolink`, commit `8f0e61b`, 2026-09-14)
/// independently landed on the same 5s figure for the same reason, and
/// `bairelay`'s own `PACER_ANOMALY_CAP_TICKS` (checked 2026-09-15) is also
/// 5s — three independent implementations agreeing is a much stronger
/// signal than any one of them alone.
const MAX_PLAUSIBLE_STEP_US: u64 = 5_000_000;

/// What to advance the presentation clock by when the measured step isn't
/// plausible and no better step is known yet (i.e. the very first frame's
/// implausible successor) — this camera family's nominal cadence, 25fps.
const DEFAULT_STEP_US: u64 = 40_000;

/// The camera-clock PTS bookkeeping `VideoSink::push_frame` mutates on
/// every call — see its doc comment for the algorithm. Plain data (not
/// `Cell`-wrapped) because `VideoSink` is pushed to and used from a
/// background thread, not the GTK main thread — see `VideoSink`'s own doc
/// comment for why.
#[derive(Default)]
struct PtsState {
    last_raw_time: Option<u32>,
    accumulated_us: u64,
    last_step_us: u64,
    // DEBUG-ONLY, temporary (`REOLING_DEBUG_PTS`): real wall-clock time of
    // the previous `push_frame`, to log real inter-arrival gaps alongside
    // the computed PTS and the pipeline's running time.
    last_push_wall: Option<std::time::Instant>,
}

/// A cheap, `Send + Sync` handle to a built pipeline's `appsrc` — cloning
/// this clones GObject references, not the pipeline itself.
/// `gstreamer::Pipeline`/`AppSrc` are internally thread-safe (GStreamer
/// elements are designed to be pushed to from any thread; this is the
/// standard pattern for a live source), so once obtained via
/// `VideoView::ensure_sink` (which must run on the GTK main thread — it
/// touches the `Picture` widget), `push_frame` can and should be called
/// directly from the network/media thread that actually has the frame
/// data, without hopping through the GTK main loop at all.
///
/// Read `.plans/reoling-baichuan-p2p-audit-2026-09-16.md` section 34
/// ("GTK main thread") for why this split exists: routing every video
/// frame through `glib::spawn_future_local` coupled frame delivery to
/// whatever else the GTK main loop happened to be doing (layout, redraws,
/// other widget updates), which is exactly the kind of thing a live video
/// source shouldn't depend on for its own pacing.
#[derive(Clone)]
pub struct VideoSink {
    appsrc: AppSrc,
    pipeline: gstreamer::Pipeline,
    pts_state: Arc<Mutex<PtsState>>,
}

pub struct VideoView {
    picture: Picture,

    // The codec (H.264 vs H.265) is only known once the first frame
    // arrives, so the pipeline is built lazily on the first ensure_sink()
    // call.
    sink: RefCell<Option<VideoSink>>,
}

impl VideoView {
    pub fn new() -> Self {
        Self {
            picture: Picture::new(),
            sink: RefCell::new(None),
        }
    }

    pub fn widget(&self) -> &Picture {
        &self.picture
    }

    /// Build:
    ///
    /// appsrc
    ///   ! h264parse / h265parse
    ///   ! queue                 (compressed-side reserve, see below)
    ///   ! decodebin
    ///   ! videoconvert
    ///   ! gtk4paintablesink
    ///
    /// `VideoFrame` already contains H.264/H.265 access units extracted
    /// from the Reolink protocol. The data entering appsrc is therefore
    /// elementary H.264/H.265, not RTP, so rtph264depay/rtph265depay must
    /// not be inserted here.
    ///
    /// PTS is computed per-frame from the camera's own capture clock
    /// (`frame.microseconds`), guarded against implausible jumps — see
    /// `VideoSink::push_frame`'s doc comment. Arrival-time pacing
    /// (`do-timestamp`) was tried 2026-09-15/16 and replaced 2026-09-16
    /// with this, following a sibling Reolink client's (`leolink`) own
    /// arrival at the same conclusion for the same protocol.
    ///
    /// decodebin is used so GStreamer can select the best available decoder
    /// for the current system. Depending on the installed plugins and their
    /// ranks this can be a hardware decoder or a software decoder.
    ///
    /// `queue` sits *before* decodebin (compressed access units), not
    /// after (decoded raw frames) — see its own doc comment below for why:
    /// a multi-second reserve of raw 4K frames is prohibitively large,
    /// the same reserve of compressed data is a few MB. decodebin's sink
    /// pad is always-present/static; its *output* is a dynamic pad, so
    /// decodebin -> videoconvert is connected in the pad-added callback.
    fn build_sink(&self, video_type: VideoType) -> VideoSink {
        let (parse_name, media_type) = match video_type {
            VideoType::H264 => ("h264parse", "video/x-h264"),
            VideoType::H265 => ("h265parse", "video/x-h265"),
        };

        let pipeline = gstreamer::Pipeline::new();

        /*
         * appsrc
         */
        let appsrc = gstreamer::ElementFactory::make("appsrc")
            .build()
            .expect("appsrc element missing — install gstreamer1.0-plugins-base")
            .downcast::<AppSrc>()
            .expect("appsrc is always an AppSrc");

        appsrc.set_caps(Some(
            &gstreamer::Caps::builder(media_type)
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        ));

        appsrc.set_is_live(true);
        appsrc.set_format(gstreamer::Format::Time);

        // No `do-timestamp`: PTS is computed manually in `push_frame` from
        // the camera's own capture clock (plausibility-guarded), not from
        // arrival time — see `Pipeline`'s doc comment.

        /*
         * h264parse / h265parse
         */
        let parse = gstreamer::ElementFactory::make(parse_name)
            .property_from_str("config-interval", "-1")
            .build()
            .unwrap_or_else(|_| {
                panic!(
                    "{parse_name} element missing — install the appropriate \
                     GStreamer parser plugin"
                )
            });

        /*
         * queue
         *
         * Placed BEFORE decodebin (holds compressed access units, not
         * decoded frames) so a multi-second reserve is cheap: at this
         * camera's ~8Mbit/s encoder rate, 3s of compressed data is only a
         * few MB. The same reserve placed after decodebin would hold raw
         * decoded 4K frames instead — ~12.4MB each at 3840x2160 YUV420, so
         * 3s at 25fps would be closer to 900MB. Not equivalent to
         * `rtspsrc latency=1000/3000`'s RTP jitterbuffer (that operates on
         * RTP packets before depayloading), but serves the same purpose
         * `pipeline.set_latency` below needs: real reserve to draw from
         * during a keyframe's transmission delay, without which a live
         * source with no buffering stalls outright instead of absorbing
         * it — confirmed 2026-09-16 on real hardware: removing this
         * reserve (this file previously had neither the queue sized for
         * it nor `pipeline.set_latency`) reproduced visible interruptions
         * on TCP too, a transport with no P2P/UDP-specific failure mode at
         * all, pointing squarely at the missing reserve rather than
         * anything transport-layer.
         */
        let queue = gstreamer::ElementFactory::make("queue")
            .property("max-size-buffers", 0u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 3_000_000_000u64) // 3s
            .property_from_str("leaky", "no")
            .build()
            .expect("queue element missing — install gstreamer1.0-plugins-base");

        /*
         * decodebin
         *
         * Decoder selection is intentionally automatic. This keeps the
         * player portable across NVIDIA, Intel, AMD and software-only
         * systems.
         */
        let decodebin = gstreamer::ElementFactory::make("decodebin")
            .property("force-sw-decoders", crate::ui::settings::force_software())
            .build()
            .expect("decodebin element missing — install gstreamer1.0-plugins-base");

        /*
         * videoconvert
         *
         * Keeps the output portable across the raw pixel formats produced
         * by whichever decoder decodebin selects.
         */
        let convert = gstreamer::ElementFactory::make("videoconvert")
            .build()
            .expect("videoconvert element missing — install gstreamer1.0-plugins-base");

        /*
         * GTK4 video sink
         */
        let sink = gstreamer::ElementFactory::make("gtk4paintablesink")
            .property("max-lateness", -1i64)
            .build()
            .expect(
                "gtk4paintablesink missing — install GStreamer GTK4 support",
            );

        /*
         * Add elements.
         */
        pipeline
            .add_many([
                appsrc.upcast_ref(),
                &parse,
                &queue,
                &decodebin,
                &convert,
                &sink,
            ])
            .expect("adding elements to pipeline failed");

        /*
         * Static part before decodebin: appsrc -> parse -> queue ->
         * decodebin's (always-present) sink pad.
         */
        appsrc
            .link(&parse)
            .expect("linking appsrc -> parser failed");

        parse
            .link(&queue)
            .expect("linking parser -> queue failed");

        queue
            .link(&decodebin)
            .expect("linking queue -> decodebin failed");

        /*
         * Static part after decodebin.
         */
        convert
            .link(&sink)
            .expect("linking videoconvert -> gtk4paintablesink failed");

        /*
         * decodebin has dynamic source pads.
         *
         * Only connect decoded raw video. If decodebin ever exposes another
         * stream type, ignore it.
         */
        let convert_weak = convert.downgrade();

        decodebin.connect_pad_added(move |_decodebin, src_pad| {
            let Some(convert) = convert_weak.upgrade() else {
                return;
            };

            let Some(convert_sink_pad) = convert.static_pad("sink") else {
                eprintln!("videoconvert has no sink pad");
                return;
            };

            // A decodebin pad-added callback can theoretically run more than
            // once. Do not attempt to link an already-linked videoconvert.
            if convert_sink_pad.is_linked() {
                return;
            }

            let caps = src_pad
                .current_caps()
                .unwrap_or_else(|| src_pad.query_caps(None));

            let Some(structure) = caps.structure(0) else {
                return;
            };

            if structure.name() != "video/x-raw" {
                return;
            }

            match src_pad.link(&convert_sink_pad) {
                Ok(_) => {}
                Err(err) => {
                    eprintln!(
                        "failed to link decodebin video output -> videoconvert: {err:?}"
                    );
                }
            }
        });

        /*
         * Connect gtk4paintablesink to GtkPicture.
         */
        let paintable =
            sink.property::<gtk4::gdk::Paintable>("paintable");

        self.picture.set_paintable(Some(&paintable));

        /*
         * Start playback.
         *
         * With a live source (`appsrc.set_is_live(true)`) and no element
         * in this chain reporting its own latency (plain `queue` doesn't,
         * unlike an RTP jitterbuffer), GStreamer's automatic live-pipeline
         * latency negotiation settles on ~0 — meaning `sync: true`
         * playback starts displaying data immediately with no reserve
         * built up first, so a keyframe's real transmission delay (which
         * doesn't shrink just because the queue moved before decodebin)
         * stalls the sink outright. `set_latency` here is what actually
         * tells the pipeline to hold live data for this long before
         * rendering, matching the `queue`'s own 3s capacity above so the
         * reserve it asks for can actually be held.
         */
        pipeline.set_latency(gstreamer::ClockTime::from_seconds(3));

        pipeline
            .set_state(gstreamer::State::Playing)
            .expect("failed to start GStreamer pipeline");

        VideoSink { appsrc, pipeline, pts_state: Arc::new(Mutex::new(PtsState::default())) }
    }

    /// Returns a `Send`-safe handle to this view's `appsrc`, building the
    /// pipeline first if this is the first call — building (and the
    /// `self.picture.set_paintable` call inside it) touches a GTK widget,
    /// so this method itself must still run on the GTK main thread. The
    /// returned `VideoSink`, unlike this method, does not need to be — see
    /// its own doc comment. Reconnecting reuses whatever pipeline already
    /// exists rather than rebuilding (matches this file's prior
    /// lazy-build-once behavior; a mid-session codec change was never
    /// supported before this either).
    /// Tears the pipeline down so the next `ensure_sink` builds a fresh one
    /// (a different device or stream can have another codec/resolution).
    pub fn reset(&self) {
        if let Some(sink) = self.sink.borrow_mut().take() {
            let _ = sink.pipeline.set_state(gstreamer::State::Null);
        }
        self.picture.set_paintable(None::<&gtk4::gdk::Paintable>);
    }

    pub fn ensure_sink(&self, video_type: VideoType) -> VideoSink {
        if self.sink.borrow().is_none() {
            let sink = self.build_sink(video_type);
            *self.sink.borrow_mut() = Some(sink);
        }
        self.sink.borrow().as_ref().expect("sink must exist after initialization").clone()
    }
}

impl VideoSink {
    /// Push one complete H.264/H.265 access unit into appsrc. Safe to call
    /// from any thread — see `VideoSink`'s own doc comment.
    ///
    /// PTS is computed from the camera's own capture clock
    /// (`frame.microseconds`, a u32 wrapping every ~71.58 minutes),
    /// ported from `leolink`'s `BcMediaParser::feed` (commit `8f0e61b`,
    /// `github.com/tombueng/leolink`, 2026-09-14 — read-only reference,
    /// never copied code from, only the algorithm):
    ///
    /// - `step = frame.microseconds.wrapping_sub(last_raw)`, unwrapping the
    ///   32-bit counter correctly across a single wrap (the same trick
    ///   this file's previous camera-clock-PTS system used).
    /// - If `step` is implausible (`0`, meaning a duplicate/non-advancing
    ///   reading, or bigger than `MAX_PLAUSIBLE_STEP_US`, meaning the
    ///   camera-side clock-reporting artifact this project spent real
    ///   effort characterizing — see
    ///   [[project-video-stutter-investigation]]), the accumulated
    ///   presentation clock advances by the *last known-good* step instead
    ///   (or `DEFAULT_STEP_US` if none is known yet), not by the
    ///   implausible value and not by resyncing to wall-clock time.
    /// - `last_raw_time` is updated to the true raw value every frame
    ///   regardless of whether its step was trusted, so the *next* frame's
    ///   step is still measured against reality and can recover a
    ///   plausible reading immediately once the camera's own numbers make
    ///   sense again — a single bad reading doesn't permanently skew
    ///   anything.
    ///
    /// This deliberately does NOT reproduce this file's previous
    /// camera-clock-PTS system (deleted 2026-09-15, see git history): that
    /// system resynced its anchor to the pipeline's real running time
    /// whenever drift in *either* direction exceeded a threshold, which
    /// needed two thresholds fighting each other and never converged on
    /// real hardware. This is simpler — one plausibility check, one
    /// fallback value, no anchor, no resync, no wall-clock reference at
    /// all — because leolink's own commit message frames the problem the
    /// same way this project independently arrived at: the presentation
    /// clock should advance at the camera's own (trusted) cadence, and an
    /// untrustworthy reading should be smoothed over, not chased.
    pub fn push_frame(&self, frame: &VideoFrame) {
        let mut state = self.pts_state.lock().expect("pts_state mutex poisoned");

        if let Some(last_raw) = state.last_raw_time {
            let step_us = frame.microseconds.wrapping_sub(last_raw) as u64;
            let step_us = if step_us == 0 || step_us > MAX_PLAUSIBLE_STEP_US {
                if state.last_step_us > 0 {
                    state.last_step_us
                } else {
                    DEFAULT_STEP_US
                }
            } else {
                state.last_step_us = step_us;
                step_us
            };
            state.accumulated_us += step_us;
        }
        state.last_raw_time = Some(frame.microseconds);

        let pts = gstreamer::ClockTime::from_useconds(state.accumulated_us);

        // DEBUG-ONLY, temporary: real wall-clock gap since the last pushed
        // frame (how fast data is actually arriving) alongside the computed
        // PTS and the pipeline's own running time (how far sync:true thinks
        // it should be), plus frame size and keyframe flag — to tell a real
        // network-throughput shortfall apart from a client-side bug without
        // guessing. Remove once this session's investigation concludes.
        if std::env::var("REOLING_DEBUG_PTS").is_ok() {
            let now = std::time::Instant::now();
            let gap = state.last_push_wall.map(|prev| now.duration_since(prev));
            state.last_push_wall = Some(now);
            let running_time = self.pipeline.current_running_time();
            eprintln!(
                "DEBUG push bytes={} keyframe={} pts={pts:?} running_time={running_time:?} wall_gap={gap:?}",
                frame.data.len(),
                frame.is_keyframe,
            );
        }
        drop(state);

        /*
         * VideoFrame::data already represents one complete access unit.
         */
        let mut buffer = gstreamer::Buffer::from_slice(frame.data.clone());
        buffer.get_mut().unwrap().set_pts(pts);

        match self.appsrc.push_buffer(buffer) {
            // The pipeline was torn down (stream stopped or replaced) while
            // frames were still on their way: expected, not worth a message.
            Ok(_) | Err(gstreamer::FlowError::Flushing) => {}
            Err(err) => eprintln!("failed to push video buffer into appsrc: {err:?}"),
        }
    }
}