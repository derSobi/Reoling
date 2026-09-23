use crate::protocol::bc::header_codec::{read_header, write_header};
use crate::protocol::bc::model::{
    has_payload_offset, Bc, BcBody, BcHeader, BcMeta, LegacyMsg, ModernMsg, MSG_ID_LOGIN, MSG_ID_TALK_DATA,
};
use crate::protocol::bc::xml::Extension;
use crate::protocol::crypto::EncryptionProtocol;
use crate::protocol::Error;
use std::collections::HashSet;

const BINARY_MARK: &[u8] = b"<binaryData>1</binaryData>";

pub fn write_bc(bc: &Bc, enc: &EncryptionProtocol) -> Vec<u8> {
    // Symmetric with `read_bc`'s override: the whole login exchange (both
    // directions) stays on BCEncrypt even once AES has been negotiated —
    // real firmware expects our modern-login request encoded that way too.
    let enc = match enc {
        EncryptionProtocol::Aes { .. } if bc.meta.msg_id == MSG_ID_LOGIN => {
            &EncryptionProtocol::BcEncrypt
        }
        other => other,
    };
    let (body_bytes, payload_offset) = match &bc.body {
        BcBody::Legacy(LegacyMsg::LoginUpgrade) => (Vec::new(), None),
        BcBody::Modern(modern) => {
            let mut body = Vec::new();
            let mut offset = 0u32;
            if let Some(ext) = &modern.extension_xml {
                let encrypted_ext = enc.encrypt(0, ext);
                offset = encrypted_ext.len() as u32;
                body.extend_from_slice(&encrypted_ext);
            }
            if let Some(payload) = &modern.payload {
                // Talk audio (which announces binary data in its extension)
                // goes out unencrypted: the official app's messages, captured,
                // start with a plain BcMedia magic while their extension is
                // encrypted. Only these: the camera's own video messages
                // encrypt their payload (partly, see `encryptLen`).
                let is_binary = bc.meta.msg_id == MSG_ID_TALK_DATA
                    && modern
                        .extension_xml
                        .as_deref()
                        .is_some_and(|e| e.windows(BINARY_MARK.len()).any(|w| w == BINARY_MARK));
                if is_binary {
                    body.extend_from_slice(payload);
                } else {
                    let encrypted_payload = enc.encrypt(offset, payload);
                    body.extend_from_slice(&encrypted_payload);
                }
            }
            // Only classes that actually carry the payload_offset word
            // (0x6414, 0x0000) get `Some`; e.g. 0x6614 (the encrypted reply
            // to legacy login) is a modern body with no offset word at all —
            // giving it one here desyncs write_header/read_header.
            let payload_offset = if has_payload_offset(bc.meta.class) {
                Some(offset)
            } else {
                None
            };
            (body, payload_offset)
        }
    };

    let header = BcHeader {
        msg_id: bc.meta.msg_id,
        body_len: body_bytes.len() as u32,
        channel_id: bc.meta.channel_id,
        stream_type: bc.meta.stream_type,
        msg_num: bc.meta.msg_num,
        response_code: bc.meta.response_code,
        class: bc.meta.class,
        payload_offset,
    };

    let mut out = write_header(&header);
    out.extend_from_slice(&body_bytes);
    out
}

pub fn read_bc(
    buf: &[u8],
    enc: &EncryptionProtocol,
    bin_mode: &mut HashSet<u16>,
) -> crate::protocol::Result<Option<(Bc, usize)>> {
    let Some((header, header_len)) = read_header(buf)? else {
        return Ok(None);
    };
    let total_len = header_len + header.body_len as usize;
    // `class == 0xffff` marks an error reply specific to `MSG_ID_IMAGE_FILE`
    // (confirmed 2026-09-23 against two real captures, both a preset
    // Reoling itself had saved and so never got a thumbnail file for:
    // asking to read a named image file that doesn't exist on the device
    // gets this back instead of an orderly "not found"). `response_code`
    // and `class` both come back `0xffff`, `body_len` truthfully `0`, but
    // the device still puts 4 extra zero bytes on the wire that its own
    // `body_len` doesn't account for. Left unskipped, those 4 bytes get
    // read as the next message's magic (always 0 — AES/BcEncrypt never
    // produces an all-zero ciphertext from a real header) and desync the
    // whole connection's framing.
    let consumed = if header.class == 0xffff { total_len + 4 } else { total_len };
    if buf.len() < consumed {
        return Ok(None);
    }
    let body = &buf[header_len..total_len];

    // The header (never encrypted) is enough on its own to tell us which
    // login-phase (`MSG_ID_LOGIN`) reply we're looking at, and real
    // hardware doesn't decrypt those the way a naive reading of the
    // negotiated `enc` would suggest — confirmed 2026-09-13 by comparing
    // against `neolink`'s `de.rs` (never copied, only read) after our own
    // AES-negotiated login failed to decrypt on real hardware:
    // - The very first login reply (the one carrying the nonce, in
    //   response to our request's `response_code` upper byte 0xdc) always
    //   comes back with upper byte 0xdd. Its lower byte is the encryption
    //   the device actually picked: 0x00 means genuinely unencrypted,
    //   anything else (0x01 BCEncrypt, 0x12 AES, ...) means BCEncrypt —
    //   never AES, since the AES key itself is derived from the nonce
    //   this very message is delivering, so it can't already be AES.
    // - Every other `MSG_ID_LOGIN` reply (e.g. the modern login's
    //   DeviceInfo) is *also* BCEncrypt whenever the negotiated protocol
    //   is AES: real firmware appears to keep the whole login exchange on
    //   BCEncrypt and only starts using AES for messages after login
    //   completes.
    let effective_enc = if header.msg_id == MSG_ID_LOGIN {
        if (header.response_code >> 8) & 0xff == 0xdd {
            if header.response_code & 0xff == 0 {
                EncryptionProtocol::Unencrypted
            } else {
                EncryptionProtocol::BcEncrypt
            }
        } else {
            match enc {
                EncryptionProtocol::Aes { .. } => EncryptionProtocol::BcEncrypt,
                other => other.clone(),
            }
        }
    } else {
        enc.clone()
    };
    let enc = &effective_enc;

    let meta = BcMeta {
        msg_id: header.msg_id,
        channel_id: header.channel_id,
        stream_type: header.stream_type,
        msg_num: header.msg_num,
        response_code: header.response_code,
        class: header.class,
    };

    let bc_body = match header.payload_offset {
        None if header.class == 0x6514 => BcBody::Legacy(LegacyMsg::LoginUpgrade),
        None => {
            // e.g. class 0x6614: modern body, no extension, whole body is payload.
            BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: if body.is_empty() {
                    None
                } else {
                    Some(enc.decrypt(0, body))
                },
            })
        }
        Some(offset) => {
            let offset = offset as usize;
            if offset > body.len() {
                return Err(Error::ProtocolError(format!(
                    "payload_offset {offset} beyond body_len {}",
                    body.len()
                )));
            }
            let extension_xml = if offset == 0 {
                None
            } else {
                Some(enc.decrypt(0, &body[..offset]))
            };
            let parsed_extension =
                extension_xml.as_ref().and_then(|xml| Extension::from_bytes(xml).ok());
            if let Some(extension) = &parsed_extension {
                match extension.binary_data {
                    Some(1) => {
                        bin_mode.insert(header.msg_num);
                    }
                    Some(0) => {
                        bin_mode.remove(&header.msg_num);
                    }
                    _ => {}
                }
            }

            let raw_payload = &body[offset..];
            let payload = if raw_payload.is_empty() {
                None
            } else if bin_mode.contains(&header.msg_num) {
                // Video/audio stream message. Real hardware only
                // AES-encrypts the leading `encryptLen` bytes of the
                // message that starts a new BcMedia unit (the one with
                // `binaryData=1`); every later message of that same unit
                // — even ones with their own `<Extension>` carrying just
                // `checkPos`/`checkValue`, no `binaryData` — is sent
                // fully in plaintext. Confirmed against real hardware
                // 2026-09-14: decrypting those unconditionally, as this
                // code used to do, XORs already-clear H.264/H.265 bytes
                // with the wrong keystream and corrupts every frame past
                // its first `encryptLen` bytes.
                match parsed_extension.as_ref().and_then(|ext| ext.encrypt_len).map(|n| n as usize)
                {
                    Some(n) if n < raw_payload.len() => {
                        let mut combined = enc.decrypt(offset as u32, &raw_payload[..n]);
                        combined.extend_from_slice(&raw_payload[n..]);
                        Some(combined)
                    }
                    Some(_) => Some(enc.decrypt(offset as u32, raw_payload)),
                    None => Some(raw_payload.to_vec()),
                }
            } else {
                Some(enc.decrypt(offset as u32, raw_payload))
            };
            BcBody::Modern(ModernMsg {
                extension_xml,
                payload,
            })
        }
    };

    Ok(Some((
        Bc {
            meta,
            body: bc_body,
        },
        consumed,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::bc::model::*;
    use crate::protocol::crypto::EncryptionProtocol;

    #[test]
    fn round_trips_a_legacy_login_upgrade() {
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: 1,
                response_code: 0xdc12,
                class: 0x6514,
            },
            body: BcBody::Legacy(LegacyMsg::LoginUpgrade),
        };
        let bytes = write_bc(&bc, &EncryptionProtocol::Unencrypted);
        assert_eq!(bytes.len(), 20); // header only, no body

        let (parsed, consumed) = read_bc(&bytes, &EncryptionProtocol::Unencrypted, &mut HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(consumed, 20);
        assert_eq!(parsed, bc);
    }

    #[test]
    fn legacy_login_upgrade_matches_a_real_captured_request_byte_for_byte() {
        // Captured 2026-09-13 from the official Windows client logging
        // into a real Home Hub Pro NVR over TCP:9000 (LAN) — see
        // .plans/docu/Wireshark/Reolink-ALL-Login_only_with_Home_Hub_Pro.pcapng,
        // frame 129. This is the exact request our own P2P/UDP client
        // (bug #15) was getting wrong: `response_code` was an invented
        // 0xdc02 instead of the real 0xdc12, and `channel_id` was being
        // set to the target camera's channel instead of 0 (login is a
        // host-level operation, even on an NVR — see `bug #14`/`#15` in
        // .plans/reolink-linux-project.md).
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: 0,
                response_code: 0xdc12,
                class: 0x6514,
            },
            body: BcBody::Legacy(LegacyMsg::LoginUpgrade),
        };
        let bytes = write_bc(&bc, &EncryptionProtocol::Unencrypted);
        assert_eq!(
            bytes,
            [
                0xf0, 0xde, 0xbc, 0x0a, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x12, 0xdc, 0x14, 0x65,
            ]
        );
    }

    #[test]
    fn round_trips_a_modern_message_with_payload_through_bc_encrypt() {
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: 1,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(b"<?xml version=\"1.0\"?><body/>".to_vec()),
            }),
        };
        let enc = EncryptionProtocol::BcEncrypt;
        let bytes = write_bc(&bc, &enc);
        let (parsed, consumed) = read_bc(&bytes, &enc, &mut HashSet::new()).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(parsed, bc);
    }

    #[test]
    fn login_messages_stay_on_bc_encrypt_even_once_aes_is_negotiated() {
        // Confirmed against a real capture, 2026-09-13 (a fresh Windows VM
        // install, login only, no prior state): the client's actual
        // LoginUser request and the device's DeviceInfo reply both decoded
        // as valid XML *only* under BCEncrypt, never AES, despite AES
        // being the negotiated protocol for the rest of the session. Every
        // other observed message id after login failed to decode as XML
        // under either Unencrypted or BCEncrypt, consistent with those
        // being genuinely AES-encrypted.
        let key = [0x42u8; 16];
        let aes = EncryptionProtocol::Aes { key };
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: 1,
                response_code: 200,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(b"<?xml version=\"1.0\"?><body><DeviceInfo/></body>".to_vec()),
            }),
        };

        // Encoding with the *negotiated* Aes protocol must still produce
        // BcEncrypt bytes on the wire for this msg_id.
        let bytes = write_bc(&bc, &aes);
        let bc_encrypt_bytes = write_bc(&bc, &EncryptionProtocol::BcEncrypt);
        assert_eq!(bytes, bc_encrypt_bytes);

        // Decoding those bytes while still passing the negotiated Aes
        // protocol must recover the original message (not garbage), i.e.
        // read_bc applies the same override on the way in.
        let (parsed, _) = read_bc(&bytes, &aes, &mut HashSet::new()).unwrap().unwrap();
        assert_eq!(parsed, bc);

        // A non-login message id is unaffected: it really does use Aes.
        let mut non_login = bc.clone();
        non_login.meta.msg_id = 999;
        let non_login_bytes = write_bc(&non_login, &aes);
        assert_ne!(non_login_bytes, write_bc(&non_login, &EncryptionProtocol::BcEncrypt));
    }

    #[test]
    fn returns_none_until_the_full_body_has_arrived() {
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: 1,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(b"<?xml version=\"1.0\"?><body/>".to_vec()),
            }),
        };
        let enc = EncryptionProtocol::Unencrypted;
        let bytes = write_bc(&bc, &enc);
        assert!(read_bc(&bytes[..bytes.len() - 1], &enc, &mut HashSet::new()).unwrap().is_none());
    }

    #[test]
    fn continuation_chunks_of_a_video_unit_with_no_encrypt_len_are_left_undecrypted() {
        // Regression test for a real-hardware bug (2026-09-14): a video/audio
        // BcMedia unit's *first* message carries an `<Extension>` with
        // `binaryData=1` and (usually) an `encryptLen` saying how many
        // leading bytes of its payload are actually AES-encrypted; every
        // later message of that same unit is sent fully in plaintext, some
        // of them with their own `<Extension>` (just `checkPos`/
        // `checkValue`, no `binaryData`). Decrypting those unconditionally
        // corrupted every real video frame past its first message.
        let key = [0x7u8; 16];
        let aes = EncryptionProtocol::Aes { key };
        let msg_num = 5;

        // Message 1: starts the unit. First 4 bytes of the payload are
        // genuinely encrypted; the last 4 are already plaintext, matching
        // what `encryptLen` promises — built by hand (not `write_bc`, which
        // always encrypts a payload in full) to control exactly which bytes
        // are ciphertext on the wire.
        let ext1 = Extension {
            version: "1.1".to_string(),
            binary_data: Some(1),
            channel_id: None,
            encrypt_len: Some(4),
            ..Default::default()
        }
        .to_bytes();
        let encrypted_ext1 = aes.encrypt(0, &ext1);
        let plain_payload1 = b"ABCDWXYZ".to_vec();
        let mut body1 = encrypted_ext1.clone();
        body1.extend_from_slice(&aes.encrypt(0, &plain_payload1[..4]));
        body1.extend_from_slice(&plain_payload1[4..]);
        let header1 = BcHeader {
            msg_id: MSG_ID_VIDEO,
            body_len: body1.len() as u32,
            channel_id: 0,
            stream_type: 0,
            msg_num,
            response_code: 0,
            class: 0x6414,
            payload_offset: Some(encrypted_ext1.len() as u32),
        };
        let mut bytes1 = write_header(&header1);
        bytes1.extend_from_slice(&body1);

        let mut bin_mode = HashSet::new();
        let (parsed1, _) = read_bc(&bytes1, &aes, &mut bin_mode).unwrap().unwrap();
        let BcBody::Modern(ModernMsg { payload: Some(payload1), .. }) = parsed1.body else {
            panic!("expected a modern payload");
        };
        assert_eq!(payload1, plain_payload1);

        // Message 2: a pure continuation of the same unit — its own
        // `<Extension>` has no `binaryData` (just a checkPos/checkValue
        // self-check), and its payload is *already* plaintext on the wire.
        let ext2 = Extension {
            version: "1.1".to_string(),
            binary_data: None,
            channel_id: None,
            encrypt_len: None,
            ..Default::default()
        }
        .to_bytes();
        let encrypted_ext2 = aes.encrypt(0, &ext2);
        let plain_payload2 = b"continuation-bytes-in-the-clear".to_vec();
        let mut body2 = encrypted_ext2.clone();
        body2.extend_from_slice(&plain_payload2);
        let header2 = BcHeader {
            msg_id: MSG_ID_VIDEO,
            body_len: body2.len() as u32,
            channel_id: 0,
            stream_type: 0,
            msg_num,
            response_code: 0,
            class: 0x6414,
            payload_offset: Some(encrypted_ext2.len() as u32),
        };
        let mut bytes2 = write_header(&header2);
        bytes2.extend_from_slice(&body2);

        let (parsed2, _) = read_bc(&bytes2, &aes, &mut bin_mode).unwrap().unwrap();
        let BcBody::Modern(ModernMsg { payload: Some(payload2), .. }) = parsed2.body else {
            panic!("expected a modern payload");
        };
        // The bug: AES-decrypting this already-clear payload would turn it
        // into garbage. It must come back untouched.
        assert_eq!(payload2, plain_payload2);
    }

    #[test]
    fn skips_the_four_stray_bytes_after_an_image_file_not_found_reply() {
        // Confirmed 2026-09-23 against two real captures: the device's
        // "no file by that name" reply for `MSG_ID_IMAGE_FILE` (asking to
        // read a preset's thumbnail that was never saved) declares
        // `body_len = 0` but still puts 4 extra zero bytes on the wire —
        // unskipped, those get read as the next message's magic (always 0)
        // and break parsing for everything after it on the connection.
        let not_found_header = BcHeader {
            msg_id: MSG_ID_IMAGE_FILE,
            body_len: 0,
            channel_id: 0,
            stream_type: 0,
            msg_num: 17,
            response_code: 0xffff,
            class: 0xffff,
            payload_offset: None,
        };
        let mut buf = write_header(&not_found_header);
        buf.extend_from_slice(&[0u8; 4]); // the stray bytes
        let next = Bc {
            meta: BcMeta { msg_id: 3, channel_id: 0, stream_type: 0, msg_num: 6, response_code: 200, class: 0x0000 },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: Some(b"frame".to_vec()) }),
        };
        buf.extend_from_slice(&write_bc(&next, &EncryptionProtocol::Unencrypted));

        let mut bin_mode = HashSet::new();
        let (first, consumed) = read_bc(&buf, &EncryptionProtocol::Unencrypted, &mut bin_mode).unwrap().unwrap();
        assert_eq!(first.meta.response_code, 0xffff);
        assert_eq!(first.meta.class, 0xffff);
        assert_eq!(consumed, 24, "the header's own 20 bytes plus the 4 stray ones");

        let (second, _) =
            read_bc(&buf[consumed..], &EncryptionProtocol::Unencrypted, &mut bin_mode).unwrap().unwrap();
        assert_eq!(second, next, "parsing resumes correctly after the stray bytes");
    }
}

#[cfg(test)]
mod talk_payload_tests {
    use super::*;

    #[test]
    fn talk_audio_payload_goes_out_unencrypted_but_its_extension_does_not() {
        let key = EncryptionProtocol::Aes { key: [7u8; 16] };
        let payload = b"01wb-plain-audio-block".to_vec();
        let ext = b"<Extension version=\"1.1\"><binaryData>1</binaryData><channelId>0</channelId></Extension>".to_vec();
        let mk = |msg_id| Bc {
            meta: BcMeta {
                msg_id,
                channel_id: 0,
                stream_type: 0,
                msg_num: 1,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg { extension_xml: Some(ext.clone()), payload: Some(payload.clone()) }),
        };
        let talk = write_bc(&mk(MSG_ID_TALK_DATA), &key);
        assert!(talk.windows(payload.len()).any(|w| w == payload), "talk payload is plain");
        assert!(!talk.windows(ext.len()).any(|w| w == ext), "its extension is encrypted");
        let other = write_bc(&mk(3), &key);
        assert!(!other.windows(payload.len()).any(|w| w == payload), "other payloads stay encrypted");
    }
}
