use crate::protocol::bc::header_codec::{read_header, wire_len, write_header};
use crate::protocol::bc::model::{
    header_has_payload_offset, Bc, BcBody, BcHeader, BcMeta, LegacyMsg, ModernMsg, MSG_ID_COVER_PREVIEW,
    MSG_ID_DOWNLOAD, MSG_ID_IMAGE_UPLOAD, MSG_ID_LOGIN, MSG_ID_REPLAY, MSG_ID_TALK_DATA,
};
use crate::protocol::bc::xml::Extension;
use crate::protocol::crypto::EncryptionProtocol;
use crate::protocol::Error;
use std::collections::HashSet;

const BINARY_MARK: &[u8] = b"<binaryData>1</binaryData>";

/// Ids whose binary payloads each carry their own `binaryData=1` Extension
/// (see the binary-mode comment in `read_bc`). 8 is framed like playback
/// data although it has not been seen on the wire.
const RECORDED_MEDIA_MSG_IDS: [u32; 4] = [MSG_ID_REPLAY, 8, MSG_ID_DOWNLOAD, MSG_ID_COVER_PREVIEW];

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
                // Talk audio and image uploads (which announce binary data in
                // their extension) go out unencrypted: the official app's messages, captured,
                // start with a plain BcMedia magic while their extension is
                // encrypted. Only these: the camera's own video messages
                // encrypt their payload (partly, see `encryptLen`).
                let is_binary = (bc.meta.msg_id == MSG_ID_TALK_DATA || bc.meta.msg_id == MSG_ID_IMAGE_UPLOAD)
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
            // Only headers that actually carry the payload_offset word
            // (classes 0x6414 and 0x0000, and recorded-media data with a
            // sequence number in class/code) get `Some`; e.g. 0x6614 (the
            // encrypted reply to legacy login) is a modern body with no
            // offset word at all — giving it one here desyncs
            // write_header/read_header. Same rule as `read_header`.
            let payload_offset = if header_has_payload_offset(bc.meta.msg_id, bc.meta.class, bc.meta.response_code) {
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
    let consumed = wire_len(&header, header_len);
    if buf.len() < consumed {
        return Ok(None);
    }
    let body = &buf[header_len..total_len];

    // The header (never encrypted) is enough on its own to tell us which
    // login-phase (`MSG_ID_LOGIN`) reply we're looking at, and real
    // hardware doesn't decrypt those the way a naive reading of the
    // negotiated `enc` would suggest. Confirmed 2026-09-13 on real hardware
    // for the AES case (a nonce reply 0xdd with a non-zero lower byte read as
    // BCEncrypt, later login replies BCEncrypt): our AES-negotiated login
    // first failed to decrypt and then succeeded with these rules. The
    // lower-byte-0x00 (unencrypted) branch has not been seen on our devices:
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

    // Recorded-media data (class/code = playback sequence number) arrive
    // with a payload offset and so take the `Some` branch below, exactly
    // like a class 0x0000 message: Extension, then binary mode per
    // `binaryData`/`encryptLen`. `meta.class`/`meta.response_code` keep the
    // sequence number for `BcMeta::replay_seq`.
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
            // Recorded media (playback, download, cover preview) put their
            // own `binaryData=1` Extension on every data message, while
            // device pushes arrive under msg_num 0 with no Extension at all.
            // So for those ids binary mode is decided per message and never
            // stored in `bin_mode`: a playback answered under msg_num 0 must
            // not leave 0 marked binary and turn later pushes into
            // undecrypted bytes. Every other id (live video included) keeps
            // the per-msg_num rule.
            let per_message_binary = RECORDED_MEDIA_MSG_IDS.contains(&header.msg_id);
            if let (false, Some(extension)) = (per_message_binary, &parsed_extension) {
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
            let is_binary = if per_message_binary {
                parsed_extension.as_ref().is_some_and(|ext| ext.binary_data == Some(1))
            } else {
                bin_mode.contains(&header.msg_num)
            };

            let raw_payload = &body[offset..];
            let payload = if raw_payload.is_empty() {
                None
            } else if is_binary {
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
        // .plans/reoling-project.md).
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
    fn image_upload_payload_goes_out_in_the_clear_but_its_extension_does_not() {
        // Captured from the official app saving a preset's picture (420): the
        // JPEG bytes sit unencrypted after an encrypted extension.
        let key = EncryptionProtocol::Aes { key: [7u8; 16] };
        let jpeg = b"\xff\xd8\xff\xe0-jpeg-bytes-\xff\xd9".to_vec();
        let ext = b"<Extension version=\"1.1\"><binaryData>1</binaryData><channelId>0</channelId></Extension>".to_vec();
        let bc = Bc {
            meta: BcMeta { msg_id: MSG_ID_IMAGE_UPLOAD, channel_id: 0, stream_type: 0, msg_num: 3, response_code: 0, class: 0x6414 },
            body: BcBody::Modern(ModernMsg { extension_xml: Some(ext.clone()), payload: Some(jpeg.clone()) }),
        };
        let wire = write_bc(&bc, &key);
        assert!(wire.windows(jpeg.len()).any(|w| w == jpeg));
        assert!(!wire.windows(ext.len()).any(|w| w == ext));
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

#[cfg(test)]
mod replay_framing_tests {
    use super::*;
    use crate::protocol::bc::model::{MSG_ID_REPLAY_CLOSE, MSG_ID_VIDEO};
    use crate::protocol::bcmedia::model::parse_info_block;

    /// Sequence number of a playback seen on the wire, and its header words.
    const SEQ: u32 = 1_792_348_477;
    const SEQ_CLASS: u16 = 0x6ad5;
    const SEQ_CODE: u16 = 0x113d;

    fn aes() -> EncryptionProtocol {
        EncryptionProtocol::Aes { key: [0x5a; 16] }
    }

    fn ext_xml(fields: &str) -> Vec<u8> {
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<Extension version=\"1.1\">\n{fields}</Extension>\n")
            .into_bytes()
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + 3) as u8).collect()
    }

    /// One media message as a device sends it: the Extension encrypted, then
    /// the payload with only its first `encrypted_prefix` bytes encrypted
    /// (built by hand, since `write_bc` encrypts a payload in full).
    #[allow(clippy::too_many_arguments)]
    fn media_message(
        enc: &EncryptionProtocol,
        msg_id: u32,
        class: u16,
        response_code: u16,
        msg_num: u16,
        ext: &[u8],
        payload: &[u8],
        encrypted_prefix: usize,
    ) -> Vec<u8> {
        let ext_wire = enc.encrypt(0, ext);
        let offset = ext_wire.len() as u32;
        let mut body = ext_wire;
        let n = encrypted_prefix.min(payload.len());
        body.extend_from_slice(&enc.encrypt(offset, &payload[..n]));
        body.extend_from_slice(&payload[n..]);
        let header = BcHeader {
            msg_id,
            body_len: body.len() as u32,
            channel_id: 0x22,
            stream_type: 0x08,
            msg_num,
            response_code,
            class,
            payload_offset: Some(offset),
        };
        let mut wire = write_header(&header);
        wire.extend_from_slice(&body);
        wire
    }

    fn payload_of(bc: &Bc) -> &[u8] {
        match &bc.body {
            BcBody::Modern(ModernMsg { payload: Some(p), .. }) => p,
            other => panic!("expected a payload, got {other:?}"),
        }
    }

    fn info_block_payload() -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(b"1001");
        info.extend_from_slice(&32u32.to_le_bytes());
        info.extend_from_slice(&640u32.to_le_bytes());
        info.extend_from_slice(&360u32.to_le_bytes());
        info.extend_from_slice(&[0x01, 0x0a]);
        info.extend_from_slice(&[0x7e, 0x0a, 0x11, 0x06, 0x15, 0x07]);
        info.extend_from_slice(&[0x7e, 0x0a, 0x11, 0x07, 0x15, 0x05]);
        info.extend_from_slice(&[0, 0]);
        info
    }

    #[test]
    fn replay_data_header_is_24_bytes_with_the_seq_in_class_and_code() {
        let header = BcHeader {
            msg_id: MSG_ID_REPLAY,
            body_len: 0,
            channel_id: 0,
            stream_type: 0,
            msg_num: 0,
            response_code: SEQ_CODE,
            class: SEQ_CLASS,
            payload_offset: Some(0),
        };
        let bytes = write_header(&header);
        assert_eq!(bytes.len(), 24);
        assert_eq!(&bytes[16..20], &[0x3d, 0x11, 0xd5, 0x6a]);
        let (parsed, used) = read_header(&bytes).unwrap().unwrap();
        assert_eq!(used, 24);
        assert_eq!(parsed, header);
        assert_eq!(parsed.replay_seq(), Some(SEQ));
        // With only 20 bytes there, the reader waits for the offset word.
        assert!(read_header(&bytes[..20]).unwrap().is_none());
    }

    #[test]
    fn replay_data_with_encrypt_len_1024_decrypts_only_that_prefix() {
        let enc = aes();
        let payload = pattern(4000);
        let ext = ext_xml("<binaryData>1</binaryData>\n<encryptLen>1024</encryptLen>\n");
        let wire = media_message(&enc, MSG_ID_REPLAY, SEQ_CLASS, SEQ_CODE, 0, &ext, &payload, 1024);
        let (bc, used) = read_bc(&wire, &enc, &mut HashSet::new()).unwrap().unwrap();
        assert_eq!(used, wire.len());
        assert_eq!(bc.meta.replay_seq(), Some(SEQ));
        assert_eq!(payload_of(&bc), payload.as_slice());
    }

    #[test]
    fn replay_data_with_encrypt_len_equal_to_the_whole_payload() {
        // Audio units of 528 bytes come with encryptLen 528: all encrypted.
        let enc = aes();
        let payload = pattern(528);
        let ext = ext_xml("<binaryData>1</binaryData>\n<encryptLen>528</encryptLen>\n");
        let wire = media_message(&enc, MSG_ID_REPLAY, SEQ_CLASS, SEQ_CODE, 0, &ext, &payload, 528);
        let (bc, used) = read_bc(&wire, &enc, &mut HashSet::new()).unwrap().unwrap();
        assert_eq!(used, wire.len());
        assert_eq!(payload_of(&bc), payload.as_slice());
    }

    #[test]
    fn replay_data_with_encrypt_len_32_on_a_larger_payload() {
        let enc = aes();
        let payload = pattern(39_488);
        let ext = ext_xml("<binaryData>1</binaryData>\n<encryptLen>32</encryptLen>\n");
        let wire = media_message(&enc, MSG_ID_REPLAY, SEQ_CLASS, SEQ_CODE, 0, &ext, &payload, 32);
        let (bc, used) = read_bc(&wire, &enc, &mut HashSet::new()).unwrap().unwrap();
        assert_eq!(used, wire.len());
        assert_eq!(payload_of(&bc), payload.as_slice());
    }

    #[test]
    fn capital_e_encrypt_len_is_honoured() {
        // Cover-preview data spell it `EncryptLen`; without reading that
        // spelling the I-frame's first 1024 bytes would stay encrypted.
        let enc = aes();
        let payload = pattern(3000);
        let ext = ext_xml("<binaryData>1</binaryData>\n<EncryptLen>1024</EncryptLen>\n");
        let wire = media_message(&enc, MSG_ID_COVER_PREVIEW, 0x0000, 200, 0, &ext, &payload, 1024);
        let (bc, _) = read_bc(&wire, &enc, &mut HashSet::new()).unwrap().unwrap();
        assert_eq!(bc.meta.replay_seq(), None);
        assert_eq!(payload_of(&bc), payload.as_slice());
    }

    #[test]
    fn a_playback_stream_parses_back_to_back_without_losing_framing() {
        let enc = aes();
        let info = info_block_payload();
        let data_a = pattern(1500);
        let data_b = pattern(777);
        let mut stream = Vec::new();
        // Open-OK: class 0x0000, code 200, binaryData only, 32 bytes.
        let open_ok = media_message(&enc, MSG_ID_REPLAY, 0x0000, 200, 0, &ext_xml("<binaryData>1</binaryData>\n"), &info, 0);
        stream.extend_from_slice(&open_ok);
        let a = media_message(
            &enc,
            MSG_ID_REPLAY,
            SEQ_CLASS,
            SEQ_CODE,
            0,
            &ext_xml("<binaryData>1</binaryData>\n<encryptLen>32</encryptLen>\n"),
            &data_a,
            32,
        );
        stream.extend_from_slice(&a);
        // A message with binaryData only and no encryptLen is all plaintext.
        let b = media_message(&enc, MSG_ID_REPLAY, SEQ_CLASS, SEQ_CODE, 0, &ext_xml("<binaryData>1</binaryData>\n"), &data_b, 0);
        stream.extend_from_slice(&b);
        // Then the empty reply to a close.
        let close_reply = write_bc(
            &Bc {
                meta: BcMeta { msg_id: MSG_ID_REPLAY_CLOSE, channel_id: 0x23, stream_type: 0x08, msg_num: 0, response_code: 200, class: 0x0000 },
                body: BcBody::Modern(ModernMsg::default()),
            },
            &enc,
        );
        stream.extend_from_slice(&close_reply);

        let mut bin_mode = HashSet::new();
        let mut pos = 0;
        let (first, used) = read_bc(&stream[pos..], &enc, &mut bin_mode).unwrap().unwrap();
        assert_eq!(used, open_ok.len());
        assert_eq!((first.meta.class, first.meta.response_code, first.meta.replay_seq()), (0x0000, 200, None));
        let block = parse_info_block(payload_of(&first)).expect("an info block");
        assert_eq!((block.width, block.height), (640, 360));
        pos += used;

        let (second, used) = read_bc(&stream[pos..], &enc, &mut bin_mode).unwrap().unwrap();
        assert_eq!(used, a.len());
        assert_eq!(second.meta.replay_seq(), Some(SEQ));
        assert_eq!(payload_of(&second), data_a.as_slice());
        pos += used;

        let (third, used) = read_bc(&stream[pos..], &enc, &mut bin_mode).unwrap().unwrap();
        assert_eq!(used, b.len());
        assert_eq!(third.meta.replay_seq(), Some(SEQ));
        assert_eq!(payload_of(&third), data_b.as_slice());
        pos += used;

        let (fourth, used) = read_bc(&stream[pos..], &enc, &mut bin_mode).unwrap().unwrap();
        assert_eq!(fourth.meta.msg_id, MSG_ID_REPLAY_CLOSE);
        assert_eq!(fourth.meta.response_code, 200);
        assert_eq!(pos + used, stream.len());
    }

    #[test]
    fn playback_data_do_not_mark_msg_num_0_binary_for_later_pushes() {
        // Pushes (channel list, alarms, ...) come under msg_num 0 with no
        // Extension; playback data may come under msg_num 0 too.
        let enc = aes();
        let mut bin_mode = HashSet::new();
        let open_ok = media_message(&enc, MSG_ID_REPLAY, 0x0000, 200, 0, &ext_xml("<binaryData>1</binaryData>\n"), &info_block_payload(), 0);
        let data = media_message(
            &enc,
            MSG_ID_REPLAY,
            SEQ_CLASS,
            SEQ_CODE,
            0,
            &ext_xml("<binaryData>1</binaryData>\n<encryptLen>32</encryptLen>\n"),
            &pattern(100),
            32,
        );
        for wire in [&open_ok, &data] {
            read_bc(wire, &enc, &mut bin_mode).unwrap().unwrap();
        }
        assert!(bin_mode.is_empty());

        let xml = b"<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<AlarmEventList version=\"1.1\">\n</AlarmEventList>\n</body>\n".to_vec();
        let push = Bc {
            meta: BcMeta { msg_id: 33, channel_id: 0, stream_type: 0, msg_num: 0, response_code: 200, class: 0x0000 },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: Some(xml.clone()) }),
        };
        let (parsed, _) = read_bc(&write_bc(&push, &enc), &enc, &mut bin_mode).unwrap().unwrap();
        assert_eq!(payload_of(&parsed), xml.as_slice(), "the push is decrypted as usual");
    }

    #[test]
    fn a_download_reply_without_extension_is_decrypted_xml() {
        let enc = aes();
        let mut bin_mode = HashSet::from([0u16]);
        let xml = b"<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n</body>\n".to_vec();
        let reply = Bc {
            meta: BcMeta { msg_id: MSG_ID_DOWNLOAD, channel_id: 0, stream_type: 0, msg_num: 0, response_code: 200, class: 0x0000 },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: Some(xml.clone()) }),
        };
        let (parsed, _) = read_bc(&write_bc(&reply, &enc), &enc, &mut bin_mode).unwrap().unwrap();
        assert_eq!(payload_of(&parsed), xml.as_slice());
    }

    #[test]
    fn live_video_headers_are_framed_as_before() {
        // msg 3 class 0x0000 / 0x6414: 24 bytes, no seq.
        for class in [0x0000u16, 0x6414] {
            let header = BcHeader {
                msg_id: MSG_ID_VIDEO,
                body_len: 0,
                channel_id: 0,
                stream_type: 0,
                msg_num: 9,
                response_code: 0,
                class,
                payload_offset: Some(0),
            };
            let (parsed, used) = read_header(&write_header(&header)).unwrap().unwrap();
            assert_eq!(used, 24);
            assert_eq!(parsed.replay_seq(), None);
        }
        // msg 3 with a class that would be a seq on msg 5 stays 20 bytes.
        let header = BcHeader {
            msg_id: MSG_ID_VIDEO,
            body_len: 0,
            channel_id: 0,
            stream_type: 0,
            msg_num: 9,
            response_code: SEQ_CODE,
            class: SEQ_CLASS,
            payload_offset: None,
        };
        let (parsed, used) = read_header(&write_header(&header)).unwrap().unwrap();
        assert_eq!(used, 20);
        assert_eq!(parsed.payload_offset, None);
        assert_eq!(parsed.replay_seq(), None);
    }

    #[test]
    fn write_bc_round_trips_a_replay_data_message() {
        let bc = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_REPLAY,
                channel_id: 0,
                stream_type: 0,
                msg_num: 0,
                response_code: SEQ_CODE,
                class: SEQ_CLASS,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: Some(ext_xml("<binaryData>1</binaryData>\n")),
                payload: Some(pattern(64)),
            }),
        };
        let enc = EncryptionProtocol::Unencrypted;
        let wire = write_bc(&bc, &enc);
        let (header, header_len) = read_header(&wire).unwrap().unwrap();
        assert_eq!(header_len, 24);
        assert_eq!(header.payload_offset, Some(ext_xml("<binaryData>1</binaryData>\n").len() as u32));
        let (parsed, used) = read_bc(&wire, &enc, &mut HashSet::new()).unwrap().unwrap();
        assert_eq!(used, wire.len());
        assert_eq!(parsed, bc);
    }
}
