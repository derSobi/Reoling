use gstreamer::prelude::*;
use gstreamer_app::AppSrc;
use gtk4::Picture;
use reoling::{VideoFrame, VideoType};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    /// The latency anchor (on unless `REOLING_NO_PTS_ANCHOR` is set): the
    /// smallest `running_time - camera_time` seen so far, in µs — see
    /// `push_frame`.
    anchor_us: Option<i64>,
    /// The last PTS handed to the decoding pipeline, kept strictly rising.
    last_display_pts_us: Option<u64>,
    /// When the next `REOLING_DEBUG_LATENCY` summary line is due.
    next_latency_report: Option<std::time::Instant>,
    /// The even display clock (µs) and the average frame interval it
    /// advances by — see `push_frame`.
    even_us: Option<f64>,
    average_step_us: Option<f64>,
    /// Frames pushed into the pipeline so far (for that summary line).
    pushed_frames: u64,
}

/// Whether the latency anchor is on: the default since 2026-10-11, when a
/// side-by-side run on real hardware (Home Hub channel, Balanced = 1 s)
/// showed the picture stuck 2.6 s behind after one network hiccup without
/// it, and back at ~0.95 s within a second with it.
/// `REOLING_NO_PTS_ANCHOR` turns it off, for comparison.
fn anchor_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("REOLING_NO_PTS_ANCHOR").is_err();
        if std::env::var("REOLING_DEBUG_LATENCY").is_ok() {
            eprintln!("video timing: camera cadence, latency anchor {}", if on { "on" } else { "off (REOLING_NO_PTS_ANCHOR)" });
        }
        on
    })
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
    /// The file being recorded to, if any; fed by the same `push_frame`.
    recorder: Arc<Mutex<Option<Recorder>>>,
    video_type: VideoType,
    /// Frames out of the decoder so far (`REOLING_DEBUG_LATENCY` only).
    decoded_frames: Arc<std::sync::atomic::AtomicU64>,
    /// Set on a replay sink (recorded footage, see `VideoView::new_replay`);
    /// `None` on a live one, whose timing this never touches.
    replay: Option<Arc<Replay>>,
}

/// Writes the incoming H.264/H.265 units into a Matroska file (which stays
/// playable even if the app dies mid-recording).
struct Recorder {
    appsrc: AppSrc,
    pipeline: gstreamer::Pipeline,
    /// Recording begins at a keyframe: nothing is playable before one.
    waiting_for_keyframe: bool,
    first_pts: Option<gstreamer::ClockTime>,
}

impl Recorder {
    fn start(path: &std::path::Path, video_type: VideoType) -> Result<Self, String> {
        let (parse, media) = match video_type {
            VideoType::H264 => ("h264parse", "video/x-h264"),
            VideoType::H265 => ("h265parse", "video/x-h265"),
        };
        let make = |name: &str| {
            gstreamer::ElementFactory::make(name)
                .build()
                .map_err(|e| format!("{name} is missing: {e}"))
        };
        let appsrc = make("appsrc")?
            .downcast::<AppSrc>()
            .map_err(|_| "appsrc is not an AppSrc".to_string())?;
        appsrc.set_caps(Some(
            &gstreamer::Caps::builder(media)
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        ));
        appsrc.set_format(gstreamer::Format::Time);
        let parser = gstreamer::ElementFactory::make(parse)
            .property_from_str("config-interval", "-1")
            .build()
            .map_err(|e| format!("{parse} is missing: {e}"))?;
        let mux = make("matroskamux")?;
        let file = gstreamer::ElementFactory::make("filesink")
            .property("location", path.to_string_lossy().to_string())
            .build()
            .map_err(|e| format!("filesink is missing: {e}"))?;
        let pipeline = gstreamer::Pipeline::new();
        pipeline
            .add_many([appsrc.upcast_ref(), &parser, &mux, &file])
            .map_err(|e| e.to_string())?;
        gstreamer::Element::link_many([appsrc.upcast_ref(), &parser, &mux, &file])
            .map_err(|e| e.to_string())?;
        pipeline.set_state(gstreamer::State::Playing).map_err(|e| e.to_string())?;
        Ok(Self { appsrc, pipeline, waiting_for_keyframe: true, first_pts: None })
    }

    fn push(&mut self, frame: &VideoFrame, pts: gstreamer::ClockTime) {
        if self.waiting_for_keyframe {
            if !frame.is_keyframe {
                return;
            }
            self.waiting_for_keyframe = false;
        }
        let first = *self.first_pts.get_or_insert(pts);
        let at = pts.saturating_sub(first);
        let mut buffer = gstreamer::Buffer::from_slice(frame.data.clone());
        {
            let b = buffer.get_mut().expect("new buffer");
            b.set_pts(at);
            b.set_dts(at);
        }
        let _ = self.appsrc.push_buffer(buffer);
    }

    /// Finishes the file. Waiting for the muxer to write its end takes a
    /// moment, so it happens on its own thread.
    fn finish(self) {
        std::thread::spawn(move || {
            let _ = self.appsrc.end_of_stream();
            if let Some(bus) = self.pipeline.bus() {
                let _ = bus.timed_pop_filtered(
                    gstreamer::ClockTime::from_seconds(5),
                    &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
                );
            }
            let _ = self.pipeline.set_state(gstreamer::State::Null);
        });
    }
}

pub struct VideoView {
    picture: Picture,

    // The codec (H.264 vs H.265) is only known once the first frame
    // arrives, so the pipeline is built lazily on the first ensure_sink()
    // call.
    sink: RefCell<Option<VideoSink>>,

    /// Whether `ensure_sink` builds replay sinks — see `new_replay`.
    replay: bool,
    /// The volume replay sinks play at (the window's volume setting).
    replay_volume: std::cell::Cell<f64>,
    /// Building a replay sink's sound failed once; said once.
    replay_audio_unavailable: std::cell::Cell<bool>,
}

impl VideoView {
    pub fn new() -> Self {
        Self {
            picture: Picture::new(),
            sink: RefCell::new(None),
            replay: false,
            replay_volume: std::cell::Cell::new(1.0),
            replay_audio_unavailable: std::cell::Cell::new(false),
        }
    }

    /// A view for recorded footage: every sink it builds is a replay sink
    /// (see `VideoSink::reset_for_open`), never a live one, playing sound at
    /// `volume` (0 to 1, see `set_volume`).
    pub fn new_replay(volume: f64) -> Self {
        Self { replay: true, replay_volume: std::cell::Cell::new(volume), ..Self::new() }
    }

    /// The volume of recorded footage's sound, now and for the sinks built
    /// from here on (0 is silence). Nothing on a live view, whose sound is
    /// `audio::AudioOutput`'s.
    pub fn set_volume(&self, volume: f64) {
        self.replay_volume.set(volume);
        if let Some(sink) = self.sink.borrow().as_ref() {
            sink.set_replay_volume(volume);
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
    /// PTS follows the camera's own capture clock, anchored to the
    /// pipeline's running time so delay can't pile up (see `push_frame`);
    /// the cadence itself comes from the camera clock
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
        // `qos=false`: the display sink reports every frame that reaches it
        // late, and videoconvert (QoS on by default) then silently discards
        // the next ones — uncounted anywhere. With sub's H.264 the decoder
        // legitimately holds frames longer than the pipeline latency, so
        // this cut sub down to ~1 frame per second (2026-10-11: reproduced
        // offline with the same frames, 24 of 226 shown; all shown with
        // videoconvert's QoS off). The sink still shows late frames
        // (`max-lateness = -1`); nothing between decoder and sink may
        // throw them away.
        let convert = gstreamer::ElementFactory::make("videoconvert")
            .property("qos", false)
            .build()
            .expect("videoconvert element missing — install gstreamer1.0-plugins-base");

        /*
         * GTK4 video sink
         */
        let sink = gstreamer::ElementFactory::make("gtk4paintablesink")
            .name("display")
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

        let decoded_frames = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let decoded_for_pad = decoded_frames.clone();
        decodebin.connect_pad_added(move |_decodebin, src_pad| {
            let decoded_frames = &decoded_for_pad;
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

            // `REOLING_DEBUG_LATENCY`: count what leaves the decoder, and
            // show what the display negotiated, to tell decoder losses from
            // display losses.
            if std::env::var("REOLING_DEBUG_LATENCY").is_ok() {
                let decoded = decoded_frames.clone();
                src_pad.add_probe(gstreamer::PadProbeType::BUFFER, move |_, _| {
                    decoded.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    gstreamer::PadProbeReturn::Ok
                });
                src_pad.connect_notify(Some("caps"), |pad, _| {
                    if let Some(caps) = pad.current_caps() {
                        eprintln!("LATENCY decoder output caps: {caps}");
                    }
                });
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

        // `REOLING_DEBUG_DECODER`: which decoder decodebin ended up with.
        if std::env::var("REOLING_DEBUG_DECODER").is_ok() {
            pipeline.connect_deep_element_added(|_, _, element| {
                if let Some(factory) = element.factory() {
                    let klass = factory.klass();
                    if klass.contains("Decoder") && klass.contains("Video") {
                        eprintln!("DECODER {} ({klass})", factory.name());
                    }
                }
            });
        }

        /*
         * Whatever video decoder decodebin picks: never discard late frames,
         * and hand each picture out as soon as it is decoded.
         *
         * These cameras' H.264 streams carry no VUI, so a decoder can't know
         * they never reorder frames (they have no B-frames) and holds up to
         * 16 decoded pictures before releasing any. That costs 0.64 s at
         * main's 25 fps but 1.6 s at sub's 10 fps — longer than the pipeline
         * latency, so nearly every frame reached the display late and the
         * decoder's QoS threw it away: measured 2026-10-11 on real hardware,
         * sub showed ~1 frame per second out of ~10 received, with every
         * decoder tried (NVIDIA and Intel), while the same frames dumped to a
         * file decoded completely offline.
         *
         * `compliance=flexible` (decoders built on GStreamer's H.264 base
         * class: va, nv, v4l2) drops that 16-frame hold — 0.64 s down to
         * 0-40 ms reported on the dumped sub stream, all 226 frames out, in
         * order. `qos=false` covers every other decoder (qsv, libav): a
         * late frame is still shown instead of being discarded.
         */
        pipeline.connect_deep_element_added(|_, _, element| {
            let Some(factory) = element.factory() else { return };
            let klass = factory.klass();
            if !(klass.contains("Decoder") && klass.contains("Video")) {
                return;
            }
            crate::ui::settings::note_decoder(&factory, false);
            if element.find_property("qos").is_some() {
                element.set_property("qos", false);
            }
            if element.find_property("compliance").is_some() {
                element.set_property_from_str("compliance", "flexible");
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
        pipeline.set_latency(gstreamer::ClockTime::from_mseconds(
            crate::ui::settings::latency().as_millis() as u64,
        ));

        pipeline
            .set_state(gstreamer::State::Playing)
            .expect("failed to start GStreamer pipeline");

        VideoSink {
            appsrc,
            pipeline,
            pts_state: Arc::new(Mutex::new(PtsState::default())),
            recorder: Arc::new(Mutex::new(None)),
            video_type,
            decoded_frames,
            replay: None,
        }
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
        let old = self.sink.borrow_mut().take();
        self.picture.set_paintable(None::<&gtk4::gdk::Paintable>);
        if let Some(sink) = old {
            sink.stop_recording();
            // Off the GTK thread: taking a pipeline down waits for its
            // streaming thread, which may itself be waiting for this thread
            // (the paintable sink hands frames over through the main loop) —
            // done here, that is a freeze.
            tear_down(sink);
        }
    }

    /// Freezes the picture where it is, at once: what the pipeline still
    /// holds (seconds of buffered video) is not shown. `reset` (or a new
    /// stream) takes the pipeline down later.
    pub fn freeze(&self) {
        if let Some(sink) = self.sink.borrow().as_ref() {
            let pipeline = sink.pipeline.clone();
            let replay = sink.replay.clone();
            // Off the GTK thread, like the teardown in `reset`. A replay
            // pipeline torn down meanwhile stays down (see `Replay::retired`).
            std::thread::spawn(move || {
                let _control = replay.as_ref().map(|replay| replay.control.lock().expect("replay control mutex poisoned"));
                if replay.as_ref().is_some_and(|replay| replay.retired.load(Ordering::SeqCst)) {
                    return;
                }
                let _ = pipeline.set_state(gstreamer::State::Paused);
            });
        }
    }

    /// The pipeline's handle, if a stream has built one.
    pub fn current_sink(&self) -> Option<VideoSink> {
        self.sink.borrow().clone()
    }

    /// The picture on screen right now, as a texture.
    pub fn snapshot(&self) -> Option<gtk4::gdk::Texture> {
        use gtk4::prelude::*;
        let paintable = self.picture.paintable()?;
        let (width, height) = (paintable.intrinsic_width(), paintable.intrinsic_height());
        if width <= 0 || height <= 0 {
            return None;
        }
        let snapshot = gtk4::Snapshot::new();
        paintable.snapshot(snapshot.upcast_ref::<gtk4::gdk::Snapshot>(), f64::from(width), f64::from(height));
        let node = snapshot.to_node()?;
        let renderer = self.picture.native()?.renderer()?;
        Some(renderer.render_texture(&node, None))
    }

    pub fn ensure_sink(&self, video_type: VideoType) -> VideoSink {
        if self.sink.borrow().is_none() {
            let sink = if self.replay { self.build_replay_sink(video_type) } else { self.build_sink(video_type) };
            *self.sink.borrow_mut() = Some(sink);
        }
        self.sink.borrow().as_ref().expect("sink must exist after initialization").clone()
    }

    /// `ensure_sink` for a view whose stream can change codec on the way
    /// (recorded footage: the main stream may be H.265 where the sub stream
    /// is H.264): a sink built for another codec is torn down first and a
    /// new one built. GTK main thread only, like `ensure_sink`.
    pub fn ensure_sink_for(&self, video_type: VideoType) -> VideoSink {
        let current = self.sink.borrow().as_ref().map(|sink| sink.video_type);
        if needs_rebuild(current, video_type) {
            self.reset();
        }
        self.ensure_sink(video_type)
    }
}

/// Takes a sink's pipeline to NULL on a thread of its own (see
/// `VideoView::reset`). A replay sink is marked retired first, on the
/// calling thread, so no clone of it held elsewhere can start the pipeline
/// again (see `Replay::retired`).
fn tear_down(sink: VideoSink) -> std::thread::JoinHandle<()> {
    if let Some(replay) = &sink.replay {
        replay.retired.store(true, Ordering::SeqCst);
    }
    let VideoSink { pipeline, replay, .. } = sink;
    std::thread::spawn(move || {
        let _control = replay.as_ref().map(|replay| replay.control.lock().expect("replay control mutex poisoned"));
        let _ = pipeline.set_state(gstreamer::State::Null);
    })
}

/// Whether a view holding a sink for codec `current` (`None`: no sink) must
/// build a new one for `wanted`.
fn needs_rebuild(current: Option<VideoType>, wanted: VideoType) -> bool {
    current.is_some_and(|current| current != wanted)
}

impl VideoSink {
    /// Starts recording what this sink shows to `path`.
    pub fn start_recording(&self, path: &std::path::Path) -> Result<(), String> {
        let recorder = Recorder::start(path, self.video_type)?;
        *self.recorder.lock().expect("recorder mutex poisoned") = Some(recorder);
        Ok(())
    }

    /// Ends the recording, if one is running. Returns whether one was.
    pub fn stop_recording(&self) -> bool {
        let recorder = self.recorder.lock().expect("recorder mutex poisoned").take();
        let was = recorder.is_some();
        if let Some(recorder) = recorder {
            recorder.finish();
        }
        was
    }

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
        if let Some(replay) = &self.replay {
            self.push_replay_frame(replay, frame);
            return;
        }
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
            // An even display clock locked to the camera's: the next frame
            // is due one *average* interval after the previous one, nudged
            // a tenth of the way towards the camera's own time so it never
            // drifts away from it. The camera stamps frames on its 40 ms
            // (25 fps) sensor grid, so a 10 fps sub stream arrives as 80,
            // 120, 80, 120 ms… — shown as such on a 60 Hz screen that is 5
            // then 7 refreshes per picture, a visible stutter main's steady
            // 40 ms never has. On the real sub timestamps (2026-10-11) this
            // gives 100-104 ms steps, never more than 84 ms off the camera.
            let average = state.average_step_us.map_or(step_us as f64, |a| a + (step_us as f64 - a) * 0.05);
            state.average_step_us = Some(average);
            let camera = state.accumulated_us as f64;
            let predicted = state.even_us.map_or(camera, |even| even + average);
            state.even_us = Some(predicted + (camera - predicted) * 0.1);
        } else {
            state.even_us = Some(state.accumulated_us as f64);
        }
        state.last_raw_time = Some(frame.microseconds);

        let pts = gstreamer::ClockTime::from_useconds(state.accumulated_us);
        state.pushed_frames += 1;
        // What the display pipeline gets. By default the camera-cadence PTS
        // above, which starts at 0 for the first frame: whatever the camera
        // sends in a burst when a stream starts (its buffered seconds), and
        // every network stall after that, stays in the picture as delay for
        // good, since the pipeline plays it all back at the camera's pace.
        //
        // With the anchor (the default), each frame is placed at `camera_time + anchor`,
        // where `anchor` is the smallest `running_time - camera_time` seen
        // so far: the frame that arrived earliest relative to its own
        // timestamp sets the pace, everything later than it uses the
        // pipeline latency as its jitter margin. The anchor only ever moves
        // earlier, so a startup burst plays through quickly once and the
        // picture then stays at the configured latency; nothing ever jumps
        // back. Kept strictly rising for the decoder.
        let display_pts = if anchor_enabled() {
            let camera_us = state.even_us.map_or(state.accumulated_us as i64, |even| even.round() as i64);
            if let Some(running) = self.pipeline.current_running_time().filter(|t| t.useconds() > 0) {
                let offset = running.useconds() as i64 - camera_us;
                state.anchor_us = Some(state.anchor_us.map_or(offset, |a| a.min(offset)));
            }
            let wanted = state.anchor_us.map_or(camera_us, |a| camera_us + a).max(0) as u64;
            let rising = state.last_display_pts_us.map_or(wanted, |last| wanted.max(last + 1));
            state.last_display_pts_us = Some(rising);
            gstreamer::ClockTime::from_useconds(rising)
        } else {
            pts
        };
        if std::env::var("REOLING_DEBUG_LATENCY").is_ok() {
            let now = std::time::Instant::now();
            if state.next_latency_report.is_none_or(|due| now >= due) {
                state.next_latency_report = Some(now + std::time::Duration::from_secs(1));
                if let Some(running) = self.pipeline.current_running_time() {
                    // How far ahead of "now" this frame is scheduled: the
                    // picture's delay behind the network, configured
                    // latency included.
                    let ahead_ms = (display_pts.useconds() as i64 + crate::ui::settings::latency().as_micros() as i64
                        - running.useconds() as i64)
                        / 1000;
                    // What the display actually did since the start:
                    // frames it rendered and frames it threw away (late or
                    // QoS), next to how many we pushed in.
                    let shown = self
                        .pipeline
                        .by_name("display")
                        .map(|sink| sink.property::<gstreamer::Structure>("stats"))
                        .map(|stats| {
                            format!(
                                "rendered {} dropped {}",
                                stats.get::<u64>("rendered").unwrap_or(0),
                                stats.get::<u64>("dropped").unwrap_or(0)
                            )
                        })
                        .unwrap_or_default();
                    eprintln!(
                        "LATENCY frame shown {ahead_ms} ms after it arrived (configured {} ms, anchor {}); pushed {} decoded {} {shown}",
                        crate::ui::settings::latency().as_millis(),
                        if anchor_enabled() { "on" } else { "off" },
                        state.pushed_frames,
                        self.decoded_frames.load(std::sync::atomic::Ordering::Relaxed)
                    );
                }
            }
        }

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

        if let Some(recorder) = self.recorder.lock().expect("recorder mutex poisoned").as_mut() {
            recorder.push(frame, pts);
        }

        // `REOLING_DUMP_VIDEO=<file>`: append every access unit exactly as
        // handed to the parser (H.264/H.265 byte-stream), plus a sidecar
        // `<file>.idx` line per unit — for checking a stream offline.
        if let Ok(path) = std::env::var("REOLING_DUMP_VIDEO") {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                let _ = f.write_all(&frame.data);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(format!("{path}.idx")) {
                let _ = writeln!(f, "{} {} {}", frame.data.len(), frame.is_keyframe, frame.microseconds);
            }
        }

        /*
         * VideoFrame::data already represents one complete access unit.
         */
        let mut buffer = gstreamer::Buffer::from_slice(frame.data.clone());
        buffer.get_mut().unwrap().set_pts(display_pts);

        match self.appsrc.push_buffer(buffer) {
            // The pipeline was torn down (stream stopped or replaced) while
            // frames were still on their way: expected, not worth a message.
            Ok(_) | Err(gstreamer::FlowError::Flushing) => {}
            Err(err) => eprintln!("failed to push video buffer into appsrc: {err:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Replay: recorded footage played back from a device
//
// Replay data are not live. After every open the device first sends a lead
// of about 4 s of wall time (2-10 s of footage at 1x, 48-64 s at 16x), then
// footage at exactly the requested speed: every frame (plus audio) up to 8x,
// I-frames only at 16x. Below 1x (0.5x, 0.25x) it sends at 1x and only the
// picture is slowed down. So a replay sink:
// - runs a non-live pipeline whose clock starts with the first frame shown,
//   with PTS = footage time / speed. Neither the latency anchor nor the even
//   display clock is used: the anchor would speed playback up to the rate
//   data arrive at (ahead of time, on purpose), and the frames' own cadence
//   is what replay shows;
// - holds the whole lead in front of the decoder without ever making the
//   producer wait;
// - starts its timeline again on every open (seek, speed change, resume), or
//   carries it on across the hop to the next file;
// - plays the footage's sound at 1x through a second source in the same
//   pipeline, so the picture and the sound share one clock and one
//   timeline (see `ReplayClock::stamp_audio` for how an audio unit is
//   timed), and stays silent at every other speed.
//
// How the playback driver (`playback_driver.rs`, on the device's network
// thread) drives it:
// - `VideoView::new_replay()` for the Playback page's picture;
//   `ensure_sink_for` through its own GTK-thread responder, once the codec
//   is known (a codec change builds a new pipeline).
// - Before the first frame of each open, on the pushing thread:
//   `reset_for_open(seek, speed)` (new position, speed or stream change) or
//   `chain_open(seek)` (the next file after an end of file, or the same
//   file resumed after a pause, a stall or a slow-motion backlog, carrying
//   on from what is buffered).
// - `push_frame` for every video frame, `push_replay_audio` for every audio
//   unit after it (at 1x; the sink plays nothing at any other speed).
// - Below 1x the device still sends at 1x, so footage piles up: once
//   `replay_status()` reports `slow_motion_backlog`, close the device's
//   playback and open it again later (see that field).
// - Pause: `set_paused(true)`; play on with `set_paused(false)`, then
//   `chain_open` for the resumed opening.
// - After the last file: `end_of_footage()`, so the decoder lets go of
//   the last frames.
// - `replay_status()` (any thread, e.g. a GTK timer) for the timeline cursor
//   and a buffering figure.
// ---------------------------------------------------------------------------

/// Replay's version of `MAX_PLAUSIBLE_STEP_US`. At 16x only I-frames arrive,
/// 3.8-4.2 s of footage apart on the sub streams (4.24 s the largest step
/// measured), too close to the live 5 s cap for a longer GOP.
const REPLAY_MAX_PLAUSIBLE_STEP_US: u64 = 10_000_000;

/// From this speed on, delta frames are dropped in front of the decoder when
/// it falls behind: at 8x a 10 fps sub stream is 80 frames per second of
/// playback, a 25 fps main stream 200. (16x brings I-frames only.)
const REPLAY_THIN_FROM_SPEED: f64 = 8.0;

/// How late (pipeline running time past its PTS) a delta frame may reach the
/// decoder before the rest of its GOP is dropped.
const REPLAY_LATE_LIMIT_US: u64 = 200_000;

/// The most compressed data a replay sink queues in front of the decoder.
/// The device's lead is a few MB on sub streams; on main at 16x it could be
/// ~40-55 MB (60 s of 1-2 s GOPs at ~1 MB per I-frame, an estimate). Past
/// this cap frames are left out until a key frame fits again, rather than
/// letting memory grow without bound.
const REPLAY_MAX_QUEUED_BYTES: u64 = 128 * 1024 * 1024;

/// A replay frame that arrives after its due time (the network stalled and
/// the lead ran out) moves the rest of the timeline later, by at most this
/// much per frame (a longer stall is caught up over the next frame or two),
/// so playback resumes at speed instead of rushing through what arrives
/// late. Never earlier: that would pull playback towards the arrival rate.
const REPLAY_MAX_REBASE_US: u64 = 30_000_000;

/// Below 1x: the footage a replay sink may hold before it asks for the
/// device's playback to be closed (`ReplaySinkStatus::slow_motion_backlog`).
/// About 80 MB at the highest main-stream rates seen (1.3 MB/s), well under
/// `REPLAY_MAX_QUEUED_BYTES`.
const REPLAY_SLOW_MOTION_MAX_BUFFERED: Duration = Duration::from_secs(60);

/// Frames remembered (pushed, not yet shown) for mapping the PTS on screen
/// back to footage time; far more than any lead holds.
const REPLAY_MAX_MARKS: usize = 20_000;

/// An audio unit whose ADTS header cannot be read is taken to be this long:
/// 1024 samples at 16 kHz, what these devices send (about 16 units per
/// footage second at 1x to 8x).
const AUDIO_UNIT_FALLBACK_US: u64 = 64_000;

/// The most compressed audio a replay sink queues (minutes at the 66 kbit/s
/// seen); past it units are left out.
const REPLAY_AUDIO_MAX_QUEUED_BYTES: u64 = 16 * 1024 * 1024;

/// Replay audio runs on its own units' durations, one unit starting where
/// the one before ended, so the decoder and the output see a contiguous
/// stream (timestamps that jump by a few tens of ms at every frame make them
/// mark discontinuities and the output resync, audibly). The picture's clock
/// is only the anchor: the next unit is placed at the latest frame's footage
/// time again once its own count has run this far from it (the devices'
/// unit count stays within about 150 ms of the frames' timestamps over
/// ten-minute openings, so this is for a hop, a lost unit or a device that
/// counts differently; a unit or two after a frame is the normal lead).
const AUDIO_REANCHOR_US: u64 = 250_000;

/// A replay sink plays only once both its picture and its sound have
/// something (a first unit, or the end of their stream) after an open:
/// once this many video frames at 1x have come without a single audio unit,
/// or `AUDIO_WAIT` has passed since the first one, the sound is told its
/// stream ended so that the picture plays; the sound starts again should
/// audio come after all. The devices send audio right after the frame it
/// belongs to, one or two units per frame, so the wait is normally a frame
/// or two; a recording without sound waits this many frames (part of the
/// opening's first burst) or, should they trickle in, `AUDIO_WAIT`.
const AUDIO_WAIT_FRAMES: u64 = 10;
const AUDIO_WAIT: gstreamer::ClockTime = gstreamer::ClockTime::from_mseconds(400);

/// "No frame has reached the display since the last reset."
const NO_PTS: u64 = u64::MAX;

/// Where a replay sink stands: see `VideoSink::replay_status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplaySinkStatus {
    /// The footage on screen, on the caller's own timeline: the `seek` given
    /// for the open it belongs to, plus the footage since that open's first
    /// frame. Until a frame is shown, the seek point itself.
    pub position: Duration,
    /// Footage received but not shown yet (the device's lead).
    pub buffered: Duration,
    /// Compressed bytes waiting in front of the decoder.
    pub buffered_bytes: u64,
    /// Frames not shown since the last `reset_for_open`: delta frames
    /// dropped because the decoder fell behind (8x and faster), and frames
    /// left out over the memory cap.
    pub dropped_frames: u64,
    /// Below 1x only: `buffered` has reached
    /// `REPLAY_SLOW_MOTION_MAX_BUFFERED`. The device sends at 1x whatever
    /// the speed below it, so the backlog grows by (1 - speed) s every
    /// second and would end up over the memory cap, where frames are left
    /// out. The driver then closes the device's playback (the sink plays
    /// on from what it holds, the frames still in the stream included) and,
    /// once `buffered` is down to a few seconds, opens it again with the
    /// client's `resume_replay` at `ReplayStatus::resume_point`, continuing
    /// with `chain_open`. The device starts at a key frame from about 1.1 s
    /// before a seek to about one GOP after it (observed); the resume point
    /// is the latest key frame's own time, and `resume_replay` drops the
    /// frames that opening repeats: nothing is shown twice and nothing is
    /// left out.
    pub slow_motion_backlog: bool,
}

/// A speed the replay clock can work with: 1/16x to 64x, anything
/// meaningless (0, negative, NaN) is 1x.
fn sanitize_speed(speed: f64) -> f64 {
    if speed.is_finite() && speed > 0.0 {
        speed.clamp(1.0 / 16.0, 64.0)
    } else {
        1.0
    }
}

fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX).min(u64::MAX / 4)
}

/// One frame handed to the pipeline, kept until it has been shown (all µs).
#[derive(Clone, Copy, Debug)]
struct Mark {
    pts_us: u64,
    /// Footage before it since the last reset.
    footage_us: u64,
    /// Its place on the caller's timeline.
    position_us: u64,
}

/// What `ReplayClock::stamp` gives a frame (µs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    /// For the display pipeline.
    pts_us: u64,
    /// For the recorder: footage time, rising across opens.
    recording_us: u64,
}

/// The replay timeline, as plain data.
///
/// - Footage time accumulates from the frames' camera timestamps (`u32` µs,
///   wrapping every ~71.6 min): each step is `raw.wrapping_sub(last_raw)`,
///   right across a wrap. A step of 0, or above
///   `REPLAY_MAX_PLAUSIBLE_STEP_US` (a backwards jump looks like that too),
///   is not believed: the last good step stands in for it.
/// - PTS = footage / speed, plus `offset_us` once the lead has run out (see
///   `stamp`), kept strictly rising.
/// - `reset` starts again from PTS 0 (a new open, pipeline flushed); `chain`
///   carries on one frame interval after the last frame (the next file,
///   nothing flushed).
struct ReplayClock {
    speed: f64,
    last_raw: Option<u32>,
    last_step_us: u64,
    /// Footage since the first frame after the last reset.
    footage_us: u64,
    /// Where the next audio unit starts on the footage's timeline (`None`:
    /// no unit since the last reset; see `stamp_audio`).
    audio_next_us: Option<u64>,
    /// Audio units stamped since the last reset.
    audio_units: u64,
    /// The current open's seek point, on the caller's timeline.
    open_seek_us: u64,
    /// `footage_us` at the current open's first frame, once it came.
    open_footage_us: Option<u64>,
    /// Added to every PTS: how much later than planned playback runs after
    /// the lead ran out.
    offset_us: u64,
    last_pts_us: Option<u64>,
    /// Frames pushed and not known to be shown yet, oldest first (plus the
    /// one on screen, as reference).
    marks: std::collections::VecDeque<Mark>,
    recording_us: Option<u64>,
    /// Frames stamped since the last reset.
    frames: u64,
}

impl ReplayClock {
    fn new() -> Self {
        Self {
            speed: 1.0,
            last_raw: None,
            last_step_us: 0,
            footage_us: 0,
            audio_next_us: None,
            audio_units: 0,
            open_seek_us: 0,
            open_footage_us: None,
            offset_us: 0,
            last_pts_us: None,
            marks: std::collections::VecDeque::new(),
            recording_us: None,
            frames: 0,
        }
    }

    /// A new open at `seek` (caller's timeline) played at `speed`: the
    /// timeline starts again at PTS 0. Only the recording clock carries on.
    fn reset(&mut self, seek: Duration, speed: f64) {
        *self = Self {
            speed: sanitize_speed(speed),
            open_seek_us: duration_us(seek),
            recording_us: self.recording_us,
            ..Self::new()
        };
    }

    /// The next open carries straight on from this one: its first frame
    /// comes one frame interval after the last one, at `seek` on the
    /// caller's timeline. Its camera clock is a new one.
    fn chain(&mut self, seek: Duration) {
        self.last_raw = None;
        self.open_seek_us = duration_us(seek);
        self.open_footage_us = None;
    }

    /// Times the next frame from its camera timestamp `raw`. `now_us` is
    /// the pipeline's running time while it is playing (`None` otherwise):
    /// a frame due before it arrived late, after the lead ran out, and the
    /// timeline moves later so it is shown now and the ones after it at
    /// speed again.
    fn stamp(&mut self, raw: u32, now_us: Option<u64>) -> Stamp {
        let fallback = if self.last_step_us > 0 { self.last_step_us } else { DEFAULT_STEP_US };
        let step = match self.last_raw {
            Some(last) => {
                let step = raw.wrapping_sub(last) as u64;
                if step == 0 || step > REPLAY_MAX_PLAUSIBLE_STEP_US {
                    fallback
                } else {
                    self.last_step_us = step;
                    step
                }
            }
            // The first frame of a chained open: one frame after the last.
            None if self.last_pts_us.is_some() => fallback,
            None => 0,
        };
        self.last_raw = Some(raw);
        self.footage_us += step;
        let open_footage = *self.open_footage_us.get_or_insert(self.footage_us);

        let mut pts = (self.footage_us as f64 / self.speed).round() as u64 + self.offset_us;
        if let Some(late) = now_us.and_then(|now| now.checked_sub(pts)).filter(|&l| l > 0) {
            let step = late.min(REPLAY_MAX_REBASE_US);
            self.offset_us += step;
            pts += step;
        }
        if let Some(last) = self.last_pts_us {
            pts = pts.max(last + 1);
        }
        self.last_pts_us = Some(pts);

        self.marks.push_back(Mark {
            pts_us: pts,
            footage_us: self.footage_us,
            position_us: self.open_seek_us.saturating_add(self.footage_us - open_footage),
        });
        if self.marks.len() > REPLAY_MAX_MARKS {
            self.marks.pop_front();
        }
        let recording_us = self.recording_us.map_or(0, |r| r + if step > 0 { step } else { fallback });
        self.recording_us = Some(recording_us);
        self.frames += 1;
        Stamp { pts_us: pts, recording_us }
    }

    /// Times an audio unit of `duration_us` of footage. Audio is placed on
    /// the picture's own timeline, contiguously: the first unit after a
    /// reset starts at the latest video frame's footage time and each one
    /// after it where the one before ended, so the sound is one unbroken
    /// stream for the decoder and the output. The frames stay its anchor:
    /// once the units' own count has run `AUDIO_REANCHOR_US` from the latest
    /// frame's footage time, the next unit starts at that time again, so the
    /// sound follows the picture through every open, hop, stall and pause
    /// (its PTS carry the same speed and offset as the frames'). `None`
    /// before the first video frame since the reset: nothing to follow yet.
    fn stamp_audio(&mut self, duration_us: u64) -> Option<u64> {
        self.last_pts_us?;
        let footage = match self.audio_next_us {
            Some(next) if next.abs_diff(self.footage_us) <= AUDIO_REANCHOR_US => next,
            _ => self.footage_us,
        };
        self.audio_next_us = Some(footage + duration_us);
        self.audio_units += 1;
        Some((footage as f64 / self.speed).round() as u64 + self.offset_us)
    }

    /// Footage and timeline position (µs) of PTS `shown_us`: from the last
    /// frame at or before it, never past the next one.
    fn locate(&self, shown_us: u64) -> Option<(u64, u64)> {
        let after = self.marks.partition_point(|m| m.pts_us <= shown_us);
        let Some(mark) = after.checked_sub(1).and_then(|i| self.marks.get(i)) else {
            return self.marks.front().map(|m| (m.footage_us, m.position_us));
        };
        let ahead = match self.marks.get(after) {
            Some(next) => (((shown_us - mark.pts_us) as f64 * self.speed).round() as u64)
                .min(next.footage_us - mark.footage_us),
            None => 0,
        };
        Some((mark.footage_us + ahead, mark.position_us.saturating_add(ahead)))
    }

    /// The timeline position on screen at PTS `shown_us` (`None`: nothing
    /// shown since the reset, so the seek point).
    fn position(&self, shown_us: Option<u64>) -> Duration {
        let us = shown_us
            .and_then(|shown| self.locate(shown))
            .map(|(_, position)| position)
            .or_else(|| self.marks.front().map(|m| m.position_us))
            .unwrap_or(self.open_seek_us);
        Duration::from_micros(us)
    }

    /// Footage pushed but not on screen yet at PTS `shown_us`.
    fn buffered(&self, shown_us: Option<u64>) -> Duration {
        let Some(last) = self.marks.back() else { return Duration::ZERO };
        let shown_footage = shown_us
            .and_then(|shown| self.locate(shown))
            .map(|(footage, _)| footage)
            .or_else(|| self.marks.front().map(|m| m.footage_us))
            .unwrap_or(0);
        Duration::from_micros(last.footage_us.saturating_sub(shown_footage))
    }

    /// Forgets the frames before the one on screen at PTS `shown_us`.
    fn forget_shown(&mut self, shown_us: u64) {
        let after = self.marks.partition_point(|m| m.pts_us <= shown_us);
        if after > 1 {
            self.marks.drain(..after - 1);
        }
    }
}

/// The sound of a replay sink: its own source in the same pipeline as the
/// picture (`appsrc ! aacparse ! <AAC decoder> ! audioconvert !
/// audioresample ! volume ! autoaudiosink`).
struct ReplayAudio {
    appsrc: AppSrc,
    /// The volume element (`None` in tests, whose chain has none).
    volume: Option<gstreamer::Element>,
}

/// A replay sink's own state, shared by every clone of the sink.
struct Replay {
    state: Mutex<ReplayState>,
    probes: Arc<ReplayProbes>,
    /// The queue in front of the decoder (for `buffered_bytes`).
    queue: gstreamer::Element,
    /// The sound, if the pipeline has it.
    audio: Option<ReplayAudio>,
    /// The view has let go of this pipeline and takes it to NULL
    /// (`VideoView::reset`): from then on the replay controls do nothing,
    /// so the thread that pushes the frames, which may still hold a clone
    /// for a while, cannot start it again.
    retired: AtomicBool,
    /// Held across the `retired` check and the state change after it, and
    /// by the thread that takes a retired pipeline to NULL, so a teardown
    /// cannot come between the two. Never taken on the GTK main thread: a
    /// state change can wait for that thread.
    control: Mutex<()>,
}

struct ReplayState {
    clock: ReplayClock,
    /// Over `REPLAY_MAX_QUEUED_BYTES`: frames are left out until a key frame
    /// fits again.
    overflowing: bool,
    dropped_over_cap: u64,
    /// The PTS last reported as on screen, so the position never goes back.
    last_shown_us: Option<u64>,
    /// `end_of_footage` closed the source: nothing is queued until the next
    /// `reset_for_open`.
    ended: bool,
    /// The sound's stream was ended since the last reset (at a speed other
    /// than 1x, after `AUDIO_WAIT_FRAMES` frames without audio, or by
    /// `end_of_footage`); an audio unit arriving after that opens it again
    /// (see `push_replay_audio`).
    audio_ended: bool,
    /// At 1x, until the first audio unit or the end of the sound's stream
    /// after the last reset: the picture waits for one of them (see
    /// `AUDIO_WAIT_FRAMES`).
    audio_waiting: bool,
    /// Counts the resets, so that a wait for audio scheduled on the clock
    /// (see `AUDIO_WAIT`) ends the sound of its own open only.
    reset_gen: u64,
    /// When the next `REOLING_DEBUG_LATENCY` line is due.
    next_report: Option<std::time::Instant>,
}

impl Replay {
    /// The sound's stream is ended while the picture is waiting for it (see
    /// `AUDIO_WAIT_FRAMES`); `audio_waiting` is cleared. Under `state`'s lock.
    fn end_audio_wait(&self, state: &mut ReplayState) {
        if !state.audio_waiting {
            return;
        }
        state.audio_waiting = false;
        state.audio_ended = true;
        if let Some(audio) = &self.audio {
            let _ = audio.appsrc.end_of_stream();
        }
    }

    /// `AUDIO_WAIT` after the first frame of the open numbered `gen`: ends
    /// the wait if that open is still waiting (from the clock's thread).
    fn audio_wait_over(&self, gen: u64) {
        if self.retired.load(Ordering::SeqCst) {
            return;
        }
        let mut state = self.state.lock().expect("replay mutex poisoned");
        if state.reset_gen == gen {
            self.end_audio_wait(&mut state);
        }
    }
}

/// What the pipeline's streaming threads report and read, lock-free.
struct ReplayProbes {
    /// The highest PTS (µs) that reached the display, or was dropped in
    /// front of the decoder, since the last reset; `NO_PTS` if none. A
    /// dropped frame's time has passed too, so it moves the cursor and
    /// leaves the buffered footage.
    arrived_pts_us: AtomicU64,
    /// The speed is `REPLAY_THIN_FROM_SPEED` or more.
    thin_when_late: AtomicBool,
    /// The decoder fell behind: the rest of the current GOP is dropped.
    skipping: AtomicBool,
    dropped_late: AtomicU64,
}

impl ReplayProbes {
    fn arrived(&self, pts_us: u64) {
        let _ = self.arrived_pts_us.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            (current == NO_PTS || pts_us > current).then_some(pts_us)
        });
    }
}

/// The pipeline's running time (µs), but only while it is really playing:
/// while it pauses, or prerolls again after a flush, the running time does
/// not belong to the frames' PTS yet.
fn playing_running_time_us(pipeline: &gstreamer::Pipeline) -> Option<u64> {
    let (_, current, pending) = pipeline.state(Some(gstreamer::ClockTime::ZERO));
    let playing = current == gstreamer::State::Playing && pending == gstreamer::State::VoidPending;
    playing.then(|| pipeline.current_running_time()).flatten().map(|t| t.useconds())
}

/// The replay source: not live, so the pipeline clock starts with the first
/// frame shown instead of following arrival, and its own queue holds the
/// device's lead. It never blocks `push_frame`.
fn replay_appsrc(video_type: VideoType) -> AppSrc {
    let media_type = match video_type {
        VideoType::H264 => "video/x-h264",
        VideoType::H265 => "video/x-h265",
    };
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
    appsrc.set_is_live(false);
    appsrc.set_format(gstreamer::Format::Time);
    appsrc.set_block(false);
    // With `block` off this is no hard limit (past it the source only says
    // it has enough); `push_replay_frame` enforces it.
    appsrc.set_max_bytes(REPLAY_MAX_QUEUED_BYTES);
    appsrc
}

/// The sound's source: as `replay_appsrc`, for AAC (ADTS) units.
fn replay_audio_appsrc() -> AppSrc {
    let appsrc = gstreamer::ElementFactory::make("appsrc")
        .build()
        .expect("appsrc element missing — install gstreamer1.0-plugins-base")
        .downcast::<AppSrc>()
        .expect("appsrc is always an AppSrc");
    appsrc.set_caps(Some(
        &gstreamer::Caps::builder("audio/mpeg").field("mpegversion", 4i32).field("stream-format", "adts").build(),
    ));
    appsrc.set_is_live(false);
    appsrc.set_format(gstreamer::Format::Time);
    appsrc.set_block(false);
    appsrc.set_max_bytes(REPLAY_AUDIO_MAX_QUEUED_BYTES);
    appsrc
}

/// Builds the sound's chain into `pipeline` (see `ReplayAudio`), at
/// `volume`. `Err` names what is missing; the pipeline is then as it was.
fn build_replay_audio(pipeline: &gstreamer::Pipeline, volume: f64) -> Result<ReplayAudio, String> {
    let make = |name: &str| {
        gstreamer::ElementFactory::make(name)
            .build()
            .map_err(|e| format!("{name} is missing: {e}"))
    };
    let appsrc = replay_audio_appsrc();
    let parse = make("aacparse")?;
    let decoder = make("avdec_aac").or_else(|_| make("faad"))?;
    let convert = make("audioconvert")?;
    let resample = make("audioresample")?;
    let vol = make("volume")?;
    vol.set_property("volume", volume);
    let sink = make("autoaudiosink")?;
    let elements = [appsrc.upcast_ref(), &parse, &decoder, &convert, &resample, &vol, &sink];
    pipeline.add_many(elements).map_err(|e| e.to_string())?;
    if let Err(e) = gstreamer::Element::link_many(elements) {
        let _ = pipeline.remove_many(elements);
        return Err(e.to_string());
    }
    Ok(ReplayAudio { appsrc, volume: Some(vol) })
}

/// How much footage an audio unit holds: the sum of its ADTS frames (1024
/// samples each, times the raw data blocks, at the header's sample rate);
/// `AUDIO_UNIT_FALLBACK_US` when the unit does not parse as ADTS.
pub(crate) fn adts_unit_duration_us(unit: &[u8]) -> u64 {
    const RATES: [u64; 13] = [96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000, 7_350];
    let mut at = 0usize;
    let mut total_us = 0u64;
    while at < unit.len() {
        let Some(header) = unit.get(at..at + 7) else { return AUDIO_UNIT_FALLBACK_US };
        let sync = header[0] == 0xff && header[1] & 0xf6 == 0xf0;
        let rate = RATES.get(usize::from((header[2] >> 2) & 0x0f)).copied();
        let length = (usize::from(header[3] & 0x03) << 11) | (usize::from(header[4]) << 3) | usize::from(header[5] >> 5);
        let blocks = u64::from(header[6] & 0x03) + 1;
        let (true, Some(rate), true) = (sync, rate, length >= 7 && at + length <= unit.len()) else {
            return AUDIO_UNIT_FALLBACK_US;
        };
        total_us += 1024 * blocks * 1_000_000 / rate;
        at += length;
    }
    if total_us > 0 {
        total_us
    } else {
        AUDIO_UNIT_FALLBACK_US
    }
}

/// A small reserve in front of the decoder, by time and by bytes; the lead
/// itself waits in the appsrc. 1 s of PTS is 16 s of footage at 16x: 4-16
/// I-frames, a few MB even on main.
fn replay_queue() -> gstreamer::Element {
    gstreamer::ElementFactory::make("queue")
        .property("max-size-buffers", 0u32)
        .property("max-size-bytes", 16u32 * 1024 * 1024)
        .property("max-size-time", 1_000_000_000u64)
        .property_from_str("leaky", "no")
        .build()
        .expect("queue element missing — install gstreamer1.0-plugins-base")
}

/// The decoder settings live video uses (see `build_sink`): a late frame is
/// shown rather than discarded, and pictures leave as soon as decoded.
fn tune_replay_decoder(element: &gstreamer::Element) {
    let Some(factory) = element.factory() else { return };
    let klass = factory.klass();
    if !(klass.contains("Decoder") && klass.contains("Video")) {
        return;
    }
    if std::env::var("REOLING_DEBUG_DECODER").is_ok() {
        eprintln!("DECODER {} ({klass}) for replay", factory.name());
    }
    crate::ui::settings::note_decoder(&factory, true);
    if element.find_property("qos").is_some() {
        element.set_property("qos", false);
    }
    if element.find_property("compliance").is_some() {
        element.set_property_from_str("compliance", "flexible");
    }
}

/// Wraps a built replay pipeline (`appsrc ! … ! queue ! <decoder> ! … !
/// display`) in a `VideoSink`, with its two probes:
/// - on `display`'s input: the PTS of the last frame that reached it (the
///   timeline cursor, see `ReplayProbes::arrived_pts_us`);
/// - on `queue`'s output, i.e. the decoder's input: from
///   `REPLAY_THIN_FROM_SPEED` on, once a delta frame gets there more than
///   `REPLAY_LATE_LIMIT_US` late, it and the rest of its GOP are dropped;
///   the next key frame always passes and decoding starts again from it.
///   Dropping one delta frame but keeping later ones would smear the
///   picture, since each refers to the one before.
///
/// `audio` is the sound's chain in the same pipeline, if it has one. The
/// pipeline keeps the system clock whatever the audio output offers, so the
/// picture runs on while the sound has nothing to play (at 2x and faster,
/// or a recording without sound).
fn assemble_replay_sink(
    video_type: VideoType,
    pipeline: gstreamer::Pipeline,
    appsrc: AppSrc,
    queue: &gstreamer::Element,
    display: &gstreamer::Element,
    audio: Option<ReplayAudio>,
    decoded_frames: Arc<AtomicU64>,
) -> VideoSink {
    pipeline.use_clock(Some(&gstreamer::SystemClock::obtain()));
    let probes = Arc::new(ReplayProbes {
        arrived_pts_us: AtomicU64::new(NO_PTS),
        thin_when_late: AtomicBool::new(false),
        skipping: AtomicBool::new(false),
        dropped_late: AtomicU64::new(0),
    });
    if let Some(pad) = display.static_pad("sink") {
        let probes = probes.clone();
        pad.add_probe(gstreamer::PadProbeType::BUFFER, move |_, info| {
            if let Some(pts) = info.buffer().and_then(|buffer| buffer.pts()) {
                probes.arrived(pts.useconds());
            }
            gstreamer::PadProbeReturn::Ok
        });
    }
    if let Some(pad) = queue.static_pad("src") {
        let probes = probes.clone();
        let pipeline = pipeline.downgrade();
        pad.add_probe(gstreamer::PadProbeType::BUFFER, move |_, info| {
            let Some(buffer) = info.buffer() else { return gstreamer::PadProbeReturn::Ok };
            if !buffer.flags().contains(gstreamer::BufferFlags::DELTA_UNIT) {
                probes.skipping.store(false, Ordering::Relaxed);
                return gstreamer::PadProbeReturn::Ok;
            }
            if !probes.thin_when_late.load(Ordering::Relaxed) {
                return gstreamer::PadProbeReturn::Ok;
            }
            if !probes.skipping.load(Ordering::Relaxed) {
                let now = pipeline.upgrade().and_then(|p| playing_running_time_us(&p));
                let late = now
                    .zip(buffer.pts())
                    .is_some_and(|(now, pts)| now > pts.useconds() + REPLAY_LATE_LIMIT_US);
                if !late {
                    return gstreamer::PadProbeReturn::Ok;
                }
                probes.skipping.store(true, Ordering::Relaxed);
            }
            probes.dropped_late.fetch_add(1, Ordering::Relaxed);
            if let Some(pts) = buffer.pts() {
                probes.arrived(pts.useconds());
            }
            gstreamer::PadProbeReturn::Drop
        });
    }
    VideoSink {
        appsrc,
        pipeline,
        pts_state: Arc::new(Mutex::new(PtsState::default())),
        recorder: Arc::new(Mutex::new(None)),
        video_type,
        decoded_frames,
        replay: Some(Arc::new(Replay {
            state: Mutex::new(ReplayState {
                clock: ReplayClock::new(),
                overflowing: false,
                dropped_over_cap: 0,
                last_shown_us: None,
                ended: false,
                audio_ended: false,
                audio_waiting: false,
                reset_gen: 0,
                next_report: None,
            }),
            probes,
            queue: queue.clone(),
            audio,
            retired: AtomicBool::new(false),
            control: Mutex::new(()),
        })),
    }
}

/// `REOLING_DUMP_VIDEO=<file>` for replay, as for live (see `push_frame`).
fn dump_replay_unit(frame: &VideoFrame) {
    if let Ok(path) = std::env::var("REOLING_DUMP_VIDEO") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = f.write_all(&frame.data);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(format!("{path}.idx")) {
            let _ = writeln!(f, "{} {} {}", frame.data.len(), frame.is_keyframe, frame.microseconds);
        }
    }
}

impl VideoView {
    /// The replay counterpart of `build_sink`, the same chain with replay's
    /// own timing and buffering:
    ///
    /// appsrc (not live; holds the device's lead)
    ///   ! h264parse / h265parse
    ///   ! queue                 (small reserve, by time and bytes)
    ///   ! decodebin
    ///   ! videoconvert          (qos off, as for live)
    ///   ! gtk4paintablesink     (late frames still shown, as for live)
    ///
    /// plus the sound's own chain (`build_replay_audio`) in the same
    /// pipeline, when its elements are there. No pipeline latency: nothing
    /// here is live, the clock simply starts with the first frame shown.
    fn build_replay_sink(&self, video_type: VideoType) -> VideoSink {
        let parse_name = match video_type {
            VideoType::H264 => "h264parse",
            VideoType::H265 => "h265parse",
        };
        let pipeline = gstreamer::Pipeline::new();
        let appsrc = replay_appsrc(video_type);
        let parse = gstreamer::ElementFactory::make(parse_name)
            .property_from_str("config-interval", "-1")
            .build()
            .unwrap_or_else(|_| panic!("{parse_name} element missing — install the appropriate GStreamer parser plugin"));
        let queue = replay_queue();
        let decodebin = gstreamer::ElementFactory::make("decodebin")
            .property("force-sw-decoders", crate::ui::settings::force_software())
            .build()
            .expect("decodebin element missing — install gstreamer1.0-plugins-base");
        let convert = gstreamer::ElementFactory::make("videoconvert")
            .property("qos", false)
            .build()
            .expect("videoconvert element missing — install gstreamer1.0-plugins-base");
        let display = gstreamer::ElementFactory::make("gtk4paintablesink")
            .name("display")
            .property("max-lateness", -1i64)
            .build()
            .expect("gtk4paintablesink missing — install GStreamer GTK4 support");

        pipeline
            .add_many([appsrc.upcast_ref(), &parse, &queue, &decodebin, &convert, &display])
            .expect("adding elements to pipeline failed");
        gstreamer::Element::link_many([appsrc.upcast_ref(), &parse, &queue, &decodebin])
            .expect("linking appsrc -> parser -> queue -> decodebin failed");
        convert.link(&display).expect("linking videoconvert -> gtk4paintablesink failed");

        let decoded_frames = Arc::new(AtomicU64::new(0));
        let decoded_for_pad = decoded_frames.clone();
        let convert_weak = convert.downgrade();
        decodebin.connect_pad_added(move |_, src_pad| {
            let Some(convert) = convert_weak.upgrade() else { return };
            let Some(convert_sink_pad) = convert.static_pad("sink") else { return };
            if convert_sink_pad.is_linked() {
                return;
            }
            let caps = src_pad.current_caps().unwrap_or_else(|| src_pad.query_caps(None));
            if caps.structure(0).map_or(true, |s| s.name() != "video/x-raw") {
                return;
            }
            if std::env::var("REOLING_DEBUG_LATENCY").is_ok() {
                let decoded = decoded_for_pad.clone();
                src_pad.add_probe(gstreamer::PadProbeType::BUFFER, move |_, _| {
                    decoded.fetch_add(1, Ordering::Relaxed);
                    gstreamer::PadProbeReturn::Ok
                });
            }
            if let Err(err) = src_pad.link(&convert_sink_pad) {
                eprintln!("failed to link decodebin video output -> videoconvert: {err:?}");
            }
        });
        pipeline.connect_deep_element_added(|_, _, element| tune_replay_decoder(element));

        let audio = match build_replay_audio(&pipeline, self.replay_volume.get()) {
            Ok(audio) => Some(audio),
            Err(e) => {
                if !self.replay_audio_unavailable.replace(true) {
                    eprintln!("no sound for recordings: {e}");
                }
                None
            }
        };

        self.picture.set_paintable(Some(&display.property::<gtk4::gdk::Paintable>("paintable")));

        let sink = assemble_replay_sink(video_type, pipeline, appsrc, &queue, &display, audio, decoded_frames);
        sink.pipeline
            .set_state(gstreamer::State::Playing)
            .expect("failed to start GStreamer pipeline");
        sink
    }
}

/// Replay control. Everything here does nothing on a live sink.
impl VideoSink {
    /// Starts the timeline of a new open: `seek` is where it starts on the
    /// caller's own timeline (e.g. time of day), `speed` the playback speed
    /// chosen (0.25, 0.5, 1, 2, 4, 8, 16; below 1x the device sends at 1x
    /// and this sink stretches the picture, so the driver must watch
    /// `ReplaySinkStatus::slow_motion_backlog` and pause the device's
    /// sending; the memory cap is no substitute, past it frames are lost).
    /// Whatever the pipeline still holds from before (picture and sound) is
    /// thrown away, the next frame pushed is shown first, at PTS 0, and the
    /// pipeline plays (also after `set_paused`). Sound plays at 1x only.
    ///
    /// Call it on the thread that pushes the frames, before the new open's
    /// first frame (so nothing of the old open can follow it), and never on
    /// the GTK main thread: flushing waits for the pipeline's own threads.
    pub fn reset_for_open(&self, seek: Duration, speed: f64) {
        let Some(replay) = &self.replay else { return };
        let _control = replay.control.lock().expect("replay control mutex poisoned");
        if replay.retired.load(Ordering::SeqCst) {
            return;
        }
        {
            // Before the flush: the old open's wait for audio (see
            // `AUDIO_WAIT`), should it fire from now on, finds its open gone
            // and leaves the sound's source alone. Were it to end the
            // sound's stream after the flush, nothing would undo that and
            // the source would refuse the new open's audio. (One that fired
            // before this is undone by the flush.)
            let mut state = replay.state.lock().expect("replay mutex poisoned");
            state.reset_gen += 1;
            state.audio_waiting = false;
        }
        // Through the sources, so their own queues are emptied as well as
        // every element after them (and an earlier end of stream is undone);
        // with `reset_time` the running time starts again from 0, where the
        // next open's PTS start, once the picture (and the sound, if it has
        // anything) is ready to show its first frame.
        for source in std::iter::once(&self.appsrc).chain(replay.audio.as_ref().map(|audio| &audio.appsrc)) {
            let _ = source.send_event(gstreamer::event::FlushStart::new());
            let _ = source.send_event(gstreamer::event::FlushStop::new(true));
        }
        let speed = sanitize_speed(speed);
        let with_audio = replay.audio.is_some() && speed == 1.0;
        {
            // The probes too, under the same lock `replay_status` reads them
            // under, so it never mixes the old open's frames with the new.
            let mut state = replay.state.lock().expect("replay mutex poisoned");
            replay.probes.arrived_pts_us.store(NO_PTS, Ordering::Relaxed);
            replay.probes.skipping.store(false, Ordering::Relaxed);
            replay.probes.dropped_late.store(0, Ordering::Relaxed);
            replay.probes.thin_when_late.store(speed >= REPLAY_THIN_FROM_SPEED, Ordering::Relaxed);
            state.clock.reset(seek, speed);
            state.overflowing = false;
            state.dropped_over_cap = 0;
            state.last_shown_us = None;
            state.ended = false;
            state.audio_ended = false;
            state.audio_waiting = with_audio;
        }
        if let Some(audio) = replay.audio.as_ref().filter(|_| !with_audio) {
            // No sound at this speed: the picture must not wait for any.
            let _ = audio.appsrc.end_of_stream();
            replay.state.lock().expect("replay mutex poisoned").audio_ended = true;
        }
        let _ = self.pipeline.set_state(gstreamer::State::Playing);
    }

    /// The next open continues this one without a break: the next file
    /// after the device stopped at the end of one (`seek` being where that
    /// open starts on the caller's timeline). Nothing buffered is thrown
    /// away, the new frames follow straight after it, at the same speed.
    /// Call it on the pushing thread before the new open's first frame.
    /// After `end_of_footage`, only `reset_for_open` starts a new open.
    pub fn chain_open(&self, seek: Duration) {
        if let Some(replay) = self.replay.as_ref().filter(|replay| !replay.retired.load(Ordering::SeqCst)) {
            replay.state.lock().expect("replay mutex poisoned").clock.chain(seek);
        }
    }

    /// No more footage follows (the last file ended and none comes after
    /// it): what is queued still plays out, and the decoder gives up the
    /// frames it still holds. A software decoder working on several frames
    /// at once keeps the last ones back until more arrive; measured offline
    /// with libav's H.264 decoder on 24 cores, 10 sparse I-frames (16x)
    /// stayed in it until the end of stream. Frames pushed after this are
    /// ignored until the next `reset_for_open`.
    pub fn end_of_footage(&self) {
        let Some(replay) = &self.replay else { return };
        if replay.retired.load(Ordering::SeqCst) {
            return;
        }
        let (was_ended, audio_was_ended) = {
            let mut state = replay.state.lock().expect("replay mutex poisoned");
            let was = (state.ended, state.audio_ended);
            state.ended = true;
            state.audio_ended = true;
            state.audio_waiting = false;
            was
        };
        if !was_ended {
            let _ = self.appsrc.end_of_stream();
        }
        if let Some(audio) = replay.audio.as_ref().filter(|_| !audio_was_ended) {
            let _ = audio.appsrc.end_of_stream();
        }
    }

    /// Holds the picture where it is (`true`) or lets it run on (`false`).
    /// After a pause the footage either plays on from what is buffered
    /// (`set_paused(false)`, then `chain_open` for the resumed opening) or
    /// starts anew with `reset_for_open`, which plays by itself. Not on the
    /// GTK main thread (like `VideoView::freeze`, which runs its pause
    /// elsewhere): call it from the pushing thread, so it stays in order
    /// with `reset_for_open`.
    pub fn set_paused(&self, paused: bool) {
        let Some(replay) = &self.replay else { return };
        let _control = replay.control.lock().expect("replay control mutex poisoned");
        if replay.retired.load(Ordering::SeqCst) {
            return;
        }
        let state = if paused { gstreamer::State::Paused } else { gstreamer::State::Playing };
        let _ = self.pipeline.set_state(state);
    }

    /// The view has torn this replay pipeline down (see `Replay::retired`):
    /// a holder should drop it and ask for a new sink. Never on a live sink.
    pub fn is_retired(&self) -> bool {
        self.replay.as_ref().is_some_and(|replay| replay.retired.load(Ordering::SeqCst))
    }

    /// The sound's volume (0 to 1). Nothing on a live sink, or without sound.
    fn set_replay_volume(&self, volume: f64) {
        if let Some(element) = self.replay.as_ref().and_then(|replay| replay.audio.as_ref()).and_then(|a| a.volume.as_ref()) {
            element.set_property("volume", volume.clamp(0.0, 1.0));
        }
    }

    /// One audio unit (AAC, ADTS) of the footage, after the video frame it
    /// follows: timed by the replay clock like the frames (see
    /// `ReplayClock::stamp_audio`) and queued without waiting. Nothing is
    /// queued at a speed other than 1x, before the open's first video
    /// frame, after `end_of_footage`, over `REPLAY_AUDIO_MAX_QUEUED_BYTES`,
    /// or on a sink without sound. Call it on the thread that pushes the
    /// frames, like `push_frame`.
    pub fn push_replay_audio(&self, unit: &[u8]) {
        let Some(replay) = &self.replay else { return };
        let Some(audio) = &replay.audio else { return };
        if replay.retired.load(Ordering::SeqCst) {
            return;
        }
        let (pts_us, reopen) = {
            let mut state = replay.state.lock().expect("replay mutex poisoned");
            if state.ended || state.clock.speed != 1.0 {
                return;
            }
            let Some(pts_us) = state.clock.stamp_audio(adts_unit_duration_us(unit)) else { return };
            state.audio_waiting = false;
            (pts_us, std::mem::replace(&mut state.audio_ended, false))
        };
        if reopen {
            // The sound's stream was ended (see `audio_ended`), but here is
            // audio after all: open it again, leaving the running time alone
            // (the picture is playing on it).
            let _ = audio.appsrc.send_event(gstreamer::event::FlushStart::new());
            let _ = audio.appsrc.send_event(gstreamer::event::FlushStop::new(false));
        }
        if audio.appsrc.current_level_bytes().saturating_add(unit.len() as u64) > REPLAY_AUDIO_MAX_QUEUED_BYTES {
            return;
        }
        let mut buffer = gstreamer::Buffer::from_slice(unit.to_vec());
        buffer.get_mut().expect("new buffer").set_pts(gstreamer::ClockTime::from_useconds(pts_us));
        match audio.appsrc.push_buffer(buffer) {
            Ok(_) | Err(gstreamer::FlowError::Flushing) => {}
            Err(err) => eprintln!("failed to push replay audio into appsrc: {err:?}"),
        }
    }

    /// Where replay stands: position on screen, footage buffered ahead of
    /// it, frames not shown, and below 1x whether the device's sending has
    /// to pause — see `ReplaySinkStatus`. `None` on a live sink. Cheap, and
    /// safe from any thread (e.g. a GTK timer).
    pub fn replay_status(&self) -> Option<ReplaySinkStatus> {
        let replay = self.replay.as_ref()?;
        let mut state = replay.state.lock().expect("replay mutex poisoned");
        Some(self.replay_status_locked(replay, &mut state))
    }

    fn replay_status_locked(&self, replay: &Replay, state: &mut ReplayState) -> ReplaySinkStatus {
        let arrived = replay.probes.arrived_pts_us.load(Ordering::Relaxed);
        let shown = (arrived != NO_PTS).then(|| {
            // While playing, the frame that reached the display waits there
            // for its time: the screen is at most at the running time.
            let at = playing_running_time_us(&self.pipeline).map_or(arrived, |now| arrived.min(now));
            state.last_shown_us.map_or(at, |last| at.max(last))
        });
        if let Some(shown) = shown {
            state.last_shown_us = Some(shown);
            state.clock.forget_shown(shown);
        }
        let in_queue = u64::from(replay.queue.property::<u32>("current-level-bytes"));
        let buffered = state.clock.buffered(shown);
        ReplaySinkStatus {
            position: state.clock.position(shown),
            buffered,
            buffered_bytes: self.appsrc.current_level_bytes() + in_queue,
            dropped_frames: state.dropped_over_cap + replay.probes.dropped_late.load(Ordering::Relaxed),
            slow_motion_backlog: state.clock.speed < 1.0 && buffered >= REPLAY_SLOW_MOTION_MAX_BUFFERED,
        }
    }

    /// `push_frame` for a replay sink: PTS from `ReplayClock`, queued
    /// without ever waiting, up to `REPLAY_MAX_QUEUED_BYTES`.
    fn push_replay_frame(&self, replay: &Arc<Replay>, frame: &VideoFrame) {
        if replay.retired.load(Ordering::SeqCst) {
            return;
        }
        let now = playing_running_time_us(&self.pipeline);
        let mut state = replay.state.lock().expect("replay mutex poisoned");
        if state.ended {
            return;
        }
        let stamp = state.clock.stamp(frame.microseconds, now);
        if state.audio_waiting {
            if state.clock.frames >= AUDIO_WAIT_FRAMES {
                // No sound with this many frames: the picture plays without
                // it (see `AUDIO_WAIT_FRAMES`).
                replay.end_audio_wait(&mut state);
            } else if state.clock.frames == 1 {
                // ...or without sound `AUDIO_WAIT` after its first frame.
                let clock = gstreamer::SystemClock::obtain();
                let (shared, gen) = (Arc::clone(replay), state.reset_gen);
                let due = clock.time().unwrap_or(gstreamer::ClockTime::ZERO) + AUDIO_WAIT;
                let _ = clock.new_single_shot_id(due).wait_async(move |_, _, _| shared.audio_wait_over(gen));
            }
        }
        let fits = self.appsrc.current_level_bytes().saturating_add(frame.data.len() as u64) <= REPLAY_MAX_QUEUED_BYTES;
        if state.overflowing {
            if frame.is_keyframe && fits {
                state.overflowing = false;
            }
        } else if !fits {
            state.overflowing = true;
        }
        let queue_it = !state.overflowing;
        if !queue_it {
            state.dropped_over_cap += 1;
        }
        if std::env::var("REOLING_DEBUG_LATENCY").is_ok() {
            let now = std::time::Instant::now();
            if state.next_report.map_or(true, |due| now >= due) {
                state.next_report = Some(now + Duration::from_secs(1));
                let status = self.replay_status_locked(replay, &mut state);
                eprintln!(
                    "REPLAY {}x at {:.1} s, {:.1} s of footage buffered ({} KB); pushed {} decoded {} not shown {}; audio units {}{}",
                    state.clock.speed,
                    status.position.as_secs_f64(),
                    status.buffered.as_secs_f64(),
                    status.buffered_bytes / 1024,
                    state.clock.frames,
                    self.decoded_frames.load(Ordering::Relaxed),
                    status.dropped_frames,
                    state.clock.audio_units,
                    if state.audio_ended { " (sound ended)" } else { "" }
                );
            }
        }
        drop(state);

        if let Some(recorder) = self.recorder.lock().expect("recorder mutex poisoned").as_mut() {
            recorder.push(frame, gstreamer::ClockTime::from_useconds(stamp.recording_us));
        }
        dump_replay_unit(frame);
        if !queue_it {
            return;
        }

        let mut buffer = gstreamer::Buffer::from_slice(frame.data.clone());
        {
            let buffer = buffer.get_mut().expect("new buffer");
            buffer.set_pts(gstreamer::ClockTime::from_useconds(stamp.pts_us));
            if !frame.is_keyframe {
                buffer.set_flags(gstreamer::BufferFlags::DELTA_UNIT);
            }
        }
        match self.appsrc.push_buffer(buffer) {
            Ok(_) | Err(gstreamer::FlowError::Flushing) => {}
            Err(err) => eprintln!("failed to push replay buffer into appsrc: {err:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000_000;

    fn clock(seek_s: u64, speed: f64) -> ReplayClock {
        let mut clock = ReplayClock::new();
        clock.reset(Duration::from_secs(seek_s), speed);
        clock
    }

    /// Stamps frames at the given camera timestamps, with no running time.
    fn pts_of(clock: &mut ReplayClock, raws: &[u32]) -> Vec<u64> {
        raws.iter().map(|&raw| clock.stamp(raw, None).pts_us).collect()
    }

    #[test]
    fn one_x_follows_the_camera_clock_from_zero() {
        let mut c = clock(0, 1.0);
        assert_eq!(pts_of(&mut c, &[5_000_000, 5_100_000, 5_200_000]), [0, 100_000, 200_000]);
    }

    #[test]
    fn steps_stay_right_across_the_u32_wrap() {
        let mut c = clock(0, 1.0);
        let before = u32::MAX - 49_999; // 50 ms before the wrap
        let pts = pts_of(&mut c, &[before, before.wrapping_add(100_000), before.wrapping_add(200_000)]);
        assert_eq!(pts, [0, 100_000, 200_000]);
    }

    #[test]
    fn pts_is_footage_divided_by_speed() {
        let mut fast = clock(0, 16.0);
        // 16x: I-frames only, ~4 s of footage apart, 4.24 s the largest seen.
        assert_eq!(pts_of(&mut fast, &[0, 4_000_000, 8_240_000]), [0, 250_000, 515_000]);
        let mut slow = clock(0, 0.25);
        assert_eq!(pts_of(&mut slow, &[0, 100_000, 200_000]), [0, 400_000, 800_000]);
        let mut half = clock(0, 0.5);
        assert_eq!(pts_of(&mut half, &[0, 120_000]), [0, 240_000]);
    }

    #[test]
    fn replay_step_cap_is_wider_than_live() {
        let mut c = clock(0, 16.0);
        // 6 s between I-frames would be clamped live (5 s); replay keeps it.
        assert_eq!(pts_of(&mut c, &[0, 6_000_000]), [0, 375_000]);
        // 12 s is not believed: the last good step (6 s) stands in.
        assert_eq!(c.stamp(18_000_000, None).pts_us, 750_000);
        // A backwards jump looks like a huge wrapped step: not believed either.
        assert_eq!(c.stamp(1_000_000, None).pts_us, 1_125_000);
        // The camera's numbers make sense again from the next frame on.
        assert_eq!(c.stamp(5_000_000, None).pts_us, 1_375_000);
    }

    #[test]
    fn implausible_first_step_uses_the_nominal_frame_interval() {
        let mut c = clock(0, 1.0);
        assert_eq!(pts_of(&mut c, &[1_000, 1_000]), [0, DEFAULT_STEP_US]);
    }

    #[test]
    fn reset_starts_the_timeline_again_at_the_new_seek_and_speed() {
        let mut c = clock(100, 1.0);
        pts_of(&mut c, &[0, 100_000, 200_000]);
        assert_eq!(c.position(Some(200_000)), Duration::from_millis(100_200));
        c.reset(Duration::from_secs(500), 4.0);
        assert!(c.last_pts_us.is_none());
        assert_eq!(c.position(None), Duration::from_secs(500));
        assert_eq!(c.buffered(None), Duration::ZERO);
        assert_eq!(pts_of(&mut c, &[77_000_000, 77_400_000]), [0, 100_000]);
        assert_eq!(c.position(Some(100_000)), Duration::from_millis(500_400));
    }

    #[test]
    fn position_and_buffered_follow_the_frame_on_screen() {
        let mut c = clock(3_600, 2.0);
        let raws: Vec<u32> = (0..50).map(|i| 1_000 + i * 100_000).collect(); // 5 s of footage
        let pts = pts_of(&mut c, &raws);
        assert_eq!(pts[49], 2_450_000);
        // Nothing shown yet: the seek point, everything buffered.
        assert_eq!(c.position(None), Duration::from_secs(3_600));
        assert_eq!(c.buffered(None), Duration::from_millis(4_900));
        // Frame 10 on screen: 1 s of footage in.
        assert_eq!(c.position(Some(500_000)), Duration::from_secs(3_601));
        assert_eq!(c.buffered(Some(500_000)), Duration::from_millis(3_900));
        // Between frames 10 and 11: interpolated, never past frame 11.
        assert_eq!(c.position(Some(525_000)), Duration::from_millis(3_601_050));
        c.forget_shown(525_000);
        assert_eq!(c.marks.len(), 40);
        assert_eq!(c.position(Some(525_000)), Duration::from_millis(3_601_050));
    }

    #[test]
    fn chained_open_carries_on_one_frame_later_at_its_own_seek() {
        let mut c = clock(1_000, 1.0);
        pts_of(&mut c, &[0, 100_000, 200_000]);
        c.chain(Duration::from_secs(2_000));
        // A new file, a new camera clock: its first frame follows one step on.
        assert_eq!(pts_of(&mut c, &[900_000_000, 900_100_000]), [300_000, 400_000]);
        assert_eq!(c.position(Some(200_000)), Duration::from_millis(1_000_200));
        assert_eq!(c.position(Some(300_000)), Duration::from_secs(2_000));
        assert_eq!(c.position(Some(400_000)), Duration::from_millis(2_000_100));
        // Between the two files the cursor moves on from the old file's last
        // frame, never past one frame interval.
        assert_eq!(c.position(Some(250_000)), Duration::from_millis(1_000_250));
        assert_eq!(c.position(Some(299_999)), Duration::from_micros(1_000_299_999));
        assert_eq!(c.buffered(Some(200_000)), Duration::from_millis(200));
    }

    #[test]
    fn a_frame_after_its_due_time_moves_the_timeline_later_never_earlier() {
        let mut c = clock(0, 1.0);
        assert_eq!(c.stamp(0, Some(0)).pts_us, 0);
        // Ahead of time (the lead): untouched.
        assert_eq!(c.stamp(100_000, Some(50_000)).pts_us, 100_000);
        // The lead ran out: the next frame was due at 200 ms, it is 1.2 s now.
        assert_eq!(c.stamp(200_000, Some(1_200_000)).pts_us, 1_200_000);
        // The ones after it keep the camera cadence from there.
        assert_eq!(c.stamp(300_000, Some(1_210_000)).pts_us, 1_300_000);
        // The cursor still counts footage, not the stall.
        assert_eq!(c.position(Some(1_300_000)), Duration::from_millis(300));
        // A stall longer than REPLAY_MAX_REBASE_US: the timeline moves by at
        // most that much per frame, and has caught up a frame or two later.
        let stall = REPLAY_MAX_REBASE_US + 10 * SEC;
        let now = 1_400_000 + stall;
        assert_eq!(c.stamp(400_000, Some(now)).pts_us, 1_400_000 + REPLAY_MAX_REBASE_US);
        assert_eq!(c.stamp(500_000, Some(now + 10_000)).pts_us, now + 10_000);
        // From there on the camera cadence again, not one late frame after
        // another.
        assert_eq!(c.stamp(600_000, Some(now + 20_000)).pts_us, now + 110_000);
        assert_eq!(c.stamp(700_000, Some(now + 30_000)).pts_us, now + 210_000);
    }

    #[test]
    fn recording_clock_rises_across_opens() {
        let mut c = clock(0, 16.0);
        let a = [c.stamp(0, None), c.stamp(4_000_000, None)];
        c.reset(Duration::from_secs(10), 1.0);
        let b = c.stamp(123, None);
        assert_eq!(a[0].recording_us, 0);
        assert_eq!(a[1].recording_us, 4 * SEC);
        assert_eq!(b.recording_us, 4 * SEC + DEFAULT_STEP_US);
        assert_eq!(b.pts_us, 0);
    }

    #[test]
    fn speeds_are_sanitized() {
        assert_eq!(sanitize_speed(16.0), 16.0);
        assert_eq!(sanitize_speed(0.25), 0.25);
        assert_eq!(sanitize_speed(0.0), 1.0);
        assert_eq!(sanitize_speed(-2.0), 1.0);
        assert_eq!(sanitize_speed(f64::NAN), 1.0);
        assert_eq!(sanitize_speed(1e9), 64.0);
    }

    #[test]
    fn audio_is_timed_after_the_frame_it_follows() {
        let mut c = clock(0, 1.0);
        // Nothing to follow before the first frame.
        assert_eq!(c.stamp_audio(64_000), None);
        c.stamp(1_000_000, None);
        // The first unit starts at the frame, the ones after it where the
        // one before ended, whatever frames came in between.
        assert_eq!(c.stamp_audio(64_000), Some(0));
        assert_eq!(c.stamp_audio(64_000), Some(64_000));
        assert_eq!(c.stamp_audio(64_000), Some(128_000));
        c.stamp(1_100_000, None);
        assert_eq!(c.stamp_audio(64_000), Some(192_000));
        c.stamp(1_200_000, None);
        assert_eq!(c.stamp_audio(32_000), Some(256_000));
        assert_eq!(c.stamp_audio(32_000), Some(288_000));
        assert_eq!(c.audio_units, 6);
    }

    #[test]
    fn audio_stays_contiguous_across_frames_and_follows_the_picture_from_afar() {
        // 10 fps with one or two 64 ms units after each frame, as the
        // devices send it: every unit starts where the one before ended,
        // although the frames' timestamps run ahead of and behind the count.
        let mut c = clock(0, 1.0);
        let mut expected = 0;
        for (i, units) in [1, 2, 1, 2, 2, 1, 2, 1, 2, 2].iter().enumerate() {
            c.stamp(i as u32 * 100_000, None);
            for _ in 0..*units {
                assert_eq!(c.stamp_audio(64_000), Some(expected), "unit after frame {i}");
                expected += 64_000;
            }
        }
        assert_eq!(c.audio_units, 16);
        // The units' count is within a unit or two of the frames: no
        // re-anchoring in 1.6 s of this.
        assert_eq!(expected, 1_024_000);

        // The sound ran far ahead of the picture (more units than the
        // footage holds): the next unit starts at the latest frame again.
        let mut c = clock(0, 1.0);
        c.stamp(0, None);
        for i in 0..4 {
            assert_eq!(c.stamp_audio(64_000), Some(i * 64_000));
        }
        c.stamp(100_000, None);
        assert_eq!(c.stamp_audio(64_000), Some(256_000), "156 ms ahead: still contiguous");
        assert_eq!(c.stamp_audio(64_000), Some(320_000), "220 ms ahead: still contiguous");
        assert_eq!(c.stamp_audio(64_000), Some(100_000), "284 ms ahead: at the latest frame again");
        assert_eq!(c.stamp_audio(64_000), Some(164_000));

        // ...and far behind (no units for a while): likewise.
        let mut c = clock(0, 1.0);
        c.stamp(0, None);
        assert_eq!(c.stamp_audio(64_000), Some(0));
        for i in 1..=4 {
            c.stamp(i * 100_000, None);
        }
        assert_eq!(c.stamp_audio(64_000), Some(400_000), "64 ms of sound after 400 ms of picture");
        // Within the margin it carries on contiguously.
        let mut c = clock(0, 1.0);
        c.stamp(0, None);
        assert_eq!(c.stamp_audio(64_000), Some(0));
        for i in 1..=3 {
            c.stamp(i * 100_000, None);
        }
        assert_eq!(c.stamp_audio(64_000), Some(64_000), "236 ms behind the picture: still contiguous");
    }

    #[test]
    fn audio_keeps_the_pictures_speed_offset_and_opens() {
        // At 2x the units are placed at half their footage time too (the
        // sink itself plays no sound then; the clock is one and the same).
        let mut fast = clock(0, 2.0);
        fast.stamp(0, None);
        fast.stamp(100_000, None);
        assert_eq!(fast.stamp_audio(64_000), Some(50_000));
        assert_eq!(fast.stamp_audio(64_000), Some(82_000));

        // A frame after its due time moves the picture later (see `stamp`):
        // the sound after it moves with it.
        let mut c = clock(0, 1.0);
        c.stamp(0, Some(0));
        assert_eq!(c.stamp_audio(64_000), Some(0));
        assert_eq!(c.stamp(100_000, Some(1_100_000)).pts_us, 1_100_000);
        assert_eq!(c.stamp_audio(64_000), Some(1_064_000));
        assert_eq!(c.stamp_audio(64_000), Some(1_128_000));

        // The next file carries straight on: its first frame comes one
        // frame interval after the last, and its audio with it, with no gap
        // and nothing twice.
        let mut c = clock(1_000, 1.0);
        for (raw, units) in [(0, 2), (100_000, 2), (200_000, 1)] {
            c.stamp(raw, None);
            for _ in 0..units {
                c.stamp_audio(64_000);
            }
        }
        assert_eq!(c.stamp_audio(64_000), Some(320_000));
        c.chain(Duration::from_secs(2_000));
        assert_eq!(c.stamp_audio(64_000), Some(384_000), "audio of the old file carries on after its last frame");
        assert_eq!(c.stamp(900_000_000, None).pts_us, 300_000);
        assert_eq!(c.stamp_audio(64_000), Some(448_000));
        assert_eq!(c.stamp_audio(64_000), Some(512_000));

        // A new open starts the sound over with the picture.
        c.reset(Duration::from_secs(5_000), 1.0);
        assert_eq!(c.stamp_audio(64_000), None);
        assert_eq!(c.audio_units, 0);
        c.stamp(77, None);
        assert_eq!(c.stamp_audio(64_000), Some(0));
    }

    /// One ADTS frame: `rate_index` from the ADTS table, `blocks` raw data
    /// blocks, `payload` bytes after the 7-byte header.
    fn adts(rate_index: u8, blocks: u8, payload: usize) -> Vec<u8> {
        let length = 7 + payload;
        let mut unit = vec![
            0xff,
            0xf1,
            (1 << 6) | (rate_index << 2),
            (1 << 6) | ((length >> 11) & 0x03) as u8,
            ((length >> 3) & 0xff) as u8,
            (((length & 0x07) << 5) as u8) | 0x1f,
            0xfc | (blocks - 1),
        ];
        unit.resize(length, 0xaa);
        unit
    }

    #[test]
    fn an_audio_units_length_comes_from_its_adts_headers() {
        // 1024 samples at 16 kHz, what the devices send.
        assert_eq!(adts_unit_duration_us(&adts(8, 1, 512)), 64_000);
        // Two frames in one unit; two raw data blocks in one frame.
        assert_eq!(adts_unit_duration_us(&[adts(8, 1, 100), adts(8, 1, 300)].concat()), 128_000);
        assert_eq!(adts_unit_duration_us(&adts(11, 2, 64)), 256_000);
        assert_eq!(adts_unit_duration_us(&adts(3, 1, 64)), 21_333);
        // Anything else is taken for the usual unit.
        assert_eq!(adts_unit_duration_us(&[]), AUDIO_UNIT_FALLBACK_US);
        assert_eq!(adts_unit_duration_us(&[0xff, 0x01, 0x02]), AUDIO_UNIT_FALLBACK_US);
        assert_eq!(adts_unit_duration_us(&[0x00; 519]), AUDIO_UNIT_FALLBACK_US);
        let mut short = adts(8, 1, 512);
        short.truncate(100); // a frame cut off
        assert_eq!(adts_unit_duration_us(&short), AUDIO_UNIT_FALLBACK_US);
        let mut bad_rate = adts(8, 1, 64);
        bad_rate[2] = 0x40 | (15 << 2);
        assert_eq!(adts_unit_duration_us(&bad_rate), AUDIO_UNIT_FALLBACK_US);
    }

    // ---- The replay pipeline's buffering, flush, thinning and sound, with
    // GStreamer core elements only (no decoders):
    //   appsrc ! queue [! identity] ! fakesink   (the picture)
    //   appsrc ! fakesink                        (the sound)

    fn test_sink(slow_element_us: Option<u32>) -> (VideoSink, gstreamer::Element) {
        gstreamer::init().expect("gstreamer init");
        let pipeline = gstreamer::Pipeline::new();
        let appsrc = replay_appsrc(VideoType::H264);
        let queue = replay_queue();
        let display = gstreamer::ElementFactory::make("fakesink")
            .property("sync", true)
            .property("max-lateness", -1i64)
            .property("qos", false)
            .build()
            .expect("fakesink");
        let mut chain: Vec<gstreamer::Element> = vec![appsrc.clone().upcast(), queue.clone()];
        if let Some(us) = slow_element_us {
            // Stands in for a decoder that cannot keep up.
            chain.push(gstreamer::ElementFactory::make("identity").property("sleep-time", us).build().expect("identity"));
        }
        chain.push(display.clone());
        pipeline.add_many(&chain).expect("add");
        gstreamer::Element::link_many(&chain).expect("link");
        // The sound: its source straight into a sink that plays by the clock
        // and takes part in the preroll, as an audio output does.
        let audio_appsrc = replay_audio_appsrc();
        let speaker = gstreamer::ElementFactory::make("fakesink")
            .name("speaker")
            .property("sync", true)
            .property("max-lateness", -1i64)
            .property("qos", false)
            .build()
            .expect("fakesink");
        pipeline.add_many([audio_appsrc.upcast_ref(), &speaker]).expect("add");
        audio_appsrc.link(&speaker).expect("link");
        let audio = Some(ReplayAudio { appsrc: audio_appsrc, volume: None });
        let sink =
            assemble_replay_sink(VideoType::H264, pipeline, appsrc, &queue, &display, audio, Arc::new(AtomicU64::new(0)));
        sink.pipeline.set_state(gstreamer::State::Playing).expect("play");
        (sink, display)
    }

    fn frame(us: u32, key: bool) -> VideoFrame {
        VideoFrame { video_type: VideoType::H264, is_keyframe: key, microseconds: us, data: vec![0u8; 2_000] }
    }

    /// A 64 ms audio unit, as the devices send them.
    fn audio_unit() -> Vec<u8> {
        adts(8, 1, 512)
    }

    /// Pushes `frames` frames of 10 fps footage from `from_us`, each followed
    /// by its audio (one or two units a frame: 15.6 units a second).
    fn push_with_sound(sink: &VideoSink, from_us: u32, frames: u32, key_every: u32) {
        for i in 0..frames {
            sink.push_frame(&frame(from_us + i * 100_000, i % key_every == 0));
            for _ in 0..(1 + (i * 5 / 8 + 1) % 2) {
                sink.push_replay_audio(&audio_unit());
            }
        }
    }

    fn rendered(display: &gstreamer::Element) -> u64 {
        display.property::<gstreamer::Structure>("stats").get::<u64>("rendered").unwrap_or(0)
    }

    fn speaker(sink: &VideoSink) -> gstreamer::Element {
        sink.pipeline.by_name("speaker").expect("the test sink's speaker")
    }

    fn audio_appsrc(sink: &VideoSink) -> &AppSrc {
        &sink.replay.as_ref().expect("replay").audio.as_ref().expect("sound").appsrc
    }

    #[test]
    fn replay_pipeline_buffers_the_lead_plays_at_speed_and_flushes_on_a_new_open() {
        let (sink, display) = test_sink(None);
        sink.reset_for_open(Duration::from_secs(1_000), 1.0);
        // 3 s of 1x footage (10 fps) arriving all at once: the device's lead.
        let started = std::time::Instant::now();
        for i in 0..30u32 {
            sink.push_frame(&frame(i * 100_000, i % 10 == 0));
        }
        assert!(started.elapsed() < Duration::from_millis(200), "pushing must never wait");
        std::thread::sleep(Duration::from_millis(1_000));
        let shown = rendered(&display);
        assert!((6..=15).contains(&shown), "1 s of 1x should show ~10 frames, not the whole lead: {shown}");
        let status = sink.replay_status().expect("replay sink");
        let position = status.position.as_secs_f64();
        assert!((1_000.5..=1_001.5).contains(&position), "position {position}");
        assert!(status.buffered >= Duration::from_millis(1_500), "buffered {:?}", status.buffered);
        assert!(status.buffered_bytes > 0);

        // A new open at 2x somewhere else: the old lead is gone at once and
        // the new frames play from the start, at their own pace.
        sink.reset_for_open(Duration::from_secs(5_000), 2.0);
        let status = sink.replay_status().expect("replay sink");
        assert_eq!(status.position, Duration::from_secs(5_000));
        assert_eq!(status.buffered, Duration::ZERO);
        assert_eq!(sink.appsrc.current_level_bytes(), 0);
        let before = rendered(&display);
        for i in 0..40u32 {
            sink.push_frame(&frame(7_000_000 + i * 100_000, i % 10 == 0));
        }
        std::thread::sleep(Duration::from_millis(500));
        let shown = rendered(&display) - before;
        // 0.5 s of playback at 2x = 1 s of footage = ~10 frames out of 40.
        assert!((5..=16).contains(&shown), "0.5 s at 2x should show ~10 frames: {shown}");
        let position = sink.replay_status().expect("replay sink").position.as_secs_f64();
        assert!((5_000.5..=5_001.6).contains(&position), "position {position}");
        let _ = sink.pipeline.set_state(gstreamer::State::Null);
    }

    #[test]
    fn replay_sound_plays_in_time_with_the_picture_at_1x_only() {
        let (sink, display) = test_sink(None);
        let speaker = speaker(&sink);
        sink.reset_for_open(Duration::from_secs(1_000), 1.0);
        // 3 s of 1x footage with its sound, arriving all at once.
        let started = std::time::Instant::now();
        push_with_sound(&sink, 0, 30, 10);
        assert!(started.elapsed() < Duration::from_millis(200), "pushing must never wait");
        std::thread::sleep(Duration::from_millis(1_000));
        let (shown, heard) = (rendered(&display), rendered(&speaker));
        assert!((6..=15).contains(&shown), "1 s of 1x should show ~10 frames: {shown}");
        assert!((10..=22).contains(&heard), "1 s of 1x should play ~16 audio units, not the whole lead: {heard}");
        assert!(audio_appsrc(&sink).current_level_bytes() > 0, "the sound's lead waits in its source");

        // A new open at 2x: the old sound is gone at once with the picture,
        // and nothing is played at that speed, while the picture plays.
        sink.reset_for_open(Duration::from_secs(5_000), 2.0);
        assert_eq!(audio_appsrc(&sink).current_level_bytes(), 0);
        let (before_shown, before_heard) = (rendered(&display), rendered(&speaker));
        push_with_sound(&sink, 7_000_000, 40, 10);
        assert_eq!(audio_appsrc(&sink).current_level_bytes(), 0, "no sound is queued at 2x");
        std::thread::sleep(Duration::from_millis(500));
        let shown = rendered(&display) - before_shown;
        assert!((5..=16).contains(&shown), "0.5 s at 2x should show ~10 frames: {shown}");
        assert_eq!(rendered(&speaker), before_heard, "silent at 2x");

        // Back at 1x the sound comes back with the picture.
        sink.reset_for_open(Duration::from_secs(6_000), 1.0);
        let (before_shown, before_heard) = (rendered(&display), rendered(&speaker));
        push_with_sound(&sink, 9_000_000, 20, 10);
        std::thread::sleep(Duration::from_millis(500));
        let (shown, heard) = (rendered(&display) - before_shown, rendered(&speaker) - before_heard);
        assert!((3..=8).contains(&shown), "0.5 s at 1x should show ~5 frames: {shown}");
        assert!((5..=12).contains(&heard), "0.5 s at 1x should play ~8 audio units: {heard}");
        let _ = sink.pipeline.set_state(gstreamer::State::Null);
    }

    #[test]
    fn replay_sound_holds_with_the_picture_and_goes_on_across_files() {
        let (sink, display) = test_sink(None);
        let speaker = speaker(&sink);
        sink.reset_for_open(Duration::from_secs(100), 1.0);
        push_with_sound(&sink, 0, 20, 10);
        std::thread::sleep(Duration::from_millis(500));
        let (shown_before, heard_before) = (rendered(&display), rendered(&speaker));
        assert!((3..=8).contains(&shown_before) && (5..=12).contains(&heard_before), "{shown_before} {heard_before}");
        // Pause: neither the picture nor the sound moves (a sink starts its
        // counts over when paused).
        sink.set_paused(true);
        std::thread::sleep(Duration::from_millis(500));
        assert!(rendered(&display) <= 1 && rendered(&speaker) <= 1, "paused but still playing");
        // The next file follows on what is buffered: nothing is flushed,
        // the sound plays on into the new file.
        sink.set_paused(false);
        sink.chain_open(Duration::from_secs(200));
        push_with_sound(&sink, 500_000_000, 20, 10);
        std::thread::sleep(Duration::from_millis(1_000));
        let (shown, heard) = (rendered(&display), rendered(&speaker));
        assert!((7..=14).contains(&shown), "1 s on from the pause should show ~10 frames: {shown}");
        assert!((11..=22).contains(&heard), "1 s on from the pause should play ~16 audio units: {heard}");
        // The last file ended: the sound ends with the picture, and a new
        // open starts both over.
        sink.end_of_footage();
        sink.push_replay_audio(&audio_unit()); // ignored
        std::thread::sleep(Duration::from_millis(2_500));
        // (A frame may have been shown between the count and the pause.)
        let shown_after = rendered(&display);
        assert!((40 - shown_before - 1..=40 - shown_before).contains(&shown_after), "{shown_after} of {}", 40 - shown_before);
        assert_eq!(audio_appsrc(&sink).current_level_bytes(), 0);
        let heard_all = rendered(&speaker);
        assert!((50..=70).contains(&heard_all), "all the sound played out: {heard_all}");
        sink.reset_for_open(Duration::from_secs(300), 1.0);
        assert_eq!(rendered(&speaker), 0, "a flushed sink starts its counts over");
        push_with_sound(&sink, 10_000_000, 20, 10);
        std::thread::sleep(Duration::from_millis(500));
        let heard = rendered(&speaker);
        assert!((5..=12).contains(&heard), "0.5 s of a new open should play ~8 audio units: {heard}");
        let _ = sink.pipeline.set_state(gstreamer::State::Null);
    }

    #[test]
    fn a_recording_without_sound_plays_and_sound_that_comes_late_joins_in() {
        let (sink, display) = test_sink(None);
        let speaker = speaker(&sink);
        sink.reset_for_open(Duration::from_secs(100), 1.0);
        // 1x, but no audio comes: after AUDIO_WAIT_FRAMES frames the picture
        // stops waiting for it.
        for i in 0..AUDIO_WAIT_FRAMES as u32 + 5 {
            sink.push_frame(&frame(i * 100_000, i == 0));
        }
        std::thread::sleep(Duration::from_millis(800));
        let shown = rendered(&display);
        assert!((5..=12).contains(&shown), "0.8 s of a silent recording should show ~8 frames: {shown}");
        assert_eq!(rendered(&speaker), 0);
        // Audio after all (the next file has some): it joins in and plays
        // on the same timeline, after the frames it follows, which are
        // still 0.7 s ahead of the picture.
        push_with_sound(&sink, 1_500_000, 20, 10);
        std::thread::sleep(Duration::from_millis(1_700));
        let heard = rendered(&speaker);
        assert!((10..=24).contains(&heard), "1 s after sound joined in it should play ~16 units: {heard}");
        let shown = rendered(&display);
        assert!((20..=32).contains(&shown), "the picture played on meanwhile: {shown}");
        let _ = sink.pipeline.set_state(gstreamer::State::Null);
    }

    #[test]
    fn replay_drops_late_delta_frames_per_gop_from_8x_only() {
        // An element taking 30 ms per frame: ~33 fps where 8x of a 10 fps
        // stream needs 80.
        for (speed, expect_drops) in [(4.0, false), (8.0, true)] {
            let (sink, _display) = test_sink(Some(30_000));
            sink.reset_for_open(Duration::ZERO, speed);
            // Two 40-frame GOPs at 10 fps.
            for i in 0..80u32 {
                sink.push_frame(&frame(i * 100_000, i % 40 == 0));
            }
            std::thread::sleep(Duration::from_millis(1_500));
            let status = sink.replay_status().expect("replay sink");
            let dropped = status.dropped_frames;
            assert_eq!(dropped > 0, expect_drops, "{speed}x dropped {dropped}");
            // Never a key frame: at most the 39 delta frames of each GOP.
            assert!(dropped <= 78, "{speed}x dropped {dropped}");
            if expect_drops {
                // Kept up: all 7.9 s of footage played (shown or skipped) by now.
                assert!(status.buffered <= Duration::from_millis(500), "{speed}x {status:?}");
                assert!(status.position >= Duration::from_millis(7_400), "{speed}x {status:?}");
            }
            let _ = sink.pipeline.set_state(gstreamer::State::Null);
        }
    }

    #[test]
    fn replay_resumes_at_speed_after_a_stall_and_after_a_pause() {
        let (sink, display) = test_sink(None);
        sink.reset_for_open(Duration::from_secs(60), 1.0);
        // Half a second of footage, then nothing for a second: the lead runs out.
        for i in 0..5u32 {
            sink.push_frame(&frame(i * 100_000, i == 0));
        }
        std::thread::sleep(Duration::from_millis(1_000));
        let before = rendered(&display);
        assert_eq!(before, 5);
        // Late frames are shown at the camera's pace from now, not all at once.
        for i in 5..25u32 {
            sink.push_frame(&frame(i * 100_000, false));
        }
        std::thread::sleep(Duration::from_millis(500));
        let shown = rendered(&display) - before;
        assert!((3..=8).contains(&shown), "0.5 s after a stall should show ~5 frames: {shown}");
        let position = sink.replay_status().expect("replay sink").position.as_secs_f64();
        assert!((60.7..=61.2).contains(&position), "the cursor counts footage, not the stall: {position}");

        // Pause: the picture holds.
        sink.set_paused(true);
        let held = rendered(&display);
        std::thread::sleep(Duration::from_millis(400));
        assert!(rendered(&display) <= held + 1, "paused but still rendering");
        // Resume = a new open at the position on screen, which plays again.
        sink.reset_for_open(Duration::from_millis(61_000), 1.0);
        let before = rendered(&display);
        for i in 0..20u32 {
            sink.push_frame(&frame(3_000_000 + i * 100_000, i % 10 == 0));
        }
        std::thread::sleep(Duration::from_millis(500));
        let shown = rendered(&display) - before;
        assert!((3..=8).contains(&shown), "0.5 s after resuming should show ~5 frames: {shown}");
        let _ = sink.pipeline.set_state(gstreamer::State::Null);
    }

    #[test]
    fn replay_end_of_footage_plays_out_and_a_new_open_starts_afresh() {
        let (sink, display) = test_sink(None);
        sink.reset_for_open(Duration::from_secs(10), 4.0);
        for i in 0..10u32 {
            sink.push_frame(&frame(i * 100_000, i == 0));
        }
        sink.end_of_footage();
        sink.push_frame(&frame(1_000_000, false)); // ignored
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(rendered(&display), 10);
        let status = sink.replay_status().expect("replay sink");
        assert_eq!(status.position, Duration::from_millis(10_900));
        assert_eq!(status.buffered, Duration::ZERO);
        sink.reset_for_open(Duration::from_secs(20), 1.0);
        let before = rendered(&display);
        for i in 0..20u32 {
            sink.push_frame(&frame(9_000_000 + i * 100_000, i % 10 == 0));
        }
        std::thread::sleep(Duration::from_millis(500));
        let shown = rendered(&display) - before;
        assert!((3..=8).contains(&shown), "0.5 s of a new open after the end should show ~5 frames: {shown}");
        let _ = sink.pipeline.set_state(gstreamer::State::Null);
    }

    #[test]
    fn slow_motion_asks_for_a_break_once_a_minute_of_footage_waits() {
        let (sink, _display) = test_sink(None);
        // 0.25x: the device keeps sending at 1x (10 fps here).
        sink.reset_for_open(Duration::ZERO, 0.25);
        for i in 0..300u32 {
            sink.push_frame(&frame(i * 100_000, i % 40 == 0));
        }
        assert!(!sink.replay_status().expect("replay sink").slow_motion_backlog, "30 s waiting is fine");
        for i in 300..700u32 {
            sink.push_frame(&frame(i * 100_000, i % 40 == 0));
        }
        let status = sink.replay_status().expect("replay sink");
        assert!(status.buffered >= REPLAY_SLOW_MOTION_MAX_BUFFERED, "buffered {:?}", status.buffered);
        assert!(status.slow_motion_backlog);
        // At 1x and faster the device paces itself: a large lead is no backlog.
        sink.reset_for_open(Duration::ZERO, 16.0);
        for i in 0..700u32 {
            sink.push_frame(&frame(i * 100_000, i % 40 == 0));
        }
        assert!(!sink.replay_status().expect("replay sink").slow_motion_backlog);
        let _ = sink.pipeline.set_state(gstreamer::State::Null);
    }

    #[test]
    fn a_torn_down_replay_sink_stays_down() {
        let (sink, _display) = test_sink(None);
        sink.reset_for_open(Duration::from_secs(10), 1.0);
        for i in 0..10u32 {
            sink.push_frame(&frame(i * 100_000, i == 0));
        }
        // The view lets go; the pushing thread still holds a clone.
        let held = sink.clone();
        assert!(!held.is_retired());
        tear_down(sink).join().expect("teardown thread");
        assert!(held.is_retired());
        assert_eq!(held.pipeline.current_state(), gstreamer::State::Null);
        // Nothing the pushing thread does brings it back.
        held.reset_for_open(Duration::from_secs(20), 1.0);
        held.set_paused(false);
        held.chain_open(Duration::from_secs(30));
        held.push_frame(&frame(2_000_000, true));
        held.end_of_footage();
        held.set_paused(true);
        let (_, current, pending) = held.pipeline.state(Some(gstreamer::ClockTime::from_mseconds(200)));
        assert_eq!((current, pending), (gstreamer::State::Null, gstreamer::State::VoidPending));
        assert_eq!(held.appsrc.current_level_bytes(), 0, "nothing is queued into a torn-down pipeline");
    }

    #[test]
    fn live_sinks_ignore_replay_controls() {
        gstreamer::init().expect("gstreamer init");
        let sink = VideoSink {
            appsrc: replay_appsrc(VideoType::H264),
            pipeline: gstreamer::Pipeline::new(),
            pts_state: Arc::new(Mutex::new(PtsState::default())),
            recorder: Arc::new(Mutex::new(None)),
            video_type: VideoType::H264,
            decoded_frames: Arc::new(AtomicU64::new(0)),
            replay: None,
        };
        sink.reset_for_open(Duration::from_secs(1), 2.0);
        sink.chain_open(Duration::from_secs(1));
        sink.end_of_footage();
        sink.set_paused(true);
        assert!(sink.replay_status().is_none());
        assert_eq!(sink.pipeline.current_state(), gstreamer::State::Null);
    }

    #[test]
    fn a_codec_change_rebuilds_the_sink() {
        // No sink yet: `ensure_sink` builds one, nothing to tear down.
        assert!(!needs_rebuild(None, VideoType::H264));
        assert!(!needs_rebuild(None, VideoType::H265));
        // The same codec keeps the pipeline (and what it holds).
        assert!(!needs_rebuild(Some(VideoType::H264), VideoType::H264));
        assert!(!needs_rebuild(Some(VideoType::H265), VideoType::H265));
        // Sub stream (H.264) to main stream (H.265) and back: a new pipeline.
        assert!(needs_rebuild(Some(VideoType::H264), VideoType::H265));
        assert!(needs_rebuild(Some(VideoType::H265), VideoType::H264));
    }
}
