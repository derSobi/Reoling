//! Manual diagnostic tool, not part of the app and never run by it: decrypts
//! a captured Baichuan (BC) session — one that shows the official app doing
//! something Reoling doesn't understand yet (e.g. PTZ Calibration or
//! Monitor Point) — using the exact same codec Reoling uses to talk to real
//! devices, so the decoding is as trustworthy as the app's own.
//!
//! Runs entirely on your own machine. The password is read from an
//! environment variable, never printed, never sent anywhere, and this tool
//! makes no network connection at all — it only reads the two local files
//! you give it.
//!
//! The capture must be a byte stream of Bc messages in each direction (not
//! a raw .pcap) — see `scripts/extract_bc_stream.py`, which produces exactly
//! that from a .pcap using `tshark`. See the top of that script for the full
//! usage. Short version:
//!
//! ```bash
//! python3 scripts/extract_bc_stream.py capture.pcap <device-ip> /tmp/bc-out
//! read -s -p "Camera password: " REOLING_CAPTURE_PASSWORD; export REOLING_CAPTURE_PASSWORD; echo
//! cargo run --example decrypt_bc_stream -- /tmp/bc-out/to_device.bin /tmp/bc-out/from_device.bin admin /tmp/bc-out/decoded.txt
//! ```
//!
//! Then read (or send) `/tmp/bc-out/decoded.txt` — a UID appearing in it is
//! blanked out, but nothing else is.

use reoling::protocol::bc::codec::read_bc;
use reoling::protocol::bc::model::{Bc, BcBody, BcMeta, ModernMsg, MSG_ID_LOGIN};
use reoling::protocol::bc::xml::BcXml;
use reoling::protocol::crypto::{aes_key_from_password, EncryptionProtocol};
use std::collections::HashSet;
use std::fmt::Write as _;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, to_device, from_device, username, out_path] = args.as_slice() else {
        eprintln!(
            "usage: decrypt_bc_stream <to_device.bin> <from_device.bin> <username> <out.txt>\n\
             (password comes from the REOLING_CAPTURE_PASSWORD environment variable)"
        );
        std::process::exit(1);
    };
    let password = std::env::var("REOLING_CAPTURE_PASSWORD").unwrap_or_else(|_| {
        eprintln!(
            "REOLING_CAPTURE_PASSWORD is not set. In your own terminal, run:\n  \
             read -s -p \"Camera password: \" REOLING_CAPTURE_PASSWORD; export REOLING_CAPTURE_PASSWORD; echo\n\
             then re-run this command in the SAME terminal."
        );
        std::process::exit(1);
    });

    let to_device_bytes = std::fs::read(to_device).unwrap_or_else(|e| {
        eprintln!("could not read {to_device}: {e}");
        std::process::exit(1);
    });
    let from_device_bytes = std::fs::read(from_device).unwrap_or_else(|e| {
        eprintln!("could not read {from_device}: {e}");
        std::process::exit(1);
    });

    // The nonce is in the legacy login's Encryption reply, from the device.
    // `read_bc` recognizes that reply from its header alone (see its own
    // doc comment), so the `enc` passed here doesn't matter yet.
    let Some(nonce) = find_nonce(&from_device_bytes) else {
        eprintln!(
            "could not find the login/nonce reply in {from_device} — this only works on a \
             capture that starts from a fresh connection (a direct or relay-direct session with \
             its own legacy login exchange), not one that reuses an already-open connection, \
             and not a pure UDP-relay session (its nonce travels in the P2P handshake, which \
             this tool does not parse)."
        );
        std::process::exit(1);
    };
    eprintln!("found the session nonce ({nonce}); deriving the AES key...");
    let key = aes_key_from_password(password.trim_end(), &nonce);
    let enc = EncryptionProtocol::Aes { key };

    let mut out = format!("# decrypted BC session, login account: {username}\n\n");
    decode_stream("-> device", &to_device_bytes, &enc, &mut out);
    decode_stream("<- device", &from_device_bytes, &enc, &mut out);

    std::fs::write(out_path, &out).unwrap_or_else(|e| {
        eprintln!("could not write {out_path}: {e}");
        std::process::exit(1);
    });
    eprintln!("wrote {out_path} ({} bytes)", out.len());
}

/// The nonce from the first legacy-login (`msg_id` 1) reply in `stream`.
/// `read_bc` decodes it correctly regardless of `enc` — that reply's
/// encoding is determined entirely by its own header (see `read_bc`'s doc
/// comment on `effective_enc`), so `Unencrypted` here is just a placeholder.
fn find_nonce(stream: &[u8]) -> Option<String> {
    let mut pos = 0;
    let mut bin_mode = HashSet::new();
    while let Ok(Some((bc, used))) = read_bc(&stream[pos..], &EncryptionProtocol::Unencrypted, &mut bin_mode) {
        if bc.meta.msg_id == MSG_ID_LOGIN {
            if let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = &bc.body {
                if let Ok(xml) = BcXml::from_bytes(payload) {
                    if let Some(nonce) = xml.encryption.map(|e| e.nonce) {
                        return Some(nonce);
                    }
                }
            }
        }
        pos += used;
    }
    None
}

/// Every message `read_bc` can pull out of `stream`, appended to `out` as
/// readable text. Stops (without failing the whole run) at the first
/// message it can't parse — a truncated or slightly corrupted capture still
/// yields whatever came before that point.
fn decode_stream(label: &str, stream: &[u8], enc: &EncryptionProtocol, out: &mut String) {
    let mut pos = 0;
    let mut bin_mode = HashSet::new();
    let mut count = 0;
    loop {
        match read_bc(&stream[pos..], enc, &mut bin_mode) {
            Ok(Some((bc, used))) => {
                append_message(label, &bc, out);
                pos += used;
                count += 1;
            }
            Ok(None) => break,
            Err(e) => {
                let _ = writeln!(out, "{label}: stopped after {count} messages: {e}\n");
                break;
            }
        }
    }
}

fn append_message(label: &str, bc: &Bc, out: &mut String) {
    let BcMeta { msg_id, channel_id, response_code, class, .. } = bc.meta;
    let _ = write!(out, "{label} msg_id={msg_id} channel={channel_id} code={response_code} class={class:#06x}");
    match &bc.body {
        BcBody::Modern(ModernMsg { extension_xml, payload }) => {
            if let Some(ext) = extension_xml {
                let _ = write!(out, " extension={}", redact(&text(ext)));
            }
            if let Some(payload) = payload {
                let _ = write!(out, " body={}", redact(&text(payload)));
            }
        }
        BcBody::Legacy(_) => {
            let _ = write!(out, " (legacy login step, no XML)");
        }
    }
    out.push('\n');
}

fn text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.replace('\n', " ").replace('\r', ""),
        Err(_) => format!("<{} bytes, not UTF-8 — likely binary data>", bytes.len()),
    }
}

/// Blanks out a `<uid>...</uid>` element, if present, without pulling in a
/// regex dependency for this one-off tool.
fn redact(text: &str) -> String {
    let (Some(start), Some(end)) = (text.find("<uid>"), text.find("</uid>")) else {
        return text.to_string();
    };
    if end < start {
        return text.to_string();
    }
    format!("{}<uid>…</uid>{}", &text[..start], &text[end + "</uid>".len()..])
}
