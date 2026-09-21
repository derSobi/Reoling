//! The stream's sound: AAC (ADTS) frames from the device into the default
//! audio output, with a volume. The pipeline is built when the first frame
//! arrives and torn down with the stream.

use gstreamer::prelude::*;
use gstreamer_app::AppSrc;
use std::sync::Mutex;

/// Audio and video travel in separate pipelines; both hold their data back
/// for the configured latency (see `settings::latency`), which keeps sound
/// and picture together.
struct Running {
    appsrc: AppSrc,
    pipeline: gstreamer::Pipeline,
    volume: gstreamer::Element,
}

pub struct AudioOutput {
    running: Mutex<Option<Running>>,
    volume: Mutex<f64>,
    /// Building the pipeline failed once; do not try on every frame.
    unavailable: Mutex<bool>,
}

impl AudioOutput {
    pub fn new(volume: f64) -> Self {
        Self { running: Mutex::new(None), volume: Mutex::new(volume), unavailable: Mutex::new(false) }
    }

    fn build(volume: f64) -> Result<Running, String> {
        let make = |name: &str| {
            gstreamer::ElementFactory::make(name)
                .build()
                .map_err(|e| format!("{name} is missing: {e}"))
        };
        let appsrc = make("appsrc")?
            .downcast::<AppSrc>()
            .map_err(|_| "appsrc is not an AppSrc".to_string())?;
        appsrc.set_caps(Some(
            &gstreamer::Caps::builder("audio/mpeg")
                .field("mpegversion", 4i32)
                .field("stream-format", "adts")
                .build(),
        ));
        appsrc.set_is_live(true);
        appsrc.set_format(gstreamer::Format::Time);
        appsrc.set_do_timestamp(true);

        let parse = make("aacparse")?;
        let decoder = make("avdec_aac").or_else(|_| make("faad"))?;
        let convert = make("audioconvert")?;
        let resample = make("audioresample")?;
        let vol = make("volume")?;
        vol.set_property("volume", volume);
        let sink = make("autoaudiosink")?;

        let pipeline = gstreamer::Pipeline::new();
        let elements = [appsrc.upcast_ref(), &parse, &decoder, &convert, &resample, &vol, &sink];
        pipeline.add_many(elements).map_err(|e| e.to_string())?;
        gstreamer::Element::link_many(elements).map_err(|e| e.to_string())?;
        pipeline.set_latency(gstreamer::ClockTime::from_mseconds(
            crate::ui::settings::latency().as_millis() as u64,
        ));
        pipeline.set_state(gstreamer::State::Playing).map_err(|e| e.to_string())?;
        Ok(Running { appsrc, pipeline, volume: vol })
    }

    /// One AAC frame. Silent while the volume is zero.
    pub fn push(&self, frame: &[u8]) {
        if *self.volume.lock().expect("volume mutex poisoned") <= 0.0 {
            return;
        }
        let mut running = self.running.lock().expect("audio mutex poisoned");
        if running.is_none() {
            if *self.unavailable.lock().expect("flag mutex poisoned") {
                return;
            }
            match Self::build(*self.volume.lock().expect("volume mutex poisoned")) {
                Ok(r) => *running = Some(r),
                Err(e) => {
                    eprintln!("no audio output: {e}");
                    *self.unavailable.lock().expect("flag mutex poisoned") = true;
                    return;
                }
            }
        }
        if let Some(r) = running.as_ref() {
            let _ = r.appsrc.push_buffer(gstreamer::Buffer::from_slice(frame.to_vec()));
        }
    }

    pub fn set_volume(&self, volume: f64) {
        *self.volume.lock().expect("volume mutex poisoned") = volume;
        if let Some(r) = self.running.lock().expect("audio mutex poisoned").as_ref() {
            r.volume.set_property("volume", volume);
        }
    }

    /// Ends the sound of the current stream.
    pub fn reset(&self) {
        let old = self.running.lock().expect("audio mutex poisoned").take();
        if let Some(r) = old {
            // Off the caller's thread: taking a pipeline down can block.
            std::thread::spawn(move || {
                let _ = r.pipeline.set_state(gstreamer::State::Null);
            });
        }
    }
}

/// The microphone, as ADPCM blocks for the camera (see `reoling::talk`).
pub struct Microphone {
    pipeline: gstreamer::Pipeline,
}

impl Microphone {
    /// Starts capturing; `on_block` gets each finished block (516 bytes) on a
    /// GStreamer thread.
    pub fn start(on_block: impl Fn(Vec<u8>) + Send + 'static) -> Result<Self, String> {
        use reoling::talk::{AdpcmEncoder, SAMPLES_PER_BLOCK, SAMPLE_RATE};
        let make = |name: &str| {
            gstreamer::ElementFactory::make(name)
                .build()
                .map_err(|e| format!("{name} is missing: {e}"))
        };
        let source = make("autoaudiosrc")?;
        let convert = make("audioconvert")?;
        let resample = make("audioresample")?;
        let caps = gstreamer::Caps::builder("audio/x-raw")
            .field("format", "S16LE")
            .field("rate", SAMPLE_RATE as i32)
            .field("channels", 1i32)
            .build();
        let sink = gstreamer_app::AppSink::builder().caps(&caps).sync(false).build();

        let mut pending: Vec<i16> = Vec::new();
        let mut encoder = AdpcmEncoder::default();
        sink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gstreamer::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gstreamer::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gstreamer::FlowError::Error)?;
                    pending.extend(map.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
                    while pending.len() >= SAMPLES_PER_BLOCK {
                        let block: Vec<i16> = pending.drain(..SAMPLES_PER_BLOCK).collect();
                        on_block(encoder.encode_block(&block));
                    }
                    Ok(gstreamer::FlowSuccess::Ok)
                })
                .build(),
        );

        let pipeline = gstreamer::Pipeline::new();
        let elements = [&source, &convert, &resample, sink.upcast_ref()];
        pipeline.add_many(elements).map_err(|e| e.to_string())?;
        gstreamer::Element::link_many(elements).map_err(|e| e.to_string())?;
        pipeline.set_state(gstreamer::State::Playing).map_err(|e| e.to_string())?;
        Ok(Self { pipeline })
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        let pipeline = self.pipeline.clone();
        // Off the caller's thread: taking a pipeline down can block.
        std::thread::spawn(move || {
            let _ = pipeline.set_state(gstreamer::State::Null);
        });
    }
}
