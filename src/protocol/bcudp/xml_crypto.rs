/// Stream-cipher key used to obfuscate the XML payload of UDP discovery
/// packets. Distinct from, and unrelated to, the BCEncrypt XOR key used for
/// TCP/data-channel XML (crypto::bc_xor).
const XML_KEY: [u32; 8] = [
    0x1f2d3c4b, 0x5a6c7f8d, 0x38172e4b, 0x8271635a, 0x863f1a2b, 0xa5c6f7d8, 0x8371e1b4, 0x17f2d3a5,
];

fn key_stream(offset: u32) -> impl Iterator<Item = u8> {
    XML_KEY
        .iter()
        .flat_map(move |word| (word.wrapping_add(offset)).to_le_bytes())
        .cycle()
}

pub fn decrypt(offset: u32, buf: &[u8]) -> Vec<u8> {
    buf.iter()
        .zip(key_stream(offset))
        .map(|(byte, key)| byte ^ key)
        .collect()
}

/// The cipher is symmetric.
pub fn encrypt(offset: u32, buf: &[u8]) -> Vec<u8> {
    decrypt(offset, buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_at_various_offsets() {
        let plaintext = b"<P2P><C2D_C><uid>012345</uid></C2D_C></P2P>".to_vec();
        for offset in [0u32, 1, 33, 12345] {
            let encrypted = encrypt(offset, &plaintext);
            assert_ne!(encrypted, plaintext);
            let decrypted = decrypt(offset, &encrypted);
            assert_eq!(decrypted, plaintext);
        }
    }
}
