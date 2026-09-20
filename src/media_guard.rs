//! Integrity guard for the video message stream.
//!
//! Every media message carries the camera's own `checkPos`/`checkValue`
//! self-check (see `Extension::check_value`). When it fails the camera sent
//! bytes other than the ones it checksummed, so the BcMedia unit being
//! assembled is damaged. Feeding it to the decoder shows up as green/garbled
//! blocks; instead the damaged unit is dropped and everything is skipped
//! until a message starts a new intra frame (or the stream-info unit that
//! precedes one).

use crate::protocol::bc::xml::Extension;

const INFO_V1: u32 = 0x31303031;
const INFO_V2: u32 = 0x32303031;
const IFRAME_FIRST: u32 = 0x63643030;
const IFRAME_LAST: u32 = 0x63643039;

/// What the caller should do with one media message.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Feed the payload to the BcMedia parser.
    Accept,
    /// Discard the payload without feeding the parser.
    Skip,
    /// The message failed its check: discard it, and also discard whatever
    /// partial unit the caller has buffered.
    Damaged,
}

#[derive(Default)]
pub struct MediaGuard {
    waiting_restart: bool,
    /// How many times the stream had to be resynchronised.
    pub resyncs: u32,
}

/// True if the extension's `checkPos`/`checkValue` do not hold for `payload`.
/// A message without both fields cannot fail.
pub fn check_fails(ext: Option<&Extension>, payload: &[u8]) -> Option<String> {
    let ext = ext?;
    let (pos, value) = (ext.check_pos? as usize, ext.check_value?);
    let actual = payload
        .get(pos..)
        .and_then(|tail| tail.get(..4))
        .map(|w| u32::from_le_bytes(w.try_into().unwrap()));
    if actual == Some(value as u32) {
        return None;
    }
    Some(format!(
        "checkPos={pos} expected={:#010x} actual={} payload_len={} binaryData={:?} encryptLen={:?}",
        value as u32,
        actual.map_or("out-of-range".to_string(), |a| format!("{a:#010x}")),
        payload.len(),
        ext.binary_data,
        ext.encrypt_len,
    ))
}

fn is_restart_point(payload: &[u8]) -> bool {
    payload.len() >= 4 && {
        let magic = u32::from_le_bytes(payload[..4].try_into().unwrap());
        matches!(magic, INFO_V1 | INFO_V2 | IFRAME_FIRST..=IFRAME_LAST)
    }
}

impl MediaGuard {
    pub fn judge(&mut self, ext: Option<&Extension>, payload: &[u8]) -> Verdict {
        if let Some(why) = check_fails(ext, payload) {
            self.resyncs += 1;
            self.waiting_restart = true;
            eprintln!("MEDIA check failed, resync #{} at next keyframe: {why}", self.resyncs);
            return Verdict::Damaged;
        }
        if self.waiting_restart {
            let starts_unit = ext.is_some_and(|e| e.binary_data == Some(1));
            if starts_unit && is_restart_point(payload) {
                self.waiting_restart = false;
            } else {
                return Verdict::Skip;
            }
        }
        Verdict::Accept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(binary: Option<u32>, check: Option<i32>) -> Extension {
        Extension {
            version: "1.1".into(),
            binary_data: binary,
            check_pos: check.map(|_| 0),
            check_value: check,
            ..Default::default()
        }
    }
    const KEY: [u8; 4] = 0x63643030u32.to_le_bytes();

    #[test]
    fn passing_and_unchecked_messages_are_accepted() {
        let mut g = MediaGuard::default();
        assert_eq!(g.judge(Some(&ext(Some(1), Some(0x63643030))), &KEY), Verdict::Accept);
        assert_eq!(g.judge(None, &[1, 2, 3]), Verdict::Accept);
        assert_eq!(g.resyncs, 0);
    }

    #[test]
    fn a_failed_check_drops_until_the_next_keyframe_start() {
        let mut g = MediaGuard::default();
        assert_eq!(g.judge(Some(&ext(None, Some(0x1234))), &KEY), Verdict::Damaged);
        // Orphaned continuation of the damaged unit, and a predictive-frame start: skipped.
        assert_eq!(g.judge(Some(&ext(None, Some(0x0403_0201))), &[1, 2, 3, 4, 5]), Verdict::Skip);
        let pframe = 0x63643130u32.to_le_bytes();
        assert_eq!(g.judge(Some(&ext(Some(1), Some(0x63643130))), &pframe), Verdict::Skip);
        // A keyframe start restarts the stream.
        assert_eq!(g.judge(Some(&ext(Some(1), Some(0x63643030))), &KEY), Verdict::Accept);
        assert_eq!(g.judge(None, &[9]), Verdict::Accept);
        assert_eq!(g.resyncs, 1);
    }

    #[test]
    fn an_out_of_range_check_position_counts_as_a_failure() {
        let mut e = ext(None, Some(5));
        e.check_pos = Some(10);
        assert!(check_fails(Some(&e), &[0; 8]).is_some());
    }
}
