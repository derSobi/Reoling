//! Replays a `REOLING_DUMP_VIDEO` capture (`<file>` + `<file>.idx`) through
//! the same kind of live pipeline the app uses, at the camera's own pace,
//! and reports how many frames come out of the decoder and reach the sink.
//!
//! cargo run --release --example replay_dump -- /tmp/reoling-sub.h264 [decoder]
//!
//! `decoder` is a GStreamer element name (e.g. `qsvh264dec`, `vah264dec`,
//! `avdec_h264`); without it decodebin picks, as in the app. Env:
//! `LATENCY_MS` (default 1000), `NO_ANCHOR` (plain camera-cadence PTS).

use gstreamer::prelude::*;
use gstreamer_app::AppSrc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: replay_dump <dump file> [decoder]");
    let decoder_name = args.next();
    let latency_ms: u64 = std::env::var("LATENCY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
    let anchor_on = std::env::var("NO_ANCHOR").is_err();

    let data = std::fs::read(&path).expect("dump file");
    let idx = std::fs::read_to_string(format!("{path}.idx")).expect("idx file");
    let mut units = Vec::new();
    let mut offset = 0usize;
    for line in idx.lines() {
        let mut f = line.split_whitespace();
        let len: usize = f.next().unwrap().parse().unwrap();
        let _key = f.next();
        let us: u32 = f.next().unwrap().parse().unwrap();
        units.push((data[offset..offset + len].to_vec(), us));
        offset += len;
    }
    let h265 = path.contains("265");

    gstreamer::init().unwrap();
    let pipeline = gstreamer::Pipeline::new();
    let appsrc = gstreamer::ElementFactory::make("appsrc").build().unwrap().downcast::<AppSrc>().unwrap();
    appsrc.set_caps(Some(
        &gstreamer::Caps::builder(if h265 { "video/x-h265" } else { "video/x-h264" })
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build(),
    ));
    appsrc.set_is_live(true);
    appsrc.set_format(gstreamer::Format::Time);
    let parse = gstreamer::ElementFactory::make(if h265 { "h265parse" } else { "h264parse" }).build().unwrap();
    let queue = gstreamer::ElementFactory::make("queue")
        .property("max-size-buffers", 0u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 3_000_000_000u64)
        .build()
        .unwrap();
    let decoder = match &decoder_name {
        Some(name) => gstreamer::ElementFactory::make(name).build().expect("decoder element"),
        None => gstreamer::ElementFactory::make("decodebin").build().unwrap(),
    };
    let convert = gstreamer::ElementFactory::make("videoconvert").build().unwrap();
    let sink = gstreamer::ElementFactory::make("fakesink")
        .property("sync", true)
        .property("max-lateness", -1i64)
        .property("qos", std::env::var("SINK_QOS").is_ok())
        .build()
        .unwrap();
    if std::env::var("CONVERT_NO_QOS").is_ok() {
        convert.set_property("qos", false);
    }
    pipeline.add_many([appsrc.upcast_ref(), &parse, &queue, &decoder, &convert, &sink]).unwrap();
    gstreamer::Element::link_many([appsrc.upcast_ref(), &parse, &queue, &decoder]).unwrap();
    convert.link(&sink).unwrap();

    let decoded = Arc::new(AtomicU64::new(0));
    let tune = |element: &gstreamer::Element| {
        if let Some(factory) = element.factory() {
            let klass = factory.klass();
            if klass.contains("Decoder") && klass.contains("Video") {
                if std::env::var("KEEP_QOS").is_err() && element.find_property("qos").is_some() {
                    element.set_property("qos", false);
                }
                if element.find_property("compliance").is_some() {
                    element.set_property_from_str("compliance", "flexible");
                }
                eprintln!("decoder: {}", factory.name());
            }
        }
    };
    let count_out = |element: &gstreamer::Element, decoded: Arc<AtomicU64>| {
        let pad = element.static_pad("src").unwrap();
        pad.add_probe(gstreamer::PadProbeType::BUFFER, move |_, _| {
            decoded.fetch_add(1, Ordering::Relaxed);
            gstreamer::PadProbeReturn::Ok
        });
    };
    if decoder_name.is_some() {
        tune(&decoder);
        count_out(&decoder, decoded.clone());
        decoder.link(&convert).unwrap();
    } else {
        let bin = decoder.clone().downcast::<gstreamer::Bin>().unwrap();
        bin.connect_deep_element_added(move |_, _, element| tune(element));
        let convert_sink = convert.static_pad("sink").unwrap();
        let d = decoded.clone();
        decoder.connect_pad_added(move |_, pad| {
            let d = d.clone();
            pad.add_probe(gstreamer::PadProbeType::BUFFER, move |_, _| {
                d.fetch_add(1, Ordering::Relaxed);
                gstreamer::PadProbeReturn::Ok
            });
            if !convert_sink.is_linked() {
                let _ = pad.link(&convert_sink);
            }
        });
    }
    pipeline.set_latency(gstreamer::ClockTime::from_mseconds(latency_ms));
    pipeline.set_state(gstreamer::State::Playing).unwrap();

    let start = Instant::now();
    let first_us = units[0].1;
    let mut last_raw: Option<u32> = None;
    let mut acc: u64 = 0;
    let mut last_step: u64 = 0;
    let mut anchor: Option<i64> = None;
    let mut last_pts: Option<u64> = None;
    let mut next_report = Instant::now() + Duration::from_secs(1);
    for (n, (bytes, us)) in units.iter().enumerate() {
        // Arrive at the camera's own pace.
        let due = Duration::from_micros(us.wrapping_sub(first_us) as u64);
        if let Some(wait) = due.checked_sub(start.elapsed()) {
            std::thread::sleep(wait);
        }
        if let Some(raw) = last_raw {
            let step = us.wrapping_sub(raw) as u64;
            let step = if step == 0 || step > 5_000_000 { if last_step > 0 { last_step } else { 40_000 } } else { last_step = step; step };
            acc += step;
        }
        last_raw = Some(*us);
        let pts = if anchor_on {
            if let Some(rt) = pipeline.current_running_time().filter(|t| t.useconds() > 0) {
                let off = rt.useconds() as i64 - acc as i64;
                anchor = Some(anchor.map_or(off, |a| a.min(off)));
            }
            let wanted = anchor.map_or(acc as i64, |a| acc as i64 + a).max(0) as u64;
            let rising = last_pts.map_or(wanted, |l| wanted.max(l + 1));
            last_pts = Some(rising);
            rising
        } else {
            acc
        };
        let mut buffer = gstreamer::Buffer::from_slice(bytes.clone());
        buffer.get_mut().unwrap().set_pts(gstreamer::ClockTime::from_useconds(pts));
        appsrc.push_buffer(buffer).unwrap();
        if Instant::now() >= next_report {
            next_report += Duration::from_secs(1);
            let stats = sink.property::<gstreamer::Structure>("stats");
            println!(
                "t={:>4.1}s pushed {:>4} decoded {:>4} rendered {:>4} dropped {}",
                start.elapsed().as_secs_f32(),
                n + 1,
                decoded.load(Ordering::Relaxed),
                stats.get::<u64>("rendered").unwrap_or(0),
                stats.get::<u64>("dropped").unwrap_or(0)
            );
        }
    }
    std::thread::sleep(Duration::from_secs(2));
    let stats = sink.property::<gstreamer::Structure>("stats");
    println!(
        "END pushed {} decoded {} rendered {} dropped {}",
        units.len(),
        decoded.load(Ordering::Relaxed),
        stats.get::<u64>("rendered").unwrap_or(0),
        stats.get::<u64>("dropped").unwrap_or(0)
    );
    pipeline.set_state(gstreamer::State::Null).unwrap();
}
