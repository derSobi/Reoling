//! The settings writes Reoling offers, each sent in the shape the official
//! Reolink app was captured sending to a Home Hub Pro (firmware
//! v3.3.0.605, 2026-10-30): an E1 Outdoor PoE on channel 1 for the OSD (45),
//! picture (25 with its 297), light (209) and volume (265) writes, an
//! RLC-1224A on channel 2 for audio noise reduction (440). Every one was
//! answered 200 and, where the app read it again, kept.
//!
//! The app does not echo its read for most of these. It leaves out the
//! parts the device only reports (ranges, abilities), adds a root or an
//! element the read never returns, and orders some children its own way;
//! only the volume and noise-reduction writes are the read reply itself. A
//! read reply patched in place was never seen on the wire, so each write
//! here is the captured body with every value copied, as received, from the
//! device's own reply to the read, except the values the user changed. A
//! write is built only from a reply whose elements are exactly those of the
//! captured read, in the same order: any other reply (another model, another
//! firmware) gives `WriteError::NotCaptured`, and the setting stays
//! read-only. Even a reply of the captured shape is written only on the
//! camera model the write was seen on (`CapturedCamera`).
//!
//! Every write carries an Extension naming the channel alone
//! (`write_extension`), also where the read names the channel type too.
//! Only the values the capture shows the app writing are offered: the OSD
//! name at top left or bottom right, the date shown or hidden, the
//! watermark, flip, day/night `auto` or `blackAndWhite`, brightness, the
//! status LED `open`/`close`, the infrared lights `auto`/`close`, the
//! volume, and noise reduction on/off with its level (a new level only with
//! noise reduction on).
//!
//! The official app was captured changing one field per write, and the
//! camera window's Save does the same: one edited row per write, each built
//! from a read made after the write before was answered. The builders take
//! several changes of one root at once; a body built so was never captured,
//! and the window never asks for one.
//!
//! Each body ends in a newline after `</body>`, which gives it the length
//! the capture lists for the official app's (the text plus one byte); the
//! decode of the capture does not show which byte that is.
//!
//! The header is Reoling's, as for its reads: the channel in `channel_id`,
//! `stream_type` 0 and Reoling's own counter in `msg_num`. The official app
//! sends `msg_num` 0 and counts its requests in the `channel_id` and
//! `stream_type` bytes instead, so its header names no channel; the camera
//! is named in the Extension and the body.

use quick_xml::events::Event;
use quick_xml::Reader;
use std::fmt;
use std::ops::RangeInclusive;

/// `OsdChannelName` + `OsdFlowData` + `OsdDatetime`, built from the 44 read.
pub const MSG_ID_SET_OSD: u32 = 45;
/// `VideoInput` + `InputAdvanceCfg`, built from the 26 read.
pub const MSG_ID_SET_VIDEO_INPUT: u32 = 25;
/// `DayNightThreshold`, built from the 296 read; sent with every 25.
pub const MSG_ID_SET_DAY_NIGHT_THRESHOLD: u32 = 297;
/// `LedState`, built from the 208 read.
pub const MSG_ID_SET_LED_STATE: u32 = 209;
/// `audioCfg`, built from the 264 read.
pub const MSG_ID_SET_AUDIO_CFG: u32 = 265;
/// `aiDenoise`, built from the 439 read.
pub const MSG_ID_SET_AI_DENOISE: u32 = 440;

/// The writes the capture of 2026-10-30 settled (Home Hub Pro, firmware
/// v3.3.0.605). A write of any other id is never sent.
pub const CAPTURED_WRITES: [u32; 6] = [
    MSG_ID_SET_OSD,
    MSG_ID_SET_VIDEO_INPUT,
    MSG_ID_SET_DAY_NIGHT_THRESHOLD,
    MSG_ID_SET_LED_STATE,
    MSG_ID_SET_AUDIO_CFG,
    MSG_ID_SET_AI_DENOISE,
];

/// The cameras the writes were captured on, by the model the channel's
/// version read (318) gives. A reply of the captured shape is not enough: a
/// write also carries values no read returns (the OSD's flow-data overlay,
/// the light's `mode`), so it is offered only on the model it was seen on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapturedCamera {
    /// E1 Outdoor PoE (firmware v3.1.0.5223): the OSD, picture, light and
    /// volume writes.
    E1OutdoorPoe,
    /// RLC-1224A (firmware v3.2.0.6011): the noise-reduction write.
    Rlc1224a,
}

impl CapturedCamera {
    pub fn from_model(model: &str) -> Option<Self> {
        match model.trim() {
            "E1 Outdoor PoE" => Some(CapturedCamera::E1OutdoorPoe),
            "RLC-1224A" => Some(CapturedCamera::Rlc1224a),
            _ => None,
        }
    }

    /// The writes the official app was captured sending to this model.
    pub fn writes(self) -> &'static [u32] {
        match self {
            CapturedCamera::E1OutdoorPoe => &[
                MSG_ID_SET_OSD,
                MSG_ID_SET_VIDEO_INPUT,
                MSG_ID_SET_DAY_NIGHT_THRESHOLD,
                MSG_ID_SET_LED_STATE,
                MSG_ID_SET_AUDIO_CFG,
            ],
            CapturedCamera::Rlc1224a => &[MSG_ID_SET_AI_DENOISE],
        }
    }
}

/// Brightness as `VideoInput/bright`. The read gives no range for it; this
/// is the 0–255 every other picture level of the same reply states (only
/// 140 was captured), and the read after the write shows what the device
/// kept.
pub const BRIGHTNESS_RANGE: RangeInclusive<u32> = 0..=255;
/// The speaker volume (`audioCfg/volume`), as the official app's slider.
pub const VOLUME_RANGE: RangeInclusive<u32> = 0..=100;
/// The noise-reduction levels, the official app's five stops (stop n is
/// `level` n).
pub const DENOISE_LEVELS: RangeInclusive<u32> = 1..=5;

/// The text of a reply a write is built from, exactly as received. It may
/// hold the camera's name; `Debug` shows only its length, so a page printed
/// for diagnostics never carries it.
#[derive(Clone, PartialEq, Eq)]
pub struct RawBody(String);

impl RawBody {
    pub fn new(text: impl Into<String>) -> Self {
        RawBody(text.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RawBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RawBody(<{} bytes>)", self.0.len())
    }
}

/// One message to send: the write id, the channel its Extension names, and
/// the body. `Debug` leaves the body out (it may hold the camera's name).
#[derive(Clone, PartialEq, Eq)]
pub struct SettingsWrite {
    pub msg_id: u32,
    pub channel: u8,
    body: String,
}

impl SettingsWrite {
    pub fn body(&self) -> &str {
        &self.body
    }
}

impl fmt::Debug for SettingsWrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (id, channel, len) = (self.msg_id, self.channel, self.body.len());
        write!(f, "SettingsWrite {{ msg_id: {id}, channel: {channel}, body: <{len} bytes> }}")
    }
}

/// Why a write was not built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    /// The read reply is not shaped like the captured one (this camera's
    /// setting is not written), or the change is one the capture does not
    /// show.
    NotCaptured,
    /// The reply is about another channel than the page's.
    WrongChannel,
    /// A value outside what the setting takes.
    OutOfRange,
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WriteError::NotCaptured => "this camera's reply or the change differs from what Reoling knows how to write",
            WriteError::WrongChannel => "the settings read belongs to another channel",
            WriteError::OutOfRange => "a value is out of range",
        })
    }
}

/// The Extension of every write: the channel alone, as the official app
/// sends it (104 bytes for a one-digit channel).
pub fn write_extension(channel: u8) -> Vec<u8> {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<Extension version=\"1.1\">\n\
         <channelId>{channel}</channelId>\n</Extension>\n"
    )
    .into_bytes()
}

// ---------------------------------------------------------------------------
// The typed changes
// ---------------------------------------------------------------------------

/// Where the camera name sits on the picture: the two positions the capture
/// shows the app writing (top left `1/1`, bottom right `65536/65536`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OsdPosition {
    TopLeft,
    BottomRight,
}

impl OsdPosition {
    pub const ALL: [OsdPosition; 2] = [OsdPosition::TopLeft, OsdPosition::BottomRight];

    /// `topLeftX`, `topLeftY`.
    pub fn coordinates(self) -> (u32, u32) {
        match self {
            OsdPosition::TopLeft => (1, 1),
            OsdPosition::BottomRight => (65536, 65536),
        }
    }

    /// The position these coordinates stand for, when it is one of these.
    /// Either start-edge value (0 or 1) reads as the start edge.
    pub fn from_coordinates(x: Option<u32>, y: Option<u32>) -> Option<Self> {
        match (x?, y?) {
            (0 | 1, 0 | 1) => Some(OsdPosition::TopLeft),
            (65536, 65536) => Some(OsdPosition::BottomRight),
            _ => None,
        }
    }
}

/// What a 45 changes; `None` keeps the device's value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OsdChange {
    pub name_position: Option<OsdPosition>,
    /// Shown, or hidden: hiding is `enable 0` with the position kept.
    pub date_shown: Option<bool>,
    pub watermark: Option<bool>,
}

/// The picture's day/night mode: the two words the capture shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DayNightMode {
    Auto,
    BlackAndWhite,
}

impl DayNightMode {
    pub const ALL: [DayNightMode; 2] = [DayNightMode::Auto, DayNightMode::BlackAndWhite];

    pub fn wire(self) -> &'static str {
        match self {
            DayNightMode::Auto => "auto",
            DayNightMode::BlackAndWhite => "blackAndWhite",
        }
    }

    pub fn from_wire(word: &str) -> Option<Self> {
        DayNightMode::ALL.into_iter().find(|m| m.wire() == word.trim())
    }
}

/// What a 25 changes; `None` keeps the device's value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PictureChange {
    pub brightness: Option<u32>,
    pub flip: Option<bool>,
    pub day_night: Option<DayNightMode>,
}

/// The status LED (`lightState`): the official app's Enable and Close.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatusLed {
    On,
    Off,
}

impl StatusLed {
    pub const ALL: [StatusLed; 2] = [StatusLed::On, StatusLed::Off];

    pub fn wire(self) -> &'static str {
        match self {
            StatusLed::On => "open",
            StatusLed::Off => "close",
        }
    }

    pub fn from_wire(word: &str) -> Option<Self> {
        StatusLed::ALL.into_iter().find(|s| s.wire() == word.trim())
    }
}

/// The infrared lights (`state`): the official app's Auto and Close (the
/// captured camera offered no "on").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InfraredLights {
    Auto,
    Off,
}

impl InfraredLights {
    pub const ALL: [InfraredLights; 2] = [InfraredLights::Auto, InfraredLights::Off];

    pub fn wire(self) -> &'static str {
        match self {
            InfraredLights::Auto => "auto",
            InfraredLights::Off => "close",
        }
    }

    pub fn from_wire(word: &str) -> Option<Self> {
        InfraredLights::ALL.into_iter().find(|s| s.wire() == word.trim())
    }
}

/// What a 209 changes; `None` keeps the device's value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LedChange {
    pub status_led: Option<StatusLed>,
    pub infrared: Option<InfraredLights>,
}

/// What a 440 changes; `None` keeps the device's value. A write with noise
/// reduction off carries the device's own level, as the app sends it (it
/// hides the level then): a new level goes only with noise reduction on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DenoiseChange {
    pub enable: Option<bool>,
    pub level: Option<u32>,
}

// ---------------------------------------------------------------------------
// The builders
// ---------------------------------------------------------------------------

/// The OSD write (45) from the 44 read of `channel`.
pub fn osd_write(read: &RawBody, channel: u8, change: &OsdChange) -> Result<SettingsWrite, WriteError> {
    let mut values = Vec::new();
    if let Some(position) = change.name_position {
        let (x, y) = position.coordinates();
        values.push(("OsdChannelName/topLeftX", x.to_string()));
        values.push(("OsdChannelName/topLeftY", y.to_string()));
    }
    if let Some(shown) = change.date_shown {
        values.push(("OsdDatetime/enable", flag(shown)));
    }
    if let Some(on) = change.watermark {
        values.push(("OsdChannelName/enWatermark", flag(on)));
    }
    build(&OSD, read, channel, &values)
}

/// The picture writes from the 26 and 296 reads of `channel`: the 25, then
/// the 297 the official app sends with every 25, re-sending the threshold
/// unchanged. Both go, in this order, or neither.
pub fn picture_writes(
    picture: &RawBody,
    threshold: &RawBody,
    channel: u8,
    change: &PictureChange,
) -> Result<[SettingsWrite; 2], WriteError> {
    let mut values = Vec::new();
    if let Some(bright) = change.brightness {
        if !BRIGHTNESS_RANGE.contains(&bright) {
            return Err(WriteError::OutOfRange);
        }
        values.push(("VideoInput/bright", bright.to_string()));
    }
    if let Some(flip) = change.flip {
        values.push(("InputAdvanceCfg/flip", flag(flip)));
    }
    if let Some(mode) = change.day_night {
        values.push(("InputAdvanceCfg/DayNight/mode", mode.wire().to_string()));
    }
    let video = build(&PICTURE, picture, channel, &values)?;
    let threshold = build(&THRESHOLD, threshold, channel, &[])?;
    Ok([video, threshold])
}

/// The light write (209) from the 208 read of `channel`: one body carries
/// both the status LED and the infrared lights.
pub fn led_write(read: &RawBody, channel: u8, change: &LedChange) -> Result<SettingsWrite, WriteError> {
    let mut values = Vec::new();
    if let Some(led) = change.status_led {
        values.push(("LedState/lightState", led.wire().to_string()));
    }
    if let Some(ir) = change.infrared {
        values.push(("LedState/state", ir.wire().to_string()));
    }
    build(&LED, read, channel, &values)
}

/// The volume write (265) from the 264 read of `channel`; `None` writes the
/// device's own volume back.
pub fn volume_write(read: &RawBody, channel: u8, volume: Option<u32>) -> Result<SettingsWrite, WriteError> {
    let mut values = Vec::new();
    if let Some(volume) = volume {
        if !VOLUME_RANGE.contains(&volume) {
            return Err(WriteError::OutOfRange);
        }
        values.push(("audioCfg/volume", volume.to_string()));
    }
    build(&AUDIO, read, channel, &values)
}

/// The noise-reduction write (440) from the 439 read of `channel`. A level
/// other than the device's goes only with noise reduction on (the change's,
/// else the device's): `enable 0` with a new level was never captured.
pub fn denoise_write(read: &RawBody, channel: u8, change: &DenoiseChange) -> Result<SettingsWrite, WriteError> {
    let mut values = Vec::new();
    if let Some(on) = change.enable {
        values.push(("aiDenoise/enable", flag(on)));
    }
    if let Some(level) = change.level {
        if !DENOISE_LEVELS.contains(&level) {
            return Err(WriteError::OutOfRange);
        }
        let on = change.enable.unwrap_or_else(|| number_at(read, "aiDenoise/enable") == Some(1));
        if !on && number_at(read, "aiDenoise/level") != Some(level) {
            return Err(WriteError::NotCaptured);
        }
        values.push(("aiDenoise/level", level.to_string()));
    }
    build(&DENOISE, read, channel, &values)
}

fn flag(on: bool) -> String {
    u8::from(on).to_string()
}

/// The element at `path` of a read, when it is a whole number.
fn number_at(read: &RawBody, path: &str) -> Option<u32> {
    elements(read.as_str())?.into_iter().find(|e| e.path == path)?.text?.trim().parse().ok()
}

// ---------------------------------------------------------------------------
// The captured shapes
// ---------------------------------------------------------------------------

/// One write as captured: the read it is built from, as its elements (every
/// value `*`), and the write's roots and elements in the app's order, where
/// `{path}` is the value of that element of the read.
struct Shape {
    msg_id: u32,
    read: &'static str,
    write: &'static str,
}

const OSD: Shape = Shape {
    msg_id: MSG_ID_SET_OSD,
    read: "\
        <OsdDatetime version=\"1.1\">\n\
        <channelId>*</channelId>\n\
        <enable>*</enable>\n\
        <topLeftX>*</topLeftX>\n\
        <topLeftY>*</topLeftY>\n\
        <width>*</width>\n\
        <height>*</height>\n\
        <language>*</language>\n\
        </OsdDatetime>\n\
        <OsdChannelName version=\"1.1\">\n\
        <channelId>*</channelId>\n\
        <name>*</name>\n\
        <enable>*</enable>\n\
        <topLeftX>*</topLeftX>\n\
        <topLeftY>*</topLeftY>\n\
        <enWatermark>*</enWatermark>\n\
        <enBgcolor>*</enBgcolor>\n\
        </OsdChannelName>\n",
    // The flow-data overlay the read does not return goes out switched off,
    // as the app sends it.
    write: "\
        <OsdChannelName version=\"1.1\">\n\
        <channelId>{OsdChannelName/channelId}</channelId>\n\
        <name>{OsdChannelName/name}</name>\n\
        <enable>{OsdChannelName/enable}</enable>\n\
        <topLeftX>{OsdChannelName/topLeftX}</topLeftX>\n\
        <topLeftY>{OsdChannelName/topLeftY}</topLeftY>\n\
        <enBgcolor>{OsdChannelName/enBgcolor}</enBgcolor>\n\
        <enWatermark>{OsdChannelName/enWatermark}</enWatermark>\n\
        </OsdChannelName>\n\
        <OsdFlowData version=\"1.1\">\n\
        <channelId>{OsdChannelName/channelId}</channelId>\n\
        <enable>0</enable>\n\
        <topLeftX>0</topLeftX>\n\
        <topLeftY>0</topLeftY>\n\
        </OsdFlowData>\n\
        <OsdDatetime version=\"1.1\">\n\
        <channelId>{OsdDatetime/channelId}</channelId>\n\
        <enable>{OsdDatetime/enable}</enable>\n\
        <topLeftX>{OsdDatetime/topLeftX}</topLeftX>\n\
        <topLeftY>{OsdDatetime/topLeftY}</topLeftY>\n\
        <language>{OsdDatetime/language}</language>\n\
        </OsdDatetime>\n",
};

const PICTURE: Shape = Shape {
    msg_id: MSG_ID_SET_VIDEO_INPUT,
    read: "\
        <VideoInput version=\"1.1\">\n\
        <channelId>*</channelId>\n\
        <bright>*</bright>\n\
        <contrast>*</contrast>\n\
        <saturation>*</saturation>\n\
        <hue>*</hue>\n\
        <sharpen>*</sharpen>\n\
        <corridorMode>*</corridorMode>\n\
        <corridorAbility>*</corridorAbility>\n\
        </VideoInput>\n\
        <InputAdvanceCfg version=\"1.1\">\n\
        <channelId>*</channelId>\n\
        <digitalChannel>*</digitalChannel>\n\
        <separateCfg>\n\
        <encType>*</encType>\n\
        <constantFrameRate>*</constantFrameRate>\n\
        </separateCfg>\n\
        <PowerLineFrequency>\n\
        <mode>*</mode>\n\
        <enable>*</enable>\n\
        </PowerLineFrequency>\n\
        <Exposure>\n\
        <mode>*</mode>\n\
        <Gainctl>\n\
        <defMin>*</defMin>\n\
        <defMax>*</defMax>\n\
        <curMin>*</curMin>\n\
        <curMax>*</curMax>\n\
        </Gainctl>\n\
        <Shutterctl>\n\
        <defMin>*</defMin>\n\
        <defMax>*</defMax>\n\
        <curMin>*</curMin>\n\
        <curMax>*</curMax>\n\
        </Shutterctl>\n\
        <shutterLevel>*</shutterLevel>\n\
        <gainLevel>*</gainLevel>\n\
        </Exposure>\n\
        <Scene>\n\
        <mode>*</mode>\n\
        <modeList>*</modeList>\n\
        <Redgain>\n\
        <min>*</min>\n\
        <max>*</max>\n\
        <cur>*</cur>\n\
        </Redgain>\n\
        <Bluegain>\n\
        <min>*</min>\n\
        <max>*</max>\n\
        <cur>*</cur>\n\
        </Bluegain>\n\
        </Scene>\n\
        <DayNight>\n\
        <mode>*</mode>\n\
        <IrcutMode>*</IrcutMode>\n\
        <Threshold>*</Threshold>\n\
        </DayNight>\n\
        <BLC>\n\
        <enable>*</enable>\n\
        <mode>*</mode>\n\
        <backlight>\n\
        <min>*</min>\n\
        <max>*</max>\n\
        <cur>*</cur>\n\
        </backlight>\n\
        <dynamicrange>\n\
        <min>*</min>\n\
        <max>*</max>\n\
        <cur>*</cur>\n\
        </dynamicrange>\n\
        </BLC>\n\
        <bdDayColor>\n\
        <mode>*</mode>\n\
        <bright>\n\
        <min>*</min>\n\
        <max>*</max>\n\
        <cur>*</cur>\n\
        </bright>\n\
        <dark>\n\
        <min>*</min>\n\
        <max>*</max>\n\
        <cur>*</cur>\n\
        </dark>\n\
        </bdDayColor>\n\
        <bdNight>\n\
        <mode>*</mode>\n\
        <bright>\n\
        <min>*</min>\n\
        <max>*</max>\n\
        <cur>*</cur>\n\
        </bright>\n\
        <dark>\n\
        <min>*</min>\n\
        <max>*</max>\n\
        <cur>*</cur>\n\
        </dark>\n\
        </bdNight>\n\
        <bdNightColor>\n\
        <mode>*</mode>\n\
        <bright>\n\
        <min>*</min>\n\
        <max>*</max>\n\
        <cur>*</cur>\n\
        </bright>\n\
        <dark>\n\
        <min>*</min>\n\
        <max>*</max>\n\
        <cur>*</cur>\n\
        </dark>\n\
        </bdNightColor>\n\
        <mirror>*</mirror>\n\
        <flip>*</flip>\n\
        <Iris>\n\
        <enable>*</enable>\n\
        <state>*</state>\n\
        <focusAutoiris>*</focusAutoiris>\n\
        </Iris>\n\
        <nr3d>\n\
        <value>*</value>\n\
        <enable>*</enable>\n\
        </nr3d>\n\
        <firstFrameStrategy>*</firstFrameStrategy>\n\
        <constantFrameRate>*</constantFrameRate>\n\
        <constantFrameRateAbility>*</constantFrameRateAbility>\n\
        <binning_mode>*</binning_mode>\n\
        </InputAdvanceCfg>\n",
    // Without the ranges and abilities; the bd* blocks after nr3d, and the
    // backlight levels the other way round, as the app sends them.
    write: "\
        <VideoInput version=\"1.1\">\n\
        <channelId>{VideoInput/channelId}</channelId>\n\
        <bright>{VideoInput/bright}</bright>\n\
        <contrast>{VideoInput/contrast}</contrast>\n\
        <saturation>{VideoInput/saturation}</saturation>\n\
        <hue>{VideoInput/hue}</hue>\n\
        <sharpen>{VideoInput/sharpen}</sharpen>\n\
        <corridorMode>{VideoInput/corridorMode}</corridorMode>\n\
        </VideoInput>\n\
        <InputAdvanceCfg version=\"1.1\">\n\
        <channelId>{InputAdvanceCfg/channelId}</channelId>\n\
        <digitalChannel>{InputAdvanceCfg/digitalChannel}</digitalChannel>\n\
        <PowerLineFrequency>\n\
        <mode>{InputAdvanceCfg/PowerLineFrequency/mode}</mode>\n\
        <enable>{InputAdvanceCfg/PowerLineFrequency/enable}</enable>\n\
        </PowerLineFrequency>\n\
        <Exposure>\n\
        <mode>{InputAdvanceCfg/Exposure/mode}</mode>\n\
        <Gainctl>\n\
        <curMin>{InputAdvanceCfg/Exposure/Gainctl/curMin}</curMin>\n\
        <curMax>{InputAdvanceCfg/Exposure/Gainctl/curMax}</curMax>\n\
        </Gainctl>\n\
        <Shutterctl>\n\
        <curMin>{InputAdvanceCfg/Exposure/Shutterctl/curMin}</curMin>\n\
        <curMax>{InputAdvanceCfg/Exposure/Shutterctl/curMax}</curMax>\n\
        </Shutterctl>\n\
        <shutterLevel>{InputAdvanceCfg/Exposure/shutterLevel}</shutterLevel>\n\
        <gainLevel>{InputAdvanceCfg/Exposure/gainLevel}</gainLevel>\n\
        </Exposure>\n\
        <Scene>\n\
        <mode>{InputAdvanceCfg/Scene/mode}</mode>\n\
        <Redgain>\n\
        <cur>{InputAdvanceCfg/Scene/Redgain/cur}</cur>\n\
        </Redgain>\n\
        <Bluegain>\n\
        <cur>{InputAdvanceCfg/Scene/Bluegain/cur}</cur>\n\
        </Bluegain>\n\
        </Scene>\n\
        <DayNight>\n\
        <mode>{InputAdvanceCfg/DayNight/mode}</mode>\n\
        <IrcutMode>{InputAdvanceCfg/DayNight/IrcutMode}</IrcutMode>\n\
        <Threshold>{InputAdvanceCfg/DayNight/Threshold}</Threshold>\n\
        </DayNight>\n\
        <BLC>\n\
        <enable>{InputAdvanceCfg/BLC/enable}</enable>\n\
        <mode>{InputAdvanceCfg/BLC/mode}</mode>\n\
        <dynamicrange>\n\
        <cur>{InputAdvanceCfg/BLC/dynamicrange/cur}</cur>\n\
        </dynamicrange>\n\
        <backlight>\n\
        <cur>{InputAdvanceCfg/BLC/backlight/cur}</cur>\n\
        </backlight>\n\
        </BLC>\n\
        <mirror>{InputAdvanceCfg/mirror}</mirror>\n\
        <flip>{InputAdvanceCfg/flip}</flip>\n\
        <Iris>\n\
        <enable>{InputAdvanceCfg/Iris/enable}</enable>\n\
        <state>{InputAdvanceCfg/Iris/state}</state>\n\
        <focusAutoiris>{InputAdvanceCfg/Iris/focusAutoiris}</focusAutoiris>\n\
        </Iris>\n\
        <nr3d>\n\
        <value>{InputAdvanceCfg/nr3d/value}</value>\n\
        <enable>{InputAdvanceCfg/nr3d/enable}</enable>\n\
        </nr3d>\n\
        <bdDayColor>\n\
        <mode>{InputAdvanceCfg/bdDayColor/mode}</mode>\n\
        <bright>\n\
        <cur>{InputAdvanceCfg/bdDayColor/bright/cur}</cur>\n\
        </bright>\n\
        <dark>\n\
        <cur>{InputAdvanceCfg/bdDayColor/dark/cur}</cur>\n\
        </dark>\n\
        </bdDayColor>\n\
        <bdNight>\n\
        <mode>{InputAdvanceCfg/bdNight/mode}</mode>\n\
        <bright>\n\
        <cur>{InputAdvanceCfg/bdNight/bright/cur}</cur>\n\
        </bright>\n\
        <dark>\n\
        <cur>{InputAdvanceCfg/bdNight/dark/cur}</cur>\n\
        </dark>\n\
        </bdNight>\n\
        <bdNightColor>\n\
        <mode>{InputAdvanceCfg/bdNightColor/mode}</mode>\n\
        <bright>\n\
        <cur>{InputAdvanceCfg/bdNightColor/bright/cur}</cur>\n\
        </bright>\n\
        <dark>\n\
        <cur>{InputAdvanceCfg/bdNightColor/dark/cur}</cur>\n\
        </dark>\n\
        </bdNightColor>\n\
        <firstFrameStrategy>{InputAdvanceCfg/firstFrameStrategy}</firstFrameStrategy>\n\
        <constantFrameRate>{InputAdvanceCfg/constantFrameRate}</constantFrameRate>\n\
        <binning_mode>{InputAdvanceCfg/binning_mode}</binning_mode>\n\
        </InputAdvanceCfg>\n",
};

const THRESHOLD: Shape = Shape {
    msg_id: MSG_ID_SET_DAY_NIGHT_THRESHOLD,
    read: "\
        <DayNightThreshold version=\"1.1\">\n\
        <channelId>*</channelId>\n\
        <threshold>*</threshold>\n\
        <stat>*</stat>\n\
        <thresholdval>\n\
        <min>*</min>\n\
        <max>*</max>\n\
        <cur>*</cur>\n\
        </thresholdval>\n\
        </DayNightThreshold>\n",
    // The write spells the value's element `thresholdVal`.
    write: "\
        <DayNightThreshold version=\"1.1\">\n\
        <channelId>{DayNightThreshold/channelId}</channelId>\n\
        <threshold>{DayNightThreshold/threshold}</threshold>\n\
        <thresholdVal>\n\
        <cur>{DayNightThreshold/thresholdval/cur}</cur>\n\
        </thresholdVal>\n\
        </DayNightThreshold>\n",
};

const LED: Shape = Shape {
    msg_id: MSG_ID_SET_LED_STATE,
    read: "\
        <LedState version=\"1.1\">\n\
        <channelId>*</channelId>\n\
        <ledVersion>*</ledVersion>\n\
        <IRLedBrightness>*</IRLedBrightness>\n\
        <state>*</state>\n\
        <lightState>*</lightState>\n\
        <doorbellLightState>*</doorbellLightState>\n\
        </LedState>\n",
    // Without `ledVersion`, with `mode 0`, as the app sends it.
    write: "\
        <LedState version=\"1.1\">\n\
        <channelId>{LedState/channelId}</channelId>\n\
        <state>{LedState/state}</state>\n\
        <lightState>{LedState/lightState}</lightState>\n\
        <doorbellLightState>{LedState/doorbellLightState}</doorbellLightState>\n\
        <IRLedBrightness>{LedState/IRLedBrightness}</IRLedBrightness>\n\
        <mode>0</mode>\n\
        </LedState>\n",
};

const AUDIO: Shape = Shape {
    msg_id: MSG_ID_SET_AUDIO_CFG,
    read: "\
        <audioCfg version=\"1.1\">\n\
        <channelId>*</channelId>\n\
        <timeout>*</timeout>\n\
        <audioSelect>*</audioSelect>\n\
        <volume>*</volume>\n\
        <preAlarm>*</preAlarm>\n\
        <audioListId>*</audioListId>\n\
        <visitorLoudspeaker>*</visitorLoudspeaker>\n\
        </audioCfg>\n",
    write: "\
        <audioCfg version=\"1.1\">\n\
        <channelId>{audioCfg/channelId}</channelId>\n\
        <timeout>{audioCfg/timeout}</timeout>\n\
        <audioSelect>{audioCfg/audioSelect}</audioSelect>\n\
        <volume>{audioCfg/volume}</volume>\n\
        <preAlarm>{audioCfg/preAlarm}</preAlarm>\n\
        <audioListId>{audioCfg/audioListId}</audioListId>\n\
        <visitorLoudspeaker>{audioCfg/visitorLoudspeaker}</visitorLoudspeaker>\n\
        </audioCfg>\n",
};

const DENOISE: Shape = Shape {
    msg_id: MSG_ID_SET_AI_DENOISE,
    // The read's root attribute is `Version`, the write's `version`.
    read: "\
        <aiDenoise Version=\"1.1\">\n\
        <channelId>*</channelId>\n\
        <enable>*</enable>\n\
        <level>*</level>\n\
        </aiDenoise>\n",
    write: "\
        <aiDenoise version=\"1.1\">\n\
        <channelId>{aiDenoise/channelId}</channelId>\n\
        <enable>{aiDenoise/enable}</enable>\n\
        <level>{aiDenoise/level}</level>\n\
        </aiDenoise>\n",
};

/// The write of `shape` from the device's reply `read` about `channel`, with
/// `values` (element path of the read, new text) in place of the device's.
fn build(shape: &Shape, read: &RawBody, channel: u8, values: &[(&str, String)]) -> Result<SettingsWrite, WriteError> {
    let reply = elements(read.as_str()).ok_or(WriteError::NotCaptured)?;
    let captured = elements(shape.read).ok_or(WriteError::NotCaptured)?;
    if !same_structure(&reply, &captured) {
        return Err(WriteError::NotCaptured);
    }
    // Every channel the reply names is the page's.
    for element in reply.iter().filter(|e| e.name() == "channelId") {
        if element.text.as_deref().and_then(|t| t.trim().parse::<u8>().ok()) != Some(channel) {
            return Err(WriteError::WrongChannel);
        }
    }
    // A value goes only where the write has a place for it.
    if values.iter().any(|(path, _)| !shape.write.contains(&format!("{{{path}}}"))) {
        return Err(WriteError::NotCaptured);
    }
    let filled = fill(shape.write, &reply, values).ok_or(WriteError::NotCaptured)?;
    Ok(SettingsWrite {
        msg_id: shape.msg_id,
        channel,
        body: format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n{filled}</body>\n"),
    })
}

/// `template` with each `{path}` replaced: by the new value given for that
/// path, else by the text of that element of the read.
fn fill(template: &str, read: &[Element], values: &[(&str, String)]) -> Option<String> {
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let close = open + rest[open..].find('}')?;
        let path = &rest[open + 1..close];
        let value = match values.iter().find(|(p, _)| *p == path) {
            Some((_, value)) => value.as_str(),
            None => read.iter().find(|e| e.path == path)?.text.as_deref()?,
        };
        out.push_str(value);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Some(out)
}

// ---------------------------------------------------------------------------
// A body as a list of its elements
// ---------------------------------------------------------------------------

/// One element of a body: its path below `<body>` (`VideoInput/bright`) and,
/// for an element without child elements, its text exactly as written
/// (entities left as they are, so it goes back on the wire unchanged).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Element {
    path: String,
    text: Option<String>,
}

impl Element {
    fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

struct Open {
    name: String,
    /// Its place in the list; `None` for the `<body>` wrapper.
    index: Option<usize>,
    children: bool,
    text: String,
}

/// Every element of `xml`, in document order, containers first; `None` for
/// anything that is not plain well-formed elements and text (CDATA, text
/// mixed with child elements, unbalanced tags, invalid UTF-8).
fn elements(xml: &str) -> Option<Vec<Element>> {
    let mut reader = Reader::from_str(xml);
    let mut out: Vec<Element> = Vec::new();
    let mut stack: Vec<Open> = Vec::new();
    loop {
        let event = reader.read_event().ok()?;
        match event {
            Event::Start(ref start) | Event::Empty(ref start) => {
                let empty = matches!(event, Event::Empty(_));
                let name = std::str::from_utf8(start.name().as_ref()).ok()?.to_string();
                if let Some(parent) = stack.last_mut() {
                    if !parent.text.trim().is_empty() {
                        return None;
                    }
                    parent.children = true;
                }
                if stack.is_empty() && name == "body" && !empty {
                    stack.push(Open { name, index: None, children: false, text: String::new() });
                    continue;
                }
                let path = stack
                    .iter()
                    .filter(|o| o.index.is_some())
                    .map(|o| o.name.as_str())
                    .chain(std::iter::once(name.as_str()))
                    .collect::<Vec<_>>()
                    .join("/");
                out.push(Element { path, text: empty.then(String::new) });
                if !empty {
                    stack.push(Open { name, index: Some(out.len() - 1), children: false, text: String::new() });
                }
            }
            Event::Text(text) => {
                let raw = std::str::from_utf8(&text).ok()?;
                match stack.last_mut() {
                    Some(open) => open.text.push_str(raw),
                    None if raw.trim().is_empty() => {}
                    None => return None,
                }
            }
            Event::End(_) => {
                let open = stack.pop()?;
                match open.index {
                    Some(index) if !open.children => out[index].text = Some(open.text),
                    _ if !open.text.trim().is_empty() => return None,
                    _ => {}
                }
            }
            Event::CData(_) => return None,
            Event::Eof => break,
            // The declaration, comments, processing instructions.
            _ => {}
        }
    }
    stack.is_empty().then_some(out)
}

/// The same elements in the same order, each with or without children as
/// the other; the texts may differ.
fn same_structure(a: &[Element], b: &[Element]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.path == y.path && x.text.is_some() == y.text.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    // As captured from the official app on 2026-10-30 (Home Hub Pro,
    // firmware v3.3.0.605): the replies to the reads of an E1 Outdoor PoE on
    // channel 1 (an RLC-1224A on channel 2 for aiDenoise) and the writes the
    // app sent, each followed by the one byte the decoder leaves out of the
    // text (the length on the wire is the text plus one).

    const OSD_READ: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<OsdDatetime version=\"1.1\">\n<channelId>1</channelId>\n<enable>1</enable>\n<topLeftX>65537</topLeftX>\n<topLeftY>1</topLeftY>\n<width>0</width>\n<height>0</height>\n<language>Chinese</language>\n</OsdDatetime>\n<OsdChannelName version=\"1.1\">\n<channelId>1</channelId>\n<name>Camera1</name>\n<enable>1</enable>\n<topLeftX>65536</topLeftX>\n<topLeftY>65536</topLeftY>\n<enWatermark>0</enWatermark>\n<enBgcolor>0</enBgcolor>\n</OsdChannelName>\n</body>\n";

    const OSD_NAME_TOP_LEFT: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<OsdChannelName version=\"1.1\">\n<channelId>1</channelId>\n<name>Camera1</name>\n<enable>1</enable>\n<topLeftX>1</topLeftX>\n<topLeftY>1</topLeftY>\n<enBgcolor>0</enBgcolor>\n<enWatermark>0</enWatermark>\n</OsdChannelName>\n<OsdFlowData version=\"1.1\">\n<channelId>1</channelId>\n<enable>0</enable>\n<topLeftX>0</topLeftX>\n<topLeftY>0</topLeftY>\n</OsdFlowData>\n<OsdDatetime version=\"1.1\">\n<channelId>1</channelId>\n<enable>1</enable>\n<topLeftX>65537</topLeftX>\n<topLeftY>1</topLeftY>\n<language>Chinese</language>\n</OsdDatetime>\n</body>\n";

    const OSD_BASE: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<OsdChannelName version=\"1.1\">\n<channelId>1</channelId>\n<name>Camera1</name>\n<enable>1</enable>\n<topLeftX>65536</topLeftX>\n<topLeftY>65536</topLeftY>\n<enBgcolor>0</enBgcolor>\n<enWatermark>0</enWatermark>\n</OsdChannelName>\n<OsdFlowData version=\"1.1\">\n<channelId>1</channelId>\n<enable>0</enable>\n<topLeftX>0</topLeftX>\n<topLeftY>0</topLeftY>\n</OsdFlowData>\n<OsdDatetime version=\"1.1\">\n<channelId>1</channelId>\n<enable>1</enable>\n<topLeftX>65537</topLeftX>\n<topLeftY>1</topLeftY>\n<language>Chinese</language>\n</OsdDatetime>\n</body>\n";

    const OSD_DATE_HIDDEN: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<OsdChannelName version=\"1.1\">\n<channelId>1</channelId>\n<name>Camera1</name>\n<enable>1</enable>\n<topLeftX>65536</topLeftX>\n<topLeftY>65536</topLeftY>\n<enBgcolor>0</enBgcolor>\n<enWatermark>0</enWatermark>\n</OsdChannelName>\n<OsdFlowData version=\"1.1\">\n<channelId>1</channelId>\n<enable>0</enable>\n<topLeftX>0</topLeftX>\n<topLeftY>0</topLeftY>\n</OsdFlowData>\n<OsdDatetime version=\"1.1\">\n<channelId>1</channelId>\n<enable>0</enable>\n<topLeftX>65537</topLeftX>\n<topLeftY>1</topLeftY>\n<language>Chinese</language>\n</OsdDatetime>\n</body>\n";

    const OSD_WATERMARK_ON: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<OsdChannelName version=\"1.1\">\n<channelId>1</channelId>\n<name>Camera1</name>\n<enable>1</enable>\n<topLeftX>65536</topLeftX>\n<topLeftY>65536</topLeftY>\n<enBgcolor>0</enBgcolor>\n<enWatermark>1</enWatermark>\n</OsdChannelName>\n<OsdFlowData version=\"1.1\">\n<channelId>1</channelId>\n<enable>0</enable>\n<topLeftX>0</topLeftX>\n<topLeftY>0</topLeftY>\n</OsdFlowData>\n<OsdDatetime version=\"1.1\">\n<channelId>1</channelId>\n<enable>1</enable>\n<topLeftX>65537</topLeftX>\n<topLeftY>1</topLeftY>\n<language>Chinese</language>\n</OsdDatetime>\n</body>\n";

    const PICTURE_READ: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<VideoInput version=\"1.1\">\n<channelId>1</channelId>\n<bright>128</bright>\n<contrast>128</contrast>\n<saturation>128</saturation>\n<hue>128</hue>\n<sharpen>128</sharpen>\n<corridorMode>close</corridorMode>\n<corridorAbility>0</corridorAbility>\n</VideoInput>\n<InputAdvanceCfg version=\"1.1\">\n<channelId>1</channelId>\n<digitalChannel>1</digitalChannel>\n<separateCfg>\n<encType>0</encType>\n<constantFrameRate>0</constantFrameRate>\n</separateCfg>\n<PowerLineFrequency>\n<mode>50hz</mode>\n<enable>0</enable>\n</PowerLineFrequency>\n<Exposure>\n<mode>auto</mode>\n<Gainctl>\n<defMin>1</defMin>\n<defMax>100</defMax>\n<curMin>1</curMin>\n<curMax>62</curMax>\n</Gainctl>\n<Shutterctl>\n<defMin>0</defMin>\n<defMax>125</defMax>\n<curMin>0</curMin>\n<curMax>125</curMax>\n</Shutterctl>\n<shutterLevel>1/30</shutterLevel>\n<gainLevel>50</gainLevel>\n</Exposure>\n<Scene>\n<mode>auto</mode>\n<modeList>auto, manual</modeList>\n<Redgain>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</Redgain>\n<Bluegain>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</Bluegain>\n</Scene>\n<DayNight>\n<mode>auto</mode>\n<IrcutMode>ir</IrcutMode>\n<Threshold>medium</Threshold>\n</DayNight>\n<BLC>\n<enable>0</enable>\n<mode>backLight</mode>\n<backlight>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</backlight>\n<dynamicrange>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</dynamicrange>\n</BLC>\n<bdDayColor>\n<mode>auto</mode>\n<bright>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</bright>\n<dark>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</dark>\n</bdDayColor>\n<bdNight>\n<mode>auto</mode>\n<bright>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</bright>\n<dark>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</dark>\n</bdNight>\n<bdNightColor>\n<mode>auto</mode>\n<bright>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</bright>\n<dark>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</dark>\n</bdNightColor>\n<mirror>0</mirror>\n<flip>0</flip>\n<Iris>\n<enable>0</enable>\n<state>success</state>\n<focusAutoiris>0</focusAutoiris>\n</Iris>\n<nr3d>\n<value>high</value>\n<enable>1</enable>\n</nr3d>\n<firstFrameStrategy>0</firstFrameStrategy>\n<constantFrameRate>2</constantFrameRate>\n<constantFrameRateAbility>7</constantFrameRateAbility>\n<binning_mode>0</binning_mode>\n</InputAdvanceCfg>\n</body>\n";

    // The revert of the flip, the day/night and the brightness rows, byte
    // for byte the same each time. Each change was this body with the one
    // element changed (checked against the capture: 1653, 1662 and 1653
    // bytes on the wire).
    const PICTURE_BASE: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<VideoInput version=\"1.1\">\n<channelId>1</channelId>\n<bright>128</bright>\n<contrast>128</contrast>\n<saturation>128</saturation>\n<hue>128</hue>\n<sharpen>128</sharpen>\n<corridorMode>close</corridorMode>\n</VideoInput>\n<InputAdvanceCfg version=\"1.1\">\n<channelId>1</channelId>\n<digitalChannel>1</digitalChannel>\n<PowerLineFrequency>\n<mode>50hz</mode>\n<enable>0</enable>\n</PowerLineFrequency>\n<Exposure>\n<mode>auto</mode>\n<Gainctl>\n<curMin>1</curMin>\n<curMax>62</curMax>\n</Gainctl>\n<Shutterctl>\n<curMin>0</curMin>\n<curMax>125</curMax>\n</Shutterctl>\n<shutterLevel>1/30</shutterLevel>\n<gainLevel>50</gainLevel>\n</Exposure>\n<Scene>\n<mode>auto</mode>\n<Redgain>\n<cur>128</cur>\n</Redgain>\n<Bluegain>\n<cur>128</cur>\n</Bluegain>\n</Scene>\n<DayNight>\n<mode>auto</mode>\n<IrcutMode>ir</IrcutMode>\n<Threshold>medium</Threshold>\n</DayNight>\n<BLC>\n<enable>0</enable>\n<mode>backLight</mode>\n<dynamicrange>\n<cur>128</cur>\n</dynamicrange>\n<backlight>\n<cur>128</cur>\n</backlight>\n</BLC>\n<mirror>0</mirror>\n<flip>0</flip>\n<Iris>\n<enable>0</enable>\n<state>success</state>\n<focusAutoiris>0</focusAutoiris>\n</Iris>\n<nr3d>\n<value>high</value>\n<enable>1</enable>\n</nr3d>\n<bdDayColor>\n<mode>auto</mode>\n<bright>\n<cur>128</cur>\n</bright>\n<dark>\n<cur>128</cur>\n</dark>\n</bdDayColor>\n<bdNight>\n<mode>auto</mode>\n<bright>\n<cur>128</cur>\n</bright>\n<dark>\n<cur>128</cur>\n</dark>\n</bdNight>\n<bdNightColor>\n<mode>auto</mode>\n<bright>\n<cur>128</cur>\n</bright>\n<dark>\n<cur>128</cur>\n</dark>\n</bdNightColor>\n<firstFrameStrategy>0</firstFrameStrategy>\n<constantFrameRate>2</constantFrameRate>\n<binning_mode>0</binning_mode>\n</InputAdvanceCfg>\n</body>\n";

    const THRESHOLD_READ: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<DayNightThreshold version=\"1.1\">\n<channelId>1</channelId>\n<threshold>custom</threshold>\n<stat>night</stat>\n<thresholdval>\n<min>0</min>\n<max>100</max>\n<cur>50</cur>\n</thresholdval>\n</DayNightThreshold>\n</body>\n";

    // The same all six times.
    const THRESHOLD_WRITE: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<DayNightThreshold version=\"1.1\">\n<channelId>1</channelId>\n<threshold>custom</threshold>\n<thresholdVal>\n<cur>50</cur>\n</thresholdVal>\n</DayNightThreshold>\n</body>\n";

    const LED_READ: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<LedState version=\"1.1\">\n<channelId>1</channelId>\n<ledVersion>0</ledVersion>\n<IRLedBrightness>0</IRLedBrightness>\n<state>auto</state>\n<lightState>open</lightState>\n<doorbellLightState>close</doorbellLightState>\n</LedState>\n</body>\n";

    const LED_BASE: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<LedState version=\"1.1\">\n<channelId>1</channelId>\n<state>auto</state>\n<lightState>open</lightState>\n<doorbellLightState>close</doorbellLightState>\n<IRLedBrightness>0</IRLedBrightness>\n<mode>0</mode>\n</LedState>\n</body>\n";

    const LED_STATUS_OFF: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<LedState version=\"1.1\">\n<channelId>1</channelId>\n<state>auto</state>\n<lightState>close</lightState>\n<doorbellLightState>close</doorbellLightState>\n<IRLedBrightness>0</IRLedBrightness>\n<mode>0</mode>\n</LedState>\n</body>\n";

    const LED_IR_OFF: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<LedState version=\"1.1\">\n<channelId>1</channelId>\n<state>close</state>\n<lightState>open</lightState>\n<doorbellLightState>close</doorbellLightState>\n<IRLedBrightness>0</IRLedBrightness>\n<mode>0</mode>\n</LedState>\n</body>\n";

    const AUDIO_READ: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<audioCfg version=\"1.1\">\n<channelId>1</channelId>\n<timeout>0</timeout>\n<audioSelect>0</audioSelect>\n<volume>92</volume>\n<preAlarm>0</preAlarm>\n<audioListId>0</audioListId>\n<visitorLoudspeaker>0</visitorLoudspeaker>\n</audioCfg>\n</body>\n";

    const AUDIO_VOLUME_50: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<audioCfg version=\"1.1\">\n<channelId>1</channelId>\n<timeout>0</timeout>\n<audioSelect>0</audioSelect>\n<volume>50</volume>\n<preAlarm>0</preAlarm>\n<audioListId>0</audioListId>\n<visitorLoudspeaker>0</visitorLoudspeaker>\n</audioCfg>\n</body>\n";

    const DENOISE_READ: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<aiDenoise Version=\"1.1\">\n<channelId>2</channelId>\n<enable>1</enable>\n<level>4</level>\n</aiDenoise>\n</body>\n";

    const DENOISE_BASE: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<aiDenoise version=\"1.1\">\n<channelId>2</channelId>\n<enable>1</enable>\n<level>4</level>\n</aiDenoise>\n</body>\n";

    const DENOISE_OFF: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<aiDenoise version=\"1.1\">\n<channelId>2</channelId>\n<enable>0</enable>\n<level>4</level>\n</aiDenoise>\n</body>\n";

    const DENOISE_LEVEL_3: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<aiDenoise version=\"1.1\">\n<channelId>2</channelId>\n<enable>1</enable>\n<level>3</level>\n</aiDenoise>\n</body>\n";

    fn raw(text: &str) -> RawBody {
        RawBody::new(text)
    }

    /// The length on the wire, as the capture lists it.
    fn wire_len(write: &SettingsWrite) -> usize {
        write.body().len()
    }

    #[test]
    fn every_captured_read_has_the_shape_of_its_write() {
        for (shape, read) in [
            (&OSD, OSD_READ),
            (&PICTURE, PICTURE_READ),
            (&THRESHOLD, THRESHOLD_READ),
            (&LED, LED_READ),
            (&AUDIO, AUDIO_READ),
            (&DENOISE, DENOISE_READ),
        ] {
            let captured = elements(read).expect("the captured read parses");
            let skeleton = elements(shape.read).expect("the shape parses");
            assert!(same_structure(&captured, &skeleton), "msg {}", shape.msg_id);
            // Every value of the write comes from an element of the read.
            let mut rest = shape.write;
            while let Some(open) = rest.find('{') {
                let close = open + rest[open..].find('}').unwrap();
                let path = &rest[open + 1..close];
                assert!(captured.iter().any(|e| e.path == path && e.text.is_some()), "msg {}: {path}", shape.msg_id);
                rest = &rest[close + 1..];
            }
            assert!(CAPTURED_WRITES.contains(&shape.msg_id));
            // Each path names one element, so a value is never taken from a
            // namesake elsewhere in the reply.
            let mut paths: Vec<&str> = captured.iter().map(|e| e.path.as_str()).collect();
            paths.sort_unstable();
            assert!(paths.windows(2).all(|w| w[0] != w[1]), "msg {}", shape.msg_id);
        }
    }

    #[test]
    fn osd_writes_are_the_captured_bodies() {
        let read = raw(OSD_READ);
        let unchanged = osd_write(&read, 1, &OsdChange::default()).unwrap();
        assert_eq!(unchanged.body(), OSD_BASE);
        assert_eq!((unchanged.msg_id, unchanged.channel, wire_len(&unchanged)), (45, 1, 576));
        let top_left = OsdChange { name_position: Some(OsdPosition::TopLeft), ..Default::default() };
        let write = osd_write(&read, 1, &top_left).unwrap();
        assert_eq!(write.body(), OSD_NAME_TOP_LEFT);
        assert_eq!(wire_len(&write), 568);
        let hidden = OsdChange { date_shown: Some(false), ..Default::default() };
        assert_eq!(osd_write(&read, 1, &hidden).unwrap().body(), OSD_DATE_HIDDEN);
        let watermark = OsdChange { watermark: Some(true), ..Default::default() };
        assert_eq!(osd_write(&read, 1, &watermark).unwrap().body(), OSD_WATERMARK_ON);
        // The revert from the changed state: the device's read then says
        // top left, the write goes back to bottom right.
        let changed = OSD_READ.replace(
            "<topLeftX>65536</topLeftX>\n<topLeftY>65536</topLeftY>",
            "<topLeftX>1</topLeftX>\n<topLeftY>1</topLeftY>",
        );
        let back = OsdChange { name_position: Some(OsdPosition::BottomRight), ..Default::default() };
        assert_eq!(osd_write(&raw(&changed), 1, &back).unwrap().body(), OSD_BASE);
        // Setting a value to what the device has changes nothing.
        let same =
            OsdChange { name_position: Some(OsdPosition::BottomRight), date_shown: Some(true), watermark: Some(false) };
        assert_eq!(osd_write(&read, 1, &same).unwrap().body(), OSD_BASE);
    }

    #[test]
    fn picture_writes_are_the_captured_bodies_with_the_threshold_echo() {
        let (picture, threshold) = (raw(PICTURE_READ), raw(THRESHOLD_READ));
        let [video, echo] = picture_writes(&picture, &threshold, 1, &PictureChange::default()).unwrap();
        assert_eq!(video.body(), PICTURE_BASE);
        assert_eq!((video.msg_id, video.channel, wire_len(&video)), (25, 1, 1653));
        assert_eq!(echo.body(), THRESHOLD_WRITE);
        assert_eq!((echo.msg_id, echo.channel, wire_len(&echo)), (297, 1, 210));
        let flip = PictureChange { flip: Some(true), ..Default::default() };
        let [video, echo] = picture_writes(&picture, &threshold, 1, &flip).unwrap();
        assert_eq!(video.body(), PICTURE_BASE.replacen("<flip>0</flip>", "<flip>1</flip>", 1));
        assert_eq!(echo.body(), THRESHOLD_WRITE);
        let bw = PictureChange { day_night: Some(DayNightMode::BlackAndWhite), ..Default::default() };
        let [video, _] = picture_writes(&picture, &threshold, 1, &bw).unwrap();
        assert_eq!(
            video.body(),
            PICTURE_BASE.replacen("<DayNight>\n<mode>auto</mode>", "<DayNight>\n<mode>blackAndWhite</mode>", 1)
        );
        assert_eq!(wire_len(&video), 1662);
        let bright = PictureChange { brightness: Some(140), ..Default::default() };
        let [video, _] = picture_writes(&picture, &threshold, 1, &bright).unwrap();
        assert_eq!(video.body(), PICTURE_BASE.replacen("<bright>128</bright>", "<bright>140</bright>", 1));
        // Only `VideoInput/bright`: the bd* blocks' own `bright` stay.
        assert_eq!(video.body().matches("<bright>\n<cur>128</cur>").count(), 3);
        assert_eq!(wire_len(&video), 1653);
    }

    #[test]
    fn led_volume_and_denoise_writes_are_the_captured_bodies() {
        let led = raw(LED_READ);
        assert_eq!(led_write(&led, 1, &LedChange::default()).unwrap().body(), LED_BASE);
        let off = LedChange { status_led: Some(StatusLed::Off), ..Default::default() };
        let write = led_write(&led, 1, &off).unwrap();
        assert_eq!((write.body(), write.msg_id, wire_len(&write)), (LED_STATUS_OFF, 209, 267));
        let ir = LedChange { infrared: Some(InfraredLights::Off), ..Default::default() };
        assert_eq!(led_write(&led, 1, &ir).unwrap().body(), LED_IR_OFF);

        let audio = raw(AUDIO_READ);
        let write = volume_write(&audio, 1, Some(50)).unwrap();
        assert_eq!((write.body(), write.msg_id, wire_len(&write)), (AUDIO_VOLUME_50, 265, 282));
        // The volume write is the read itself.
        assert_eq!(volume_write(&audio, 1, None).unwrap().body(), AUDIO_READ);
        assert_eq!(volume_write(&audio, 1, Some(92)).unwrap().body(), AUDIO_READ);

        let denoise = raw(DENOISE_READ);
        assert_eq!(denoise_write(&denoise, 2, &DenoiseChange::default()).unwrap().body(), DENOISE_BASE);
        let off = DenoiseChange { enable: Some(false), ..Default::default() };
        let write = denoise_write(&denoise, 2, &off).unwrap();
        assert_eq!((write.body(), write.msg_id, write.channel, wire_len(&write)), (DENOISE_OFF, 440, 2, 155));
        let level = DenoiseChange { level: Some(3), ..Default::default() };
        assert_eq!(denoise_write(&denoise, 2, &level).unwrap().body(), DENOISE_LEVEL_3);
    }

    #[test]
    fn a_new_noise_reduction_level_goes_only_with_noise_reduction_on() {
        let denoise = raw(DENOISE_READ);
        let off = DENOISE_READ.replace("<enable>1</enable>", "<enable>0</enable>");
        let level = DenoiseChange { level: Some(3), ..Default::default() };
        // Off (the change's, or the device's) with a new level: never captured.
        let off_and_level = DenoiseChange { enable: Some(false), level: Some(3) };
        assert_eq!(denoise_write(&denoise, 2, &off_and_level).unwrap_err(), WriteError::NotCaptured);
        assert_eq!(denoise_write(&raw(&off), 2, &level).unwrap_err(), WriteError::NotCaptured);
        // Off with the device's own level is the captured body.
        let own_level = DenoiseChange { enable: Some(false), level: Some(4) };
        assert_eq!(denoise_write(&denoise, 2, &own_level).unwrap().body(), DENOISE_OFF);
        // Turned on with a new level.
        let on_and_level = DenoiseChange { enable: Some(true), level: Some(3) };
        assert_eq!(denoise_write(&raw(&off), 2, &on_and_level).unwrap().body(), DENOISE_LEVEL_3);
    }

    #[test]
    fn values_outside_their_range_are_refused() {
        let (picture, threshold) = (raw(PICTURE_READ), raw(THRESHOLD_READ));
        for bright in [0, 255] {
            let change = PictureChange { brightness: Some(bright), ..Default::default() };
            assert!(picture_writes(&picture, &threshold, 1, &change).is_ok());
        }
        let change = PictureChange { brightness: Some(256), ..Default::default() };
        assert_eq!(picture_writes(&picture, &threshold, 1, &change).unwrap_err(), WriteError::OutOfRange);
        let audio = raw(AUDIO_READ);
        assert!(volume_write(&audio, 1, Some(0)).is_ok() && volume_write(&audio, 1, Some(100)).is_ok());
        assert_eq!(volume_write(&audio, 1, Some(101)).unwrap_err(), WriteError::OutOfRange);
        let denoise = raw(DENOISE_READ);
        for (level, ok) in [(0, false), (1, true), (5, true), (6, false)] {
            let change = DenoiseChange { level: Some(level), ..Default::default() };
            assert_eq!(denoise_write(&denoise, 2, &change).is_ok(), ok, "level {level}");
        }
    }

    #[test]
    fn only_a_reply_shaped_like_the_captured_one_is_written() {
        // Another channel than the page's.
        assert_eq!(osd_write(&raw(OSD_READ), 0, &OsdChange::default()).unwrap_err(), WriteError::WrongChannel);
        assert_eq!(
            denoise_write(&raw(DENOISE_READ), 1, &DenoiseChange::default()).unwrap_err(),
            WriteError::WrongChannel
        );
        let threshold_ch2 = THRESHOLD_READ.replace("<channelId>1</channelId>", "<channelId>2</channelId>");
        let change = PictureChange { flip: Some(true), ..Default::default() };
        assert_eq!(
            picture_writes(&raw(PICTURE_READ), &raw(&threshold_ch2), 1, &change).unwrap_err(),
            WriteError::WrongChannel
        );
        // An element more (another model's reply), one fewer, another order.
        let more = PICTURE_READ
            .replace("<binning_mode>0</binning_mode>", "<encType>1</encType>\n<binning_mode>0</binning_mode>");
        assert_eq!(picture_writes(&raw(&more), &raw(THRESHOLD_READ), 1, &change).unwrap_err(), WriteError::NotCaptured);
        let fewer = LED_READ.replace("<ledVersion>0</ledVersion>\n", "");
        assert_eq!(led_write(&raw(&fewer), 1, &LedChange::default()).unwrap_err(), WriteError::NotCaptured);
        let reordered = AUDIO_READ.replace(
            "<timeout>0</timeout>\n<audioSelect>0</audioSelect>",
            "<audioSelect>0</audioSelect>\n<timeout>0</timeout>",
        );
        assert_eq!(volume_write(&raw(&reordered), 1, Some(50)).unwrap_err(), WriteError::NotCaptured);
        // A container where the read has a value.
        let nested = AUDIO_READ.replace("<timeout>0</timeout>", "<timeout><cur>0</cur></timeout>");
        assert_eq!(volume_write(&raw(&nested), 1, None).unwrap_err(), WriteError::NotCaptured);
        // The values themselves may differ: another camera's name and levels.
        let other = OSD_READ
            .replace("Camera1", "Gate &amp; drive")
            .replace("<language>Chinese</language>", "<language>English</language>");
        let write = osd_write(&raw(&other), 1, &OsdChange::default()).unwrap();
        assert!(write.body().contains("<name>Gate &amp; drive</name>"), "the name goes back as received");
        assert!(write.body().contains("<language>English</language>"));
    }

    #[test]
    fn malformed_replies_never_panic() {
        for bad in [
            "",
            "<body>",
            "</body>",
            "<body><OsdDatetime></body>",
            "<body><LedState><state>auto</LedState></body>",
            "<body><LedState>text<state>auto</state></LedState></body>",
            "<body><audioCfg><![CDATA[x]]></audioCfg></body>",
            "<body><aiDenoise><channelId>2</channelId><enable>1</enable><level>4</level></aiDenoise>",
            "<?xml version=\"1.0\" ?><body><aiDenoise Version=\"1.1\"><channelId>999</channelId>\
             <enable>1</enable><level>4</level></aiDenoise></body>",
            "<body><aiDenoise Version=\"1.1\"><channelId/><enable/><level/></aiDenoise></body>",
            "\u{0}<body>\u{ffff}</body>",
        ] {
            let body = raw(bad);
            let _ = osd_write(&body, 1, &OsdChange { name_position: Some(OsdPosition::TopLeft), ..Default::default() });
            let _ = picture_writes(&body, &body, 1, &PictureChange { flip: Some(true), ..Default::default() });
            let _ = led_write(&body, 1, &LedChange::default());
            let _ = volume_write(&body, 1, Some(10));
            assert!(denoise_write(&body, 2, &DenoiseChange::default()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_write_extension_names_the_channel_alone() {
        let ext = write_extension(1);
        assert_eq!(ext.len(), 104, "as captured");
        let parsed = crate::protocol::bc::xml::Extension::from_bytes(&ext).unwrap();
        assert_eq!(parsed.channel_id, Some(1));
        let text = String::from_utf8(ext).unwrap();
        assert!(!text.contains("chnType"));
        assert_eq!(write_extension(2).len(), 104);
    }

    #[test]
    fn vocabularies_and_positions_round_trip() {
        for p in OsdPosition::ALL {
            let (x, y) = p.coordinates();
            assert_eq!(OsdPosition::from_coordinates(Some(x), Some(y)), Some(p));
        }
        assert_eq!(OsdPosition::from_coordinates(Some(0), Some(0)), Some(OsdPosition::TopLeft));
        assert_eq!(OsdPosition::from_coordinates(Some(65537), Some(1)), None, "top centre is not offered");
        assert_eq!(OsdPosition::from_coordinates(None, Some(1)), None);
        for m in DayNightMode::ALL {
            assert_eq!(DayNightMode::from_wire(m.wire()), Some(m));
        }
        assert_eq!(DayNightMode::from_wire("color"), None, "not captured");
        for s in StatusLed::ALL {
            assert_eq!(StatusLed::from_wire(s.wire()), Some(s));
        }
        for s in InfraredLights::ALL {
            assert_eq!(InfraredLights::from_wire(s.wire()), Some(s));
        }
        assert_eq!(InfraredLights::from_wire("open"), None, "not captured");
    }

    #[test]
    fn each_write_belongs_to_the_model_it_was_captured_on() {
        let e1 = CapturedCamera::from_model(" E1 Outdoor PoE\n").unwrap();
        assert_eq!(e1, CapturedCamera::E1OutdoorPoe);
        assert!(!e1.writes().contains(&MSG_ID_SET_AI_DENOISE));
        assert_eq!(CapturedCamera::from_model("RLC-1224A").map(|c| c.writes()), Some(&[MSG_ID_SET_AI_DENOISE][..]));
        for model in ["", "E1 Outdoor", "RLC-1224A Pro", "Reolink Home Hub Pro"] {
            assert_eq!(CapturedCamera::from_model(model), None, "{model}");
        }
        for camera in [CapturedCamera::E1OutdoorPoe, CapturedCamera::Rlc1224a] {
            assert!(camera.writes().iter().all(|id| CAPTURED_WRITES.contains(id)));
        }
    }

    #[test]
    fn debug_prints_no_body() {
        let write = osd_write(&raw(OSD_READ), 1, &OsdChange::default()).unwrap();
        let printed = format!("{write:?} {:?}", raw(OSD_READ));
        assert!(!printed.contains("Camera1"), "{printed}");
        assert!(printed.contains("576 bytes"));
    }
}
