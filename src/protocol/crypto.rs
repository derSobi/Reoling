use aes::cipher::{AsyncStreamCipher, KeyIvInit};
use aes::Aes128;
use cfb_mode::{Decryptor, Encryptor};
use md5::{Digest, Md5};

type Aes128CfbEnc = Encryptor<Aes128>;
type Aes128CfbDec = Decryptor<Aes128>;

/// Fixed IV used by the camera's AES-128-CFB control-channel encryption.
const AES_IV: &[u8; 16] = b"0123456789abcdef";

/// Derives the AES-128 key from the login password and the nonce the camera
/// sent during the legacy login reply.
///
/// This is *not* the raw MD5 digest of `password + nonce` (an earlier
/// version of this function assumed that, and it was never actually
/// exercised against real hardware until 2026-09-13's first real login
/// attempt, which failed — the decrypted DeviceInfo reply came back as
/// garbage/invalid UTF-8). The real key phrase is `"{nonce}-{password}"`
/// (nonce first, joined with a literal `-`), and the key is not the raw
/// digest bytes but the first 16 bytes of the digest's *uppercase hex
/// string representation* — confirmed by reading (not copying) the
/// equivalent step in `neolink`'s `make_aeskey`, since neolink is a
/// proven-working implementation and no other ground truth for this exact
/// step was available.
pub fn aes_key_from_password(password: &str, nonce: &str) -> [u8; 16] {
    let key_phrase = format!("{nonce}-{password}");
    let digest = Md5::digest(key_phrase.as_bytes());
    let hex_upper: Vec<u8> = digest.iter().flat_map(|b| format!("{b:02X}").into_bytes()).collect();
    let mut key = [0u8; 16];
    key.copy_from_slice(&hex_upper[..16]);
    key
}

pub fn aes_encrypt(key: [u8; 16], buf: &[u8]) -> Vec<u8> {
    let mut data = buf.to_vec();
    Aes128CfbEnc::new(key.as_slice().into(), AES_IV.as_slice().into()).encrypt(&mut data);
    data
}

pub fn aes_decrypt(key: [u8; 16], buf: &[u8]) -> Vec<u8> {
    let mut data = buf.to_vec();
    Aes128CfbDec::new(key.as_slice().into(), AES_IV.as_slice().into()).decrypt(&mut data);
    data
}

/// The encryption scheme negotiated during login. `Aes` covers the control
/// channel only; the video stream itself is unencrypted for the cameras we
/// target in the MVP.
#[derive(Debug, Clone)]
pub enum EncryptionProtocol {
    Unencrypted,
    BcEncrypt,
    Aes { key: [u8; 16] },
}

impl EncryptionProtocol {
    pub fn encrypt(&self, offset: u32, buf: &[u8]) -> Vec<u8> {
        match self {
            EncryptionProtocol::Unencrypted => buf.to_vec(),
            EncryptionProtocol::BcEncrypt => bc_xor(offset, buf),
            EncryptionProtocol::Aes { key } => aes_encrypt(*key, buf),
        }
    }

    pub fn decrypt(&self, offset: u32, buf: &[u8]) -> Vec<u8> {
        match self {
            EncryptionProtocol::Unencrypted => buf.to_vec(),
            EncryptionProtocol::BcEncrypt => bc_xor(offset, buf),
            EncryptionProtocol::Aes { key } => aes_decrypt(*key, buf),
        }
    }
}

/// Fixed 8-byte XOR key used by the "BCEncrypt" scheme (cameras/firmwares
/// from before ~2021). Byte `i` of the buffer is XORed with
/// `BC_XOR_KEY[(offset + i) % 8]` and then with `offset as u8` (the low byte
/// of the header's payload offset).
const BC_XOR_KEY: [u8; 8] = [0x1F, 0x2D, 0x3C, 0x4B, 0x5A, 0x69, 0x78, 0xFF];

/// Encrypts or decrypts (the scheme is symmetric) `buf` using BCEncrypt.
/// `offset` is the payload offset from the Bc header of the message this
/// buffer belongs to.
pub fn bc_xor(offset: u32, buf: &[u8]) -> Vec<u8> {
    let start = offset as usize % 8;
    let offset_byte = offset as u8;
    buf.iter()
        .enumerate()
        .map(|(i, b)| b ^ BC_XOR_KEY[(start + i) % 8] ^ offset_byte)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bc_xor_round_trips_at_various_offsets() {
        let plaintext = b"<?xml version=\"1.0\"?><body><Encryption/></body>".to_vec();
        for offset in [0u32, 1, 7, 8, 9, 1000] {
            let encrypted = bc_xor(offset, &plaintext);
            assert_ne!(encrypted, plaintext, "offset {offset} did not change the data");
            let decrypted = bc_xor(offset, &encrypted);
            assert_eq!(decrypted, plaintext, "offset {offset} did not round-trip");
        }
    }
}

#[cfg(test)]
mod aes_tests {
    use super::*;

    #[test]
    fn aes_round_trips() {
        let key = aes_key_from_password("swordfish", "1234567890");
        let plaintext = b"<?xml version=\"1.0\"?><body><LoginUser/></body>".to_vec();
        let encrypted = aes_encrypt(key, &plaintext);
        assert_ne!(encrypted, plaintext);
        let decrypted = aes_decrypt(key, &encrypted);
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn key_derivation_matches_the_real_algorithm() {
        // Independently computed (`md5(nonce + "-" + password)`, uppercase
        // hex, first 16 bytes) via `printf 'TESTNONCE-swordfish' | md5sum`,
        // not derived from this function's own code — pins the exact
        // algorithm (nonce-then-password order, hex string not raw digest)
        // that the earlier, untested version of this function got wrong.
        let key = aes_key_from_password("swordfish", "TESTNONCE");
        assert_eq!(key, *b"54248178201B47B4");
    }

    #[test]
    fn key_derivation_is_deterministic() {
        let a = aes_key_from_password("pw", "nonce");
        let b = aes_key_from_password("pw", "nonce");
        assert_eq!(a, b);
        let c = aes_key_from_password("pw", "different-nonce");
        assert_ne!(a, c);
    }
}
