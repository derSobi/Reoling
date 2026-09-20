//! Bounded, opt-in capture of decrypted video messages before BcMedia parsing.
//! This contains video and media extensions, never login messages or keys.
use crate::protocol::bc::{codec::write_bc, model::{Bc, MSG_ID_VIDEO}};
use crate::protocol::crypto::EncryptionProtocol;
use std::{io::{self, Write}, sync::Mutex, time::Instant};

const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_MESSAGES: usize = 65_536;

pub struct MediaTrace {
    started: Instant,
    data: Mutex<TraceData>,
}

#[derive(Default)]
struct TraceData {
    records: Vec<(u64, Vec<u8>)>,
    bytes: usize,
    truncated: bool,
}

impl Default for MediaTrace {
    fn default() -> Self {
        Self { started: Instant::now(), data: Mutex::new(TraceData::default()) }
    }
}

impl MediaTrace {
    pub(crate) fn record(&self, message: &Bc) {
        if message.meta.msg_id != MSG_ID_VIDEO { return; }
        let elapsed_us = self.started.elapsed().as_micros() as u64;
        let mut data = self.data.lock().unwrap();
        if data.truncated { return; }
        let bytes = write_bc(message, &EncryptionProtocol::Unencrypted);
        if data.bytes + bytes.len() > MAX_BYTES || data.records.len() >= MAX_MESSAGES {
            data.truncated = true;
            return;
        }
        data.bytes += bytes.len();
        data.records.push((elapsed_us, bytes));
    }

    /// Write after stopping video. Format: RLMTRACE1, truncated:u8,
    /// count:u32le, then (arrival_us:u64le, length:u32le, plaintext BC bytes).
    /// Returns the message count and whether the capture reached its limit.
    pub fn write_to(&self, mut output: impl Write) -> io::Result<(usize, bool)> {
        let data = self.data.lock().unwrap();
        output.write_all(b"RLMTRACE1")?;
        output.write_all(&[u8::from(data.truncated)])?;
        output.write_all(&(data.records.len() as u32).to_le_bytes())?;
        for (arrival, bytes) in &data.records {
            output.write_all(&arrival.to_le_bytes())?;
            output.write_all(&(bytes.len() as u32).to_le_bytes())?;
            output.write_all(bytes)?;
        }
        output.flush()?;
        Ok((data.records.len(), data.truncated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::bc::{codec::read_bc, model::*};
    use std::collections::HashSet;

    #[test]
    fn capture_excludes_login_and_preserves_media_message_boundaries() {
        let trace = MediaTrace::default();
        let mut message = Bc {
            meta: BcMeta { msg_id: MSG_ID_LOGIN, channel_id: 2, stream_type: 0,
                msg_num: 7, response_code: 200, class: 0x6414 },
            body: BcBody::Modern(ModernMsg { extension_xml: None,
                payload: Some(vec![1, 2, 3, 4]) }),
        };
        trace.record(&message);
        message.meta.msg_id = MSG_ID_VIDEO;
        trace.record(&message);
        let mut output = Vec::new();
        assert_eq!(trace.write_to(&mut output).unwrap(), (1, false));
        assert_eq!(&output[..9], b"RLMTRACE1");
        let length = u32::from_le_bytes(output[22..26].try_into().unwrap()) as usize;
        assert_eq!(length, output.len() - 26);
        let (restored, used) = read_bc(&output[26..], &EncryptionProtocol::Unencrypted,
            &mut HashSet::new()).unwrap().unwrap();
        assert_eq!(used, length);
        assert_eq!(restored.meta.channel_id, 2);
        assert_eq!(restored.meta.msg_num, 7);
        assert_eq!(restored.body, message.body);
    }

    #[test]
    fn capture_stops_at_byte_limit_and_reports_truncation() {
        let trace = MediaTrace::default();
        trace.data.lock().unwrap().bytes = MAX_BYTES;
        let message = Bc {
            meta: BcMeta { msg_id: MSG_ID_VIDEO, channel_id: 0, stream_type: 0,
                msg_num: 1, response_code: 200, class: 0x6414 },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: None }),
        };
        trace.record(&message);
        assert_eq!(trace.write_to(Vec::new()).unwrap(), (0, true));
    }
}
