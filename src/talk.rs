//! Sound to the camera (two-way audio): IMA/DVI-4 ADPCM, 16 kHz mono, in the
//! framing the official app uses (checked against its captured messages: each
//! message carries one 516-byte block inside a 528-byte BcMedia unit).

const STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66, 73,
    80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449, 494,
    544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272, 2499,
    2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493, 10442,
    11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];
const INDEX_TABLE: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

/// Samples one block carries: the first goes in the block header, the other
/// 1024 (`lengthPerEncoder` the camera announces) are the 512 bytes of nibbles.
pub const SAMPLES_PER_BLOCK: usize = 1025;
/// Header (4) + nibbles (512).
pub const BLOCK_BYTES: usize = 516;
/// The sample rate the camera announces for talking.
pub const SAMPLE_RATE: u32 = 16_000;

const MAGIC_ADPCM: u32 = 0x6277_3130;
const MAGIC_ADPCM_DATA: u16 = 0x0100;

/// Carries the ADPCM state from block to block.
#[derive(Debug, Default)]
pub struct AdpcmEncoder {
    predictor: i32,
    index: i32,
}

impl AdpcmEncoder {
    fn nibble(&mut self, sample: i16) -> u8 {
        let step = STEP_TABLE[self.index as usize];
        let mut diff = i32::from(sample) - self.predictor;
        let mut code = 0u8;
        if diff < 0 {
            code = 8;
            diff = -diff;
        }
        let mut delta = step >> 3;
        let mut s = step;
        if diff >= s {
            code |= 4;
            diff -= s;
            delta += step;
        }
        s >>= 1;
        if diff >= s {
            code |= 2;
            diff -= s;
            delta += step >> 1;
        }
        s >>= 1;
        if diff >= s {
            code |= 1;
            delta += step >> 2;
        }
        self.predictor = if code & 8 != 0 { self.predictor - delta } else { self.predictor + delta };
        self.predictor = self.predictor.clamp(-32768, 32767);
        self.index = (self.index + INDEX_TABLE[usize::from(code & 7)]).clamp(0, 88);
        code
    }

    /// One block from exactly `SAMPLES_PER_BLOCK` samples: the state at the
    /// start (the first sample as predictor, then the step index), then the
    /// remaining samples as nibbles, low nibble first.
    pub fn encode_block(&mut self, samples: &[i16]) -> Vec<u8> {
        assert_eq!(samples.len(), SAMPLES_PER_BLOCK, "a block takes {SAMPLES_PER_BLOCK} samples");
        self.predictor = i32::from(samples[0]);
        let mut block = Vec::with_capacity(BLOCK_BYTES);
        block.extend_from_slice(&(samples[0]).to_le_bytes());
        block.push(self.index as u8);
        block.push(0);
        for pair in samples[1..].chunks(2) {
            let low = self.nibble(pair[0]);
            let high = self.nibble(pair[1]);
            block.push(low | (high << 4));
        }
        block
    }
}

/// Decodes a block (for tests).
pub fn decode_block(block: &[u8]) -> Vec<i16> {
    let mut predictor = i32::from(i16::from_le_bytes([block[0], block[1]]));
    let mut index = i32::from(block[2]);
    let mut out = vec![predictor as i16];
    for byte in &block[4..] {
        for code in [byte & 0x0F, byte >> 4] {
            let step = STEP_TABLE[index as usize];
            let mut diff = step >> 3;
            if code & 4 != 0 {
                diff += step;
            }
            if code & 2 != 0 {
                diff += step >> 1;
            }
            if code & 1 != 0 {
                diff += step >> 2;
            }
            predictor = if code & 8 != 0 { predictor - diff } else { predictor + diff };
            predictor = predictor.clamp(-32768, 32767);
            index = (index + INDEX_TABLE[usize::from(code & 7)]).clamp(0, 88);
            out.push(predictor as i16);
        }
    }
    out
}

/// A block wrapped as the BcMedia unit the camera expects in a talk message.
pub fn adpcm_unit(block: &[u8]) -> Vec<u8> {
    let size = (block.len() + 4) as u16;
    let mut unit = Vec::with_capacity(block.len() + 12);
    unit.extend_from_slice(&MAGIC_ADPCM.to_le_bytes());
    unit.extend_from_slice(&size.to_le_bytes());
    unit.extend_from_slice(&size.to_le_bytes());
    unit.extend_from_slice(&MAGIC_ADPCM_DATA.to_le_bytes());
    // The official app writes 2 here for this camera family (some others
    // write half the block size).
    unit.extend_from_slice(&2u16.to_le_bytes());
    unit.extend_from_slice(block);
    unit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(len: usize, phase: usize) -> Vec<i16> {
        (0..len)
            .map(|i| {
                let t = (i + phase) as f64 / f64::from(SAMPLE_RATE);
                (f64::sin(2.0 * std::f64::consts::PI * 440.0 * t) * 12_000.0) as i16
            })
            .collect()
    }

    #[test]
    fn a_block_is_516_bytes_and_a_unit_528() {
        let block = AdpcmEncoder::default().encode_block(&tone(SAMPLES_PER_BLOCK, 0));
        assert_eq!(block.len(), BLOCK_BYTES);
        let unit = adpcm_unit(&block);
        assert_eq!(unit.len(), 528);
        // What the official app's messages start with.
        assert_eq!(&unit[..12], &[0x30, 0x31, 0x77, 0x62, 0x08, 0x02, 0x08, 0x02, 0x00, 0x01, 0x02, 0x00]);
    }

    #[test]
    fn a_tone_survives_encoding_closely() {
        let mut encoder = AdpcmEncoder::default();
        let mut phase = 0;
        let mut worst = 0i32;
        for _ in 0..4 {
            let samples = tone(SAMPLES_PER_BLOCK, phase);
            let decoded = decode_block(&encoder.encode_block(&samples));
            assert_eq!(decoded.len(), SAMPLES_PER_BLOCK);
            for (a, b) in samples.iter().zip(&decoded).skip(64) {
                worst = worst.max((i32::from(*a) - i32::from(*b)).abs());
            }
            phase += SAMPLES_PER_BLOCK;
        }
        // 4-bit ADPCM is lossy; a 12000-amplitude tone stays within a few
        // percent once the step size has adapted.
        assert!(worst < 1_200, "worst error {worst}");
    }
}
