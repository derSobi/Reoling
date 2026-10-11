//! A camera's video encoder settings (resolution, frame rate, bitrate,
//! codec, I-frame interval per stream) and the values it allows.
//!
//! Read with message 56 (`Compression`, per channel), written back with 57.
//! The allowed values are the `StreamInfoList` a device sends in its login
//! reply: a list of profiles, each a combination of a main and a sub
//! resolution with the frame rates and bitrates allowed for both, for the
//! channels in its `channelBits`. Where no profile covers a channel, the
//! per-channel request 730 returns the same structure (a Home Hub answers
//! it; an NVR, whose login list covers every channel, refuses it).
//!
//! Writing echoes the device's own reply with the changed values patched in
//! place, so fields this client doesn't know survive untouched.

/// One stream's current settings, as the device reports them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamEncoding {
    pub width: u32,
    pub height: u32,
    /// Frames per second.
    pub frame_rate: u32,
    /// Kbit/s.
    pub bitrate: u32,
    /// 0 = H.264, 1 = H.265.
    pub codec: u32,
    /// I-frame interval in seconds ("1x", "2x"… in the official app).
    pub gop: Option<GopRange>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GopRange {
    pub current: u32,
    pub min: u32,
    pub max: u32,
}

/// Which of a channel's streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EncodedStream {
    Main,
    Sub,
    /// `thirdStream` in 56/57, `externStream` in the allowed-values list.
    Third,
}

impl EncodedStream {
    /// The element name in the 56/57 `Compression` XML.
    pub fn compression_tag(self) -> &'static str {
        match self {
            EncodedStream::Main => "mainStream",
            EncodedStream::Sub => "subStream",
            EncodedStream::Third => "thirdStream",
        }
    }

    /// The `type` in the allowed-values list.
    pub fn table_type(self) -> &'static str {
        match self {
            EncodedStream::Main => "mainStream",
            EncodedStream::Sub => "subStream",
            EncodedStream::Third => "externStream",
        }
    }
}

/// A channel's encoder settings, plus the device's own reply to patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoding {
    pub channel_id: u8,
    pub main: Option<StreamEncoding>,
    pub sub: Option<StreamEncoding>,
    pub third: Option<StreamEncoding>,
    /// The `<Compression>…</Compression>` element exactly as received.
    pub raw: String,
}

impl Encoding {
    pub fn stream(&self, which: EncodedStream) -> Option<&StreamEncoding> {
        match which {
            EncodedStream::Main => self.main.as_ref(),
            EncodedStream::Sub => self.sub.as_ref(),
            EncodedStream::Third => self.third.as_ref(),
        }
    }

    /// Parses a 56 reply body. `None` without a `<Compression>` element.
    pub fn parse(xml: &str) -> Option<Encoding> {
        let start = xml.find("<Compression")?;
        let end = xml.find("</Compression>")? + "</Compression>".len();
        let raw = xml[start..end].to_string();
        let channel_id = number(&raw, "channelId").unwrap_or(0) as u8;
        let stream = |tag: &str| element(&raw, tag).map(parse_stream);
        Some(Encoding {
            channel_id,
            main: stream("mainStream"),
            sub: stream("subStream"),
            third: stream("thirdStream"),
            raw,
        })
    }

    /// The 57 body: the device's own reply with `changes` applied to the
    /// `which` stream (resolution, frame rate, bitrate, codec, GOP).
    pub fn with_changes(&self, which: EncodedStream, changes: &StreamEncoding) -> String {
        let tag = which.compression_tag();
        let Some((start, end)) = element_span(&self.raw, tag) else {
            return wrap(&self.raw);
        };
        let mut block = self.raw[start..end].to_string();
        block = replace_value(&block, "width", &changes.width.to_string());
        block = replace_value(&block, "height", &changes.height.to_string());
        block = replace_value(&block, "resolutionName", &format!("{}*{}", changes.width, changes.height));
        block = replace_value(&block, "frame", &changes.frame_rate.to_string());
        block = replace_value(&block, "bitRate", &changes.bitrate.to_string());
        block = replace_value(&block, "videoEncType", &changes.codec.to_string());
        if let Some(gop) = changes.gop {
            if let Some((gs, ge)) = element_span(&block, "gop") {
                let inner = replace_value(&block[gs..ge], "cur", &gop.current.to_string());
                block = format!("{}{}{}", &block[..gs], inner, &block[ge..]);
            }
        }
        wrap(&format!("{}{}{}", &self.raw[..start], block, &self.raw[end..]))
    }
}

fn wrap(compression: &str) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n{compression}\n</body>\n")
}

fn parse_stream(block: &str) -> StreamEncoding {
    let gop = element(block, "gop").map(|g| GopRange {
        current: number(g, "cur").unwrap_or(0),
        min: number(g, "min").unwrap_or(0),
        max: number(g, "max").unwrap_or(0),
    });
    StreamEncoding {
        width: number(block, "width").unwrap_or(0),
        height: number(block, "height").unwrap_or(0),
        frame_rate: number(block, "frame").unwrap_or(0),
        bitrate: number(block, "bitRate").unwrap_or(0),
        codec: number(block, "videoEncType").unwrap_or(0),
        gop: gop.filter(|g| g.max > 0),
    }
}

/// The allowed values for one stream of one profile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EncodeTable {
    pub stream_type: String,
    pub width: u32,
    pub height: u32,
    pub codec: Option<u32>,
    pub default_frame_rate: u32,
    pub default_bitrate: u32,
    pub default_gop: Option<u32>,
    pub frame_rates: Vec<u32>,
    pub bitrates: Vec<u32>,
}

/// A combination of resolutions with their allowed values, for the channels
/// whose bits are set in `channel_bits`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EncodeProfile {
    pub channel_bits: u64,
    pub tables: Vec<EncodeTable>,
}

impl EncodeProfile {
    pub fn covers(&self, channel: u8) -> bool {
        channel < 64 && self.channel_bits & (1u64 << channel) != 0
    }

    pub fn table(&self, which: EncodedStream) -> Option<&EncodeTable> {
        self.tables.iter().find(|t| t.stream_type == which.table_type())
    }

    /// Parses every `<StreamInfo>` of a `StreamInfoList` (login reply, 146
    /// or 730). Empty when there is none.
    pub fn parse_list(xml: &str) -> Vec<EncodeProfile> {
        let Some(list) = element(xml, "StreamInfoList") else {
            return Vec::new();
        };
        elements(list, "StreamInfo")
            .into_iter()
            .map(|info| EncodeProfile {
                channel_bits: number(info, "channelBits").unwrap_or(0) as u64,
                tables: elements(info, "encodeTable")
                    .into_iter()
                    .map(|t| EncodeTable {
                        stream_type: text(t, "type").unwrap_or_default().to_string(),
                        width: number(t, "width").unwrap_or(0),
                        height: number(t, "height").unwrap_or(0),
                        codec: number(t, "videoEncType"),
                        default_frame_rate: number(t, "defaultFramerate").unwrap_or(0),
                        default_bitrate: number(t, "defaultBitrate").unwrap_or(0),
                        default_gop: number(t, "defaultGop"),
                        frame_rates: list_until_zero(text(t, "framerateTable")),
                        bitrates: list_until_zero(text(t, "bitrateTable")),
                    })
                    .collect(),
            })
            .collect()
    }
}

/// The profiles that apply to `channel`, as the official app picks them.
pub fn profiles_for(profiles: &[EncodeProfile], channel: u8) -> Vec<&EncodeProfile> {
    profiles.iter().filter(|p| p.covers(channel)).collect()
}

/// The profile matching the channel's current main resolution: the one
/// whose values apply to every stream as long as main stays as it is.
pub fn current_profile<'a>(
    profiles: &[&'a EncodeProfile],
    encoding: &Encoding,
) -> Option<&'a EncodeProfile> {
    let main = encoding.main.as_ref()?;
    profiles.iter().copied().find(|p| {
        p.table(EncodedStream::Main).is_some_and(|t| t.width == main.width && t.height == main.height)
    })
}

fn list_until_zero(text: Option<&str>) -> Vec<u32> {
    text.unwrap_or_default()
        .split(',')
        .map_while(|v| v.trim().parse::<u32>().ok().filter(|n| *n > 0))
        .collect()
}

// --- Minimal XML helpers: these replies are flat, well-formed and small. ---

fn element_span(xml: &str, tag: &str) -> Option<(usize, usize)> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut from = 0;
    loop {
        let start = from + xml[from..].find(&open)?;
        // Not a longer tag that merely starts the same way.
        let next = xml[start + open.len()..].chars().next()?;
        if next == '>' || next == ' ' || next == '/' {
            let end = start + xml[start..].find(&close)? + close.len();
            return Some((start, end));
        }
        from = start + open.len();
    }
}

fn element<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    element_span(xml, tag).map(|(s, e)| &xml[s..e])
}

fn elements<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some((s, e)) = element_span(rest, tag) {
        out.push(&rest[s..e]);
        rest = &rest[e..];
    }
    out
}

fn text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let block = element(xml, tag)?;
    let inner_start = block.find('>')? + 1;
    let inner_end = block.rfind("</")?;
    Some(block[inner_start..inner_end].trim())
}

fn number(xml: &str, tag: &str) -> Option<u32> {
    text(xml, tag)?.parse().ok()
}

/// Replaces the text of the first `<tag>…</tag>` directly holding a value.
fn replace_value(xml: &str, tag: &str, value: &str) -> String {
    let Some((s, e)) = element_span(xml, tag) else {
        return xml.to_string();
    };
    let block = &xml[s..e];
    let Some(gt) = block.find('>') else {
        return xml.to_string();
    };
    format!("{}{}{}</{tag}>{}", &xml[..s], &block[..=gt], value, &xml[e..])
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shaped exactly like real 56 and login replies (2026-10-11, an E1
    // Outdoor PoE on an NVR channel), identifying values left out.
    const COMPRESSION: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<Compression version=\"1.1\">\n<channelId>11</channelId>\n<isNoTranslateFrame>1</isNoTranslateFrame>\n<mainStream>\n<audio>1</audio>\n<resolutionName>3840*2160</resolutionName>\n<width>3840</width>\n<height>2160</height>\n<encoderType>vbr</encoderType>\n<frame>25</frame>\n<bitRate>6144</bitRate>\n<encoderProfile>high</encoderProfile>\n<videoEncType>1</videoEncType>\n<gop>\n<cur>2</cur>\n<max>2</max>\n<min>1</min>\n</gop>\n</mainStream>\n<subStream>\n<audio>1</audio>\n<resolutionName>640*360</resolutionName>\n<width>640</width>\n<height>360</height>\n<encoderType>vbr</encoderType>\n<frame>10</frame>\n<bitRate>256</bitRate>\n<encoderProfile>high</encoderProfile>\n<videoEncType>0</videoEncType>\n<gop>\n<cur>4</cur>\n<max>4</max>\n<min>1</min>\n</gop>\n</subStream>\n<thirdStream>\n<audio>1</audio>\n<resolutionName>896*512</resolutionName>\n<width>896</width>\n<height>512</height>\n<encoderType>vbr</encoderType>\n<frame>20</frame>\n<bitRate>1228</bitRate>\n<encoderProfile>high</encoderProfile>\n<videoEncType>0</videoEncType>\n<gop>\n<cur>0</cur>\n<max>0</max>\n<min>0</min>\n</gop>\n</thirdStream>\n</Compression>\n</body>\n";

    const LOGIN_LIST: &str = "<body><DeviceInfo version=\"1.1\"><type>nvr</type></DeviceInfo><StreamInfoList version=\"1.1\"><StreamInfo><channelBits>511</channelBits><encodeTable><type>mainStream</type><resolution><width>4512</width><height>2512</height></resolution><videoEncType>1</videoEncType><defaultFramerate>20</defaultFramerate><defaultBitrate>8192</defaultBitrate><defaultGop>2</defaultGop><framerateTable>20,18,16,15,12,10,8,6,4,2</framerateTable><bitrateTable>3072,4096</bitrateTable></encodeTable></StreamInfo><StreamInfo><channelBits>7680</channelBits><encodeTable><type>mainStream</type><resolution><width>3840</width><height>2160</height></resolution><videoEncType>0</videoEncType><defaultFramerate>25</defaultFramerate><defaultBitrate>6144</defaultBitrate><defaultGop>2</defaultGop><framerateTable>25,22,20,18,16,15,12,10,8,6,4,2</framerateTable><bitrateTable>4096,5120,6144,7168,8192</bitrateTable></encodeTable><encodeTable><type>subStream</type><resolution><width>640</width><height>360</height></resolution><videoEncType>0</videoEncType><defaultFramerate>10</defaultFramerate><defaultBitrate>256</defaultBitrate><defaultGop>4</defaultGop><framerateTable>15,10,7,4</framerateTable><bitrateTable>64,128,160,192,256,384,512</bitrateTable></encodeTable></StreamInfo></StreamInfoList></body>";

    #[test]
    fn parses_the_current_settings() {
        let e = Encoding::parse(COMPRESSION).unwrap();
        assert_eq!(e.channel_id, 11);
        let sub = e.sub.as_ref().unwrap();
        assert_eq!((sub.width, sub.height, sub.frame_rate, sub.bitrate, sub.codec), (640, 360, 10, 256, 0));
        assert_eq!(sub.gop, Some(GopRange { current: 4, min: 1, max: 4 }));
        assert_eq!(e.main.as_ref().unwrap().codec, 1);
        // A stream without a GOP range offers none.
        assert_eq!(e.third.as_ref().unwrap().gop, None);
    }

    #[test]
    fn picks_the_profile_covering_the_channel_and_its_main_resolution() {
        let profiles = EncodeProfile::parse_list(LOGIN_LIST);
        assert_eq!(profiles.len(), 2);
        let for_11 = profiles_for(&profiles, 11);
        assert_eq!(for_11.len(), 1);
        let e = Encoding::parse(COMPRESSION).unwrap();
        let p = current_profile(&for_11, &e).unwrap();
        let sub = p.table(EncodedStream::Sub).unwrap();
        assert_eq!(sub.frame_rates, vec![15, 10, 7, 4]);
        assert_eq!(sub.bitrates.last(), Some(&512));
        assert!(profiles_for(&profiles, 0).iter().all(|p| p.table(EncodedStream::Sub).is_none()));
    }

    #[test]
    fn a_change_patches_only_that_stream_and_keeps_everything_else() {
        let e = Encoding::parse(COMPRESSION).unwrap();
        let mut sub = e.sub.clone().unwrap();
        sub.frame_rate = 15;
        sub.bitrate = 384;
        sub.gop = Some(GopRange { current: 2, ..sub.gop.unwrap() });
        let body = e.with_changes(EncodedStream::Sub, &sub);
        let again = Encoding::parse(&body).unwrap();
        assert_eq!(again.sub.as_ref().unwrap().frame_rate, 15);
        assert_eq!(again.sub.as_ref().unwrap().bitrate, 384);
        assert_eq!(again.sub.as_ref().unwrap().gop.unwrap().current, 2);
        assert_eq!(again.main, e.main);
        assert_eq!(again.third, e.third);
        assert!(body.contains("<isNoTranslateFrame>1</isNoTranslateFrame>"));
        assert!(body.contains("<encoderProfile>high</encoderProfile>"));
        // Unchanged settings write back byte-identical.
        let same = e.with_changes(EncodedStream::Sub, e.sub.as_ref().unwrap());
        assert!(same.contains(&e.raw));
    }
}
