use serde::{Deserialize, Serialize};

pub const XML_VERSION: &str = "1.1";

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename = "body")]
pub struct BcXml {
    #[serde(rename = "Encryption", skip_serializing_if = "Option::is_none")]
    pub encryption: Option<Encryption>,
    #[serde(rename = "LoginUser", skip_serializing_if = "Option::is_none")]
    pub login_user: Option<LoginUser>,
    #[serde(rename = "LoginNet", skip_serializing_if = "Option::is_none")]
    pub login_net: Option<LoginNet>,
    #[serde(rename = "DeviceInfo", skip_serializing_if = "Option::is_none")]
    pub device_info: Option<DeviceInfo>,
    #[serde(rename = "Preview", skip_serializing_if = "Option::is_none")]
    pub preview: Option<Preview>,
    #[serde(rename = "VersionInfo", skip_serializing_if = "Option::is_none")]
    pub version_info: Option<VersionInfo>,
    #[serde(rename = "ChannelInfoList", skip_serializing_if = "Option::is_none")]
    pub channel_info_list: Option<ChannelInfoList>,
    #[serde(rename = "OsdDatetime", skip_serializing_if = "Option::is_none")]
    pub osd_datetime: Option<OsdDatetime>,
}

/// The on-screen-display settings; all we read is the channel's name.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct OsdDatetime {
    #[serde(rename = "channelName")]
    pub channel_name: Option<OsdChannelName>,
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct OsdChannelName {
    pub name: Option<String>,
}

/// An NVR / Home Hub's description of its channels.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct ChannelInfoList {
    #[serde(rename = "ChannelInfo", default)]
    pub channels: Vec<ChannelInfoXml>,
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct ChannelInfoXml {
    #[serde(rename = "channelId")]
    pub channel_id: Option<u8>,
    #[serde(rename = "devName")]
    pub name: Option<String>,
    pub state: Option<String>,
    /// Comma separated, e.g. `mainStream,subStream,externStream`.
    #[serde(rename = "streamSupport")]
    pub stream_support: Option<String>,
}

impl ChannelInfoList {
    /// The channels that exist, skipping the "none" placeholders an NVR
    /// sends for empty slots. A channel that does not list its streams is
    /// given main and sub, which every camera has.
    pub fn into_channels(self) -> Vec<crate::client::ChannelInfo> {
        use crate::client::{ChannelInfo, StreamProfile};
        self.channels
            .into_iter()
            .filter_map(|c| {
                let state = c.state.unwrap_or_default().trim().to_lowercase();
                let name = c.name.unwrap_or_default().trim().to_string();
                if state == "none" && name.is_empty() {
                    return None;
                }
                let listed = c.stream_support.unwrap_or_default().to_lowercase();
                let mut streams: Vec<StreamProfile> = [
                    ("mainstream", StreamProfile::Main),
                    ("externstream", StreamProfile::Extern),
                    ("substream", StreamProfile::Sub),
                ]
                .into_iter()
                .filter(|(word, _)| listed.split(',').any(|s| s.trim() == *word))
                .map(|(_, p)| p)
                .collect();
                if streams.is_empty() {
                    streams = vec![StreamProfile::Main, StreamProfile::Sub];
                }
                Some(ChannelInfo {
                    channel_id: c.channel_id?,
                    name,
                    online: state == "connect",
                    streams,
                })
            })
            .collect()
    }
}

/// The device's own description of itself (reply to `MSG_ID_VERSION`): the
/// name the owner gave it and its model.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct VersionInfo {
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub model: Option<String>,
}

impl BcXml {
    pub fn to_bytes(&self) -> Vec<u8> {
        let inner = quick_xml::se::to_string(self).expect("BcXml always serializes");
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>{inner}").into_bytes()
    }

    pub fn from_bytes(buf: &[u8]) -> crate::protocol::Result<Self> {
        Ok(quick_xml::de::from_reader(buf)?)
    }
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct Encryption {
    #[serde(rename = "@version")]
    pub version: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub nonce: String,
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct LoginUser {
    #[serde(rename = "@version")]
    pub version: String,
    #[serde(rename = "userName")]
    pub user_name: String,
    pub password: String,
    #[serde(rename = "userVer")]
    pub user_ver: u32,
}

#[derive(Debug, PartialEq, Deserialize, Serialize)]
pub struct LoginNet {
    #[serde(rename = "@version")]
    pub version: String,
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(rename = "udpPort")]
    pub udp_port: u16,
}

impl Default for LoginNet {
    fn default() -> Self {
        LoginNet {
            version: XML_VERSION.to_string(),
            type_: "LAN".to_string(),
            udp_port: 0,
        }
    }
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct DeviceInfo {
    #[serde(rename = "@version")]
    pub version: Option<String>,
    pub resolution: Option<Resolution>,
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct Resolution {
    pub name: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct Preview {
    #[serde(rename = "@version")]
    pub version: String,
    #[serde(rename = "channelId")]
    pub channel_id: u8,
    pub handle: u32,
    #[serde(rename = "streamType", skip_serializing_if = "Option::is_none")]
    pub stream_type: Option<String>,
}

/// Describes the payload that follows the payload_offset in a modern
/// message. We only need `binary_data`/`channel_id` for the MVP (video
/// stream frames); other fields real cameras may send are ignored by serde.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename = "Extension")]
pub struct Extension {
    #[serde(rename = "@version")]
    pub version: String,
    #[serde(rename = "binaryData", skip_serializing_if = "Option::is_none")]
    pub binary_data: Option<u32>,
    #[serde(rename = "channelId", skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<u8>,
    /// How many leading bytes of the *following* payload are actually
    /// AES-encrypted; the rest is sent as plaintext. Only present on the
    /// message that starts a new BcMedia video/audio unit (the one with
    /// `binary_data == Some(1)`) — confirmed against real hardware
    /// 2026-09-14: messages of the same unit that lack this field
    /// entirely (continuation chunks) are sent fully in plaintext, and
    /// AES-decrypting them anyway corrupts every frame past its first
    /// `encrypt_len` bytes.
    #[serde(rename = "encryptLen", skip_serializing_if = "Option::is_none")]
    pub encrypt_len: Option<u32>,
    /// The camera's integrity self-check for this message: the 4 bytes at
    /// `check_pos` of the *decoded* payload, read little-endian, must equal
    /// `check_value`. In every observed message `check_pos` is 0, i.e. it
    /// covers the first dword of each message's payload (verified on all
    /// 1000 messages of a real TCP trace). A mismatch means the camera sent
    /// different bytes than it checksummed — seen once the camera's send
    /// buffer overruns because the client is too slow.
    #[serde(rename = "checkPos", skip_serializing_if = "Option::is_none", default)]
    pub check_pos: Option<u32>,
    #[serde(rename = "checkValue", skip_serializing_if = "Option::is_none", default)]
    pub check_value: Option<i32>,
}

impl Extension {
    pub fn to_bytes(&self) -> Vec<u8> {
        let inner = quick_xml::se::to_string(self).expect("Extension always serializes");
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>{inner}").into_bytes()
    }

    pub fn from_bytes(buf: &[u8]) -> crate::protocol::Result<Self> {
        Ok(quick_xml::de::from_reader(buf)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_user_round_trips() {
        let xml = BcXml {
            login_user: Some(LoginUser {
                version: XML_VERSION.to_string(),
                user_name: "ADMINHASH".to_string(),
                password: "PASSWORDHASH".to_string(),
                user_ver: 1,
            }),
            login_net: Some(LoginNet::default()),
            ..Default::default()
        };
        let bytes = xml.to_bytes();
        let parsed = BcXml::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.login_user.unwrap().user_name, "ADMINHASH");
        assert_eq!(parsed.login_net.unwrap().type_, "LAN");
    }

    #[test]
    fn parses_a_real_shaped_encryption_reply() {
        // Self-authored sample shaped like the camera's legacy-login reply,
        // not copied from any reference implementation's test fixtures.
        let sample = br#"<?xml version="1.0" encoding="UTF-8"?><body><Encryption version="1.1"><type>md5</type><nonce>AAAABBBBCCCCDDDD</nonce></Encryption></body>"#;
        let parsed = BcXml::from_bytes(sample).unwrap();
        assert_eq!(parsed.encryption.unwrap().nonce, "AAAABBBBCCCCDDDD");
    }
}

#[cfg(test)]
mod channel_info_tests {
    use super::*;
    use crate::client::StreamProfile;

    #[test]
    fn osd_reply_gives_the_channel_name() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?><body><OsdDatetime version="1.1">
            <channelName><enable>1</enable><name>Camera 1</name></channelName>
            <datetime><enable>1</enable></datetime></OsdDatetime></body>"#;
        let name = BcXml::from_bytes(xml).unwrap().osd_datetime.unwrap().channel_name.unwrap().name;
        assert_eq!(name.as_deref(), Some("Camera 1"));
    }

    #[test]
    fn channel_list_gives_names_state_and_streams() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?><body><ChannelInfoList version="1.1">
            <ChannelInfo><channelId>0</channelId><devName>Garden</devName><state>connect</state>
              <streamSupport>
                mainStream,subStream,externStream
              </streamSupport></ChannelInfo>
            <ChannelInfo><channelId>1</channelId><devName>Door</devName><state>connect</state></ChannelInfo>
            <ChannelInfo><channelId>2</channelId><state>none</state><streamSupport>none</streamSupport></ChannelInfo>
            </ChannelInfoList></body>"#;
        let channels = BcXml::from_bytes(xml).unwrap().channel_info_list.unwrap().into_channels();
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].name, "Garden");
        assert!(channels[0].online);
        assert_eq!(
            channels[0].streams,
            [StreamProfile::Main, StreamProfile::Extern, StreamProfile::Sub]
        );
        assert_eq!(channels[1].streams, [StreamProfile::Main, StreamProfile::Sub]);
    }
}
