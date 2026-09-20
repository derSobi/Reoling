//! Offline analysis tool: replays a `.rlmtrace` file (captured via
//! `probe_login_video --dump-bc`) through the exact same BcMedia decode
//! pipeline `client.rs`'s `start_video` uses, printing every parsed unit
//! (Info/Video/Skipped) with its capture-relative arrival timestamp. No
//! network access, no credentials — pure offline replay.
//! Usage: cargo run --example analyze_trace -- <path.rlmtrace>
use reoling::protocol::bc::codec::read_bc;
use reoling::protocol::bc::model::{BcBody, ModernMsg};
use reoling::protocol::bcmedia::model::{parse_one, BcMediaMessage};
use reoling::protocol::crypto::EncryptionProtocol;
use std::collections::HashSet;
use std::io::Read;

fn main() {
    let path = std::env::args().nth(1).expect("usage: analyze_trace <path.rlmtrace>");
    let mut file = std::fs::File::open(&path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let mut all = Vec::new();
    file.read_to_end(&mut all).expect("read trace file");

    let mut pos = 0usize;
    let magic = &all[pos..pos + 9];
    assert_eq!(magic, b"RLMTRACE1", "not an RLMTRACE1 file");
    pos += 9;
    let truncated = all[pos];
    pos += 1;
    let count = u32::from_le_bytes(all[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    println!("trace: {count} messages, truncated={truncated}");

    let mut buffer: Vec<u8> = Vec::new();
    let mut info_count = 0u32;
    let mut video_count = 0u32;
    let mut last_video_us: Option<u32> = None;

    for i in 0..count {
        let arrival_us = u64::from_le_bytes(all[pos..pos + 8].try_into().unwrap());
        pos += 8;
        let length = u32::from_le_bytes(all[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        let bc_bytes = &all[pos..pos + length];
        pos += length;

        let (bc, used) = read_bc(bc_bytes, &EncryptionProtocol::Unencrypted, &mut HashSet::new())
            .expect("read_bc failed")
            .expect("incomplete Bc message in trace record");
        assert_eq!(used, bc_bytes.len(), "trace record #{i} had trailing bytes");
        let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = bc.body else {
            continue;
        };

        buffer.extend_from_slice(&payload);
        loop {
            match parse_one(&buffer) {
                Ok(Some((msg, used))) => {
                    buffer.drain(..used);
                    match msg {
                        BcMediaMessage::Info { width, height } => {
                            info_count += 1;
                            println!(
                                "[record {i}] arrival={:.3}s  INFO #{info_count}: {width}x{height}",
                                arrival_us as f64 / 1e6
                            );
                        }
                        BcMediaMessage::Video(frame) => {
                            video_count += 1;
                            let step = last_video_us
                                .map(|last| frame.microseconds.wrapping_sub(last));
                            last_video_us = Some(frame.microseconds);
                            if let Some(step) = step {
                                if step > 200_000 {
                                    println!(
                                        "[record {i}] arrival={:.3}s  VIDEO #{video_count}: {} bytes, camera_us={}, keyframe={}, STEP={:.3}s (JUMP)",
                                        arrival_us as f64 / 1e6,
                                        frame.data.len(),
                                        frame.microseconds,
                                        frame.is_keyframe,
                                        step as f64 / 1e6,
                                    );
                                }
                            }
                        }
                        BcMediaMessage::Skipped => {}
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    println!("[record {i}] parse_one error: {e}");
                    break;
                }
            }
        }
    }

    println!("\ntotal: {info_count} Info units, {video_count} Video frames");
}
