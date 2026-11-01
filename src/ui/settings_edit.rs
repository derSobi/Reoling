//! The camera settings Reoling can change (see `settings_write` in the
//! library): which rows of a tab become controls, the user's unsaved edits,
//! the writes a Save sends and in which order, and what the device kept
//! afterwards. Nothing here needs GTK, so the logic is tested here; the
//! camera window only renders it.
//!
//! Writes are offered for a Home Hub's cameras only (an NVR's stay read-only
//! until its own capture confirms the shapes on its firmware), on the camera
//! model each write was captured on, for a row whose current value is one the
//! capture shows, and only when the page's reply has exactly the captured
//! shape. A Save writes one edited row at a time, as the official app was
//! captured changing one value per write: before each write it reads the tab
//! again and builds the write from that read with only that row changed, so
//! a change made elsewhere since the tab was shown, and the rows the same
//! Save wrote before, are not written back. A row the camera has by its
//! turn is skipped.

use reoling::protocol::bc::settings_write::{
    denoise_write, led_write, osd_write, picture_writes, volume_write, CapturedCamera, BRIGHTNESS_RANGE,
    DENOISE_LEVELS, MSG_ID_SET_AI_DENOISE, MSG_ID_SET_AUDIO_CFG, MSG_ID_SET_DAY_NIGHT_THRESHOLD, MSG_ID_SET_LED_STATE,
    MSG_ID_SET_OSD, MSG_ID_SET_VIDEO_INPUT, VOLUME_RANGE,
};
use reoling::{
    CameraPage, ChannelAbilities, DayNightMode, DenoiseChange, DeviceKind, InfraredLights, LedChange, OsdChange,
    OsdPosition, PictureChange, SettingsPage, SettingsTarget, SettingsWrite, StatusLed, WriteError, WriteOutcome,
    WriteReply,
};

/// One setting the user may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    NamePosition,
    DateShown,
    Watermark,
    Brightness,
    Flip,
    DayNight,
    StatusLed,
    Infrared,
    Volume,
    Denoise,
    DenoiseLevel,
}

impl Field {
    /// Every field, in the order a Save writes them, one write each. Noise
    /// reduction goes before its level: a new level is written only with
    /// noise reduction on (see `locked`), which the write before has then
    /// switched on.
    pub const ALL: [Field; 11] = [
        Field::NamePosition,
        Field::DateShown,
        Field::Watermark,
        Field::Brightness,
        Field::Flip,
        Field::DayNight,
        Field::StatusLed,
        Field::Infrared,
        Field::Volume,
        Field::Denoise,
        Field::DenoiseLevel,
    ];

    /// The name a status line gives it.
    pub fn label(self) -> &'static str {
        match self {
            Field::NamePosition => "Camera name position",
            Field::DateShown => "Date and time",
            Field::Watermark => "Watermark",
            Field::Brightness => "Brightness",
            Field::Flip => "Flip",
            Field::DayNight => "Day and night mode",
            Field::StatusLed => "Status LED",
            Field::Infrared => "Infrared lights",
            Field::Volume => "Volume",
            Field::Denoise => "Noise reduction",
            Field::DenoiseLevel => "Noise reduction level",
        }
    }

    fn root(self) -> Root {
        match self {
            Field::NamePosition | Field::DateShown | Field::Watermark => Root::Osd,
            Field::Brightness | Field::Flip | Field::DayNight => Root::Picture,
            Field::StatusLed | Field::Infrared => Root::Led,
            Field::Volume => Root::Audio,
            Field::Denoise | Field::DenoiseLevel => Root::Denoise,
        }
    }
}

/// A new value for one field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    NamePosition(OsdPosition),
    DateShown(bool),
    Watermark(bool),
    Brightness(u32),
    Flip(bool),
    DayNight(DayNightMode),
    StatusLed(StatusLed),
    Infrared(InfraredLights),
    Volume(u32),
    Denoise(bool),
    DenoiseLevel(u32),
}

impl Edit {
    pub fn field(self) -> Field {
        match self {
            Edit::NamePosition(_) => Field::NamePosition,
            Edit::DateShown(_) => Field::DateShown,
            Edit::Watermark(_) => Field::Watermark,
            Edit::Brightness(_) => Field::Brightness,
            Edit::Flip(_) => Field::Flip,
            Edit::DayNight(_) => Field::DayNight,
            Edit::StatusLed(_) => Field::StatusLed,
            Edit::Infrared(_) => Field::Infrared,
            Edit::Volume(_) => Field::Volume,
            Edit::Denoise(_) => Field::Denoise,
            Edit::DenoiseLevel(_) => Field::DenoiseLevel,
        }
    }

    /// The edit a number control makes; `None` outside the field's range.
    pub fn number(field: Field, value: u32) -> Option<Edit> {
        let (range, make): (_, fn(u32) -> Edit) = match field {
            Field::Brightness => (BRIGHTNESS_RANGE, Edit::Brightness),
            Field::Volume => (VOLUME_RANGE, Edit::Volume),
            Field::DenoiseLevel => (DENOISE_LEVELS, Edit::DenoiseLevel),
            _ => return None,
        };
        range.contains(&value).then(|| make(value))
    }

    /// The value as a row or a status line says it.
    pub fn text(self) -> String {
        let on_off = |on: bool| if on { "On" } else { "Off" }.to_string();
        match self {
            Edit::NamePosition(p) => position_text(p).to_string(),
            Edit::DateShown(shown) => if shown { "Shown" } else { "Hidden" }.to_string(),
            Edit::Watermark(on) | Edit::Flip(on) | Edit::Denoise(on) => on_off(on),
            Edit::DayNight(DayNightMode::Auto) => "Auto".to_string(),
            Edit::DayNight(DayNightMode::BlackAndWhite) => "Black & white".to_string(),
            Edit::StatusLed(led) => on_off(led == StatusLed::On),
            Edit::Infrared(InfraredLights::Auto) => "Auto".to_string(),
            Edit::Infrared(InfraredLights::Off) => "Off".to_string(),
            Edit::Brightness(n) | Edit::Volume(n) | Edit::DenoiseLevel(n) => n.to_string(),
        }
    }
}

fn position_text(position: OsdPosition) -> &'static str {
    match position {
        OsdPosition::TopLeft => "Top left",
        OsdPosition::BottomRight => "Bottom right",
    }
}

/// The settings one write (or the picture's pair) carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Root {
    Osd,
    Picture,
    Led,
    Audio,
    Denoise,
}

impl Root {
    /// The writes it goes out as.
    fn writes(self) -> &'static [u32] {
        match self {
            Root::Osd => &[MSG_ID_SET_OSD],
            Root::Picture => &[MSG_ID_SET_VIDEO_INPUT, MSG_ID_SET_DAY_NIGHT_THRESHOLD],
            Root::Led => &[MSG_ID_SET_LED_STATE],
            Root::Audio => &[MSG_ID_SET_AUDIO_CFG],
            Root::Denoise => &[MSG_ID_SET_AI_DENOISE],
        }
    }
}

/// Whether the page holds the replies `root`'s writes are built from and
/// the values its rows show: the reads answered, each with its roots (a
/// refused or unanswered read keeps no reply; one without a root reads that
/// root as missing).
fn answered(page: &SettingsPage, root: Root) -> bool {
    match (root, page) {
        (Root::Osd, SettingsPage::CameraDisplay { page, .. }) => {
            page.osd_reply().is_some() && page.name.is_ok() && page.datetime.is_ok()
        }
        (Root::Picture, SettingsPage::CameraImage { page, .. }) => {
            page.picture_reply().is_some()
                && page.threshold_reply().is_some()
                && page.video_input.is_ok()
                && page.advanced.is_ok()
                && page.threshold.is_ok()
        }
        (Root::Led, SettingsPage::CameraLightSound { page, .. }) => page.led_reply().is_some() && page.led.is_ok(),
        (Root::Audio, SettingsPage::CameraLightSound { page, .. }) => {
            page.audio_reply().is_some() && page.audio.is_ok()
        }
        (Root::Denoise, SettingsPage::CameraLightSound { page, .. }) => {
            page.denoise_reply().is_some() && page.denoise.is_ok()
        }
        _ => false,
    }
}

/// `0`/`1` as a switch; anything else is not one.
fn flag(value: Option<u32>) -> Option<bool> {
    match value {
        Some(0) => Some(false),
        Some(1) => Some(true),
        _ => None,
    }
}

/// What the device has for `field`, when the page answered it with a value
/// the capture shows (else the row stays read-only).
pub fn current(page: &SettingsPage, field: Field) -> Option<Edit> {
    match (page, field) {
        (SettingsPage::CameraDisplay { page, .. }, Field::NamePosition) => {
            let name = page.name.as_ref().ok()?;
            OsdPosition::from_coordinates(name.top_left_x, name.top_left_y).map(Edit::NamePosition)
        }
        (SettingsPage::CameraDisplay { page, .. }, Field::DateShown) => {
            flag(page.datetime.as_ref().ok()?.enable).map(Edit::DateShown)
        }
        (SettingsPage::CameraDisplay { page, .. }, Field::Watermark) => {
            flag(page.name.as_ref().ok()?.watermark).map(Edit::Watermark)
        }
        (SettingsPage::CameraImage { page, .. }, Field::Brightness) => {
            page.video_input.as_ref().ok()?.bright.and_then(|b| Edit::number(Field::Brightness, b))
        }
        (SettingsPage::CameraImage { page, .. }, Field::Flip) => {
            flag(page.advanced.as_ref().ok()?.flip).map(Edit::Flip)
        }
        (SettingsPage::CameraImage { page, .. }, Field::DayNight) => {
            let mode = page.advanced.as_ref().ok()?.day_night.as_ref()?.mode.as_deref()?;
            DayNightMode::from_wire(mode).map(Edit::DayNight)
        }
        (SettingsPage::CameraLightSound { page, .. }, Field::StatusLed) => {
            StatusLed::from_wire(page.led.as_ref().ok()?.light_state.as_deref()?).map(Edit::StatusLed)
        }
        (SettingsPage::CameraLightSound { page, .. }, Field::Infrared) => {
            InfraredLights::from_wire(page.led.as_ref().ok()?.state.as_deref()?).map(Edit::Infrared)
        }
        (SettingsPage::CameraLightSound { page, .. }, Field::Volume) => {
            page.audio.as_ref().ok()?.volume.and_then(|v| Edit::number(Field::Volume, v))
        }
        (SettingsPage::CameraLightSound { page, .. }, Field::Denoise) => {
            flag(page.denoise.as_ref().ok()?.enable).map(Edit::Denoise)
        }
        (SettingsPage::CameraLightSound { page, .. }, Field::DenoiseLevel) => {
            page.denoise.as_ref().ok()?.level.and_then(|l| Edit::number(Field::DenoiseLevel, l))
        }
        _ => None,
    }
}

/// What decides whether a camera's settings may be written at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteGate {
    pub kind: DeviceKind,
    pub abilities: Option<ChannelAbilities>,
    /// The channel's camera, when its model is one a write was captured on
    /// (`None` also until the device has said which model it is).
    pub camera: Option<CapturedCamera>,
}

impl WriteGate {
    /// Whether every write of `root` was captured on this camera.
    fn captured(&self, root: Root) -> bool {
        self.camera.is_some_and(|c| root.writes().iter().all(|id| c.writes().contains(id)))
    }

    /// Whether a row gated by an ability is offered, as the read-only rows
    /// are: yes until the device has said what the channel can do.
    fn allows(&self, ability: impl Fn(&ChannelAbilities) -> bool) -> bool {
        self.abilities.as_ref().map_or(true, ability)
    }
}

/// The positions the camera name may be moved to: the captured ones, less
/// the one the date already takes.
pub fn name_positions(page: &SettingsPage) -> Vec<OsdPosition> {
    let SettingsPage::CameraDisplay { page, .. } = page else { return Vec::new() };
    let date = page.datetime.as_ref().ok().and_then(|d| OsdPosition::from_coordinates(d.top_left_x, d.top_left_y));
    OsdPosition::ALL.into_iter().filter(|p| Some(*p) != date).collect()
}

/// Whether `field` is a control on this page: a Hub's camera of the model the
/// write was captured on, a row the read-only page would show, a value the
/// capture shows, and a reply of the captured shape.
pub fn writable(page: &SettingsPage, field: Field, gate: &WriteGate) -> bool {
    if gate.kind != DeviceKind::Hub || !gate.captured(field.root()) || current(page, field).is_none() {
        return false;
    }
    let shown = match field {
        Field::StatusLed => gate.allows(|a| a.status_led),
        Field::Volume => gate.allows(|a| a.siren),
        Field::NamePosition => name_positions(page).len() > 1,
        _ => true,
    };
    shown && build(page, field.root(), &Edits::default()).is_ok()
}

/// The user's unsaved changes on one tab, at most one per field, each
/// different from what the device has.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Edits(Vec<Edit>);

impl Edits {
    /// The user chose `edit`; choosing the device's own value drops it, and
    /// so does choosing it for a row that is locked (see `locked`).
    /// Turning noise reduction off drops a level not saved yet.
    pub fn set(&mut self, edit: Edit, page: &SettingsPage) {
        self.0.retain(|e| e.field() != edit.field());
        if current(page, edit.field()) != Some(edit) {
            self.0.push(edit);
        }
        self.drop_locked(page);
    }

    pub fn get(&self, field: Field) -> Option<Edit> {
        self.0.iter().copied().find(|e| e.field() == field)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn list(&self) -> &[Edit] {
        &self.0
    }

    /// Drops the edits of these fields (saved).
    pub fn remove(&mut self, fields: &[Field]) {
        self.0.retain(|e| !fields.contains(&e.field()));
    }

    /// After a new read: an edit the device now has, of a row no longer
    /// writable, or of a row now locked, goes. An edit whose read failed
    /// this time stays (it is neither saved nor shown until a read answers).
    pub fn settle(&mut self, page: &SettingsPage, gate: &WriteGate) {
        self.0.retain(|e| {
            let field = e.field();
            !answered(page, field.root()) || (writable(page, field, gate) && current(page, field) != Some(*e))
        });
        self.drop_locked(page);
    }

    /// The new value of `field`, or what the device has.
    fn value(&self, page: &SettingsPage, field: Field) -> Option<Edit> {
        self.get(field).or_else(|| current(page, field))
    }

    fn drop_locked(&mut self, page: &SettingsPage) {
        let locked: Vec<Field> = Field::ALL.into_iter().filter(|f| locked(page, *f, self)).collect();
        self.remove(&locked);
    }
}

/// Whether `field`'s control is shown but takes no change now: the noise
/// reduction level while noise reduction is off (the user's edit, else the
/// device's value). The official app hides the level then, and its write
/// with noise reduction off was only seen with the device's own level. An
/// unknown value (the read failed) does not lock it.
pub fn locked(page: &SettingsPage, field: Field, edits: &Edits) -> bool {
    field == Field::DenoiseLevel && edits.value(page, Field::Denoise) == Some(Edit::Denoise(false))
}

/// The writes of one root with the page's edits applied (the device's own
/// values for everything else).
fn build(page: &SettingsPage, root: Root, edits: &Edits) -> Result<Vec<SettingsWrite>, WriteError> {
    match (root, page) {
        (Root::Osd, SettingsPage::CameraDisplay { channel, page }) => {
            let read = page.osd_reply().ok_or(WriteError::NotCaptured)?;
            let mut change = OsdChange::default();
            for edit in edits.list() {
                match *edit {
                    Edit::NamePosition(p) => change.name_position = Some(p),
                    Edit::DateShown(shown) => change.date_shown = Some(shown),
                    Edit::Watermark(on) => change.watermark = Some(on),
                    _ => {}
                }
            }
            Ok(vec![osd_write(read, *channel, &change)?])
        }
        (Root::Picture, SettingsPage::CameraImage { channel, page }) => {
            let picture = page.picture_reply().ok_or(WriteError::NotCaptured)?;
            let threshold = page.threshold_reply().ok_or(WriteError::NotCaptured)?;
            let mut change = PictureChange::default();
            for edit in edits.list() {
                match *edit {
                    Edit::Brightness(b) => change.brightness = Some(b),
                    Edit::Flip(on) => change.flip = Some(on),
                    Edit::DayNight(mode) => change.day_night = Some(mode),
                    _ => {}
                }
            }
            Ok(picture_writes(picture, threshold, *channel, &change)?.to_vec())
        }
        (Root::Led, SettingsPage::CameraLightSound { channel, page }) => {
            let read = page.led_reply().ok_or(WriteError::NotCaptured)?;
            let mut change = LedChange::default();
            for edit in edits.list() {
                match *edit {
                    Edit::StatusLed(led) => change.status_led = Some(led),
                    Edit::Infrared(ir) => change.infrared = Some(ir),
                    _ => {}
                }
            }
            Ok(vec![led_write(read, *channel, &change)?])
        }
        (Root::Audio, SettingsPage::CameraLightSound { channel, page }) => {
            let read = page.audio_reply().ok_or(WriteError::NotCaptured)?;
            let volume = edits.list().iter().find_map(|e| match *e {
                Edit::Volume(v) => Some(v),
                _ => None,
            });
            Ok(vec![volume_write(read, *channel, volume)?])
        }
        (Root::Denoise, SettingsPage::CameraLightSound { channel, page }) => {
            let read = page.denoise_reply().ok_or(WriteError::NotCaptured)?;
            let mut change = DenoiseChange::default();
            for edit in edits.list() {
                match *edit {
                    Edit::Denoise(on) => change.enable = Some(on),
                    Edit::DenoiseLevel(level) => change.level = Some(level),
                    _ => {}
                }
            }
            Ok(vec![denoise_write(read, *channel, &change)?])
        }
        _ => Err(WriteError::NotCaptured),
    }
}

/// One write of a Save: the message that changes one edited row (or the
/// picture's 25 with its 297), and that row's new value.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteGroup {
    pub writes: Vec<SettingsWrite>,
    pub edit: Edit,
}

/// The write of `edit`'s row, built from the page's own replies with only
/// that row's value changed.
fn row_write(page: &SettingsPage, edit: Edit) -> Result<WriteGroup, WriteError> {
    Ok(WriteGroup { writes: build(page, edit.field().root(), &Edits(vec![edit]))?, edit })
}

/// What a Save does with a read it made: the first, made when Save was
/// pressed, or one made after a row's write was answered 200.
#[derive(Debug, Clone, PartialEq)]
pub enum Refreshed {
    /// Send this row's write, built from that read.
    Save(WriteGroup),
    /// Every edited row of the Save was written, or the camera has its value
    /// now: nothing is left to write.
    Done,
    /// The read the next row's write is built from did not answer: it is
    /// not written, and the edits not saved stay.
    NotRead,
    /// Nothing was written yet and the camera already has every edited
    /// value: nothing is written.
    Already,
    /// The camera's values moved under the edits (a row can no longer be
    /// written, or is locked now): nothing more is written, and the other
    /// edits stay to be looked at and saved again.
    Changed,
    /// The next row's write could not be built from that read.
    Cannot(WriteError),
}

/// A read the Save made has come. The rows go in `Field::ALL` order, each
/// looked at on the read made before its own write: a row whose value the
/// camera has by its turn is skipped, and the next one is looked at on the
/// same read (nothing was written since); the first other row gets its
/// write, built from that read. The rows after it wait for their turn, so
/// one the camera had on an earlier read is still written when its value
/// moved since. Nothing more goes when the next row's read did not answer,
/// or when any edit can no longer be written (or is locked now); the edits
/// left then settle on that read. `wrote` tells whether a row of this Save
/// was written already.
fn refreshed(edits: &mut Edits, page: &SettingsPage, gate: &WriteGate, wrote: bool) -> Refreshed {
    let mut settled = edits.clone();
    settled.settle(page, gate);
    // An edit gone for another reason than the camera having its value.
    let changed = edits.list().iter().any(|e| settled.get(e.field()).is_none() && current(page, e.field()) != Some(*e));
    while let Some(edit) = Field::ALL.into_iter().find_map(|field| edits.get(field)) {
        let stop = if !answered(page, edit.field().root()) {
            Refreshed::NotRead
        } else if changed {
            Refreshed::Changed
        } else if current(page, edit.field()) == Some(edit) {
            edits.remove(&[edit.field()]);
            continue;
        } else {
            match row_write(page, edit) {
                Ok(group) => return Refreshed::Save(group),
                Err(e) => Refreshed::Cannot(e),
            }
        };
        *edits = settled;
        return stop;
    }
    if wrote {
        Refreshed::Done
    } else {
        Refreshed::Already
    }
}

/// What the camera window does next with a Save, once a row's write was
/// answered.
#[derive(Debug, Clone, PartialEq)]
pub enum SaveStep {
    /// Answered 200: the row is saved and its edit gone. The tab is read
    /// again, and the next row's write is built from that read.
    ReadAgain,
    /// The device refused the write `msg_id` with this code; the rows before
    /// were saved, and this one too (`row_saved`) when only the picture's
    /// 297 was refused, after its 25 answered 200. The rest is not sent.
    Refused { code: u16, msg_id: u32, saved: Vec<Edit>, row_saved: bool },
    /// The write had no answer (or could not be sent); the rows before it
    /// were saved (and this one when only the picture's 297 went
    /// unanswered), the rest is not sent.
    NoAnswer { saved: Vec<Edit> },
}

/// A Save under way, one edited row at a time: each row's write is built
/// from a read of the tab made after the write before it was answered 200,
/// and the first other answer ends it.
#[derive(Debug, Clone, PartialEq)]
pub struct SaveRun {
    pub target: SettingsTarget,
    /// The number the write that is out went under (see `sent_as`).
    group: u64,
    /// The row whose write is out; `None` while the Save waits for a read.
    out: Option<Edit>,
    saved: Vec<Edit>,
}

impl SaveRun {
    /// A Save of `target`'s edits, nothing written yet.
    pub fn new(target: SettingsTarget) -> SaveRun {
        SaveRun { target, group: 0, out: None, saved: Vec::new() }
    }

    /// A read the Save made has come: `edits` settle on it, and the next
    /// row's write is built from it (see `refreshed`).
    pub fn read(&mut self, edits: &mut Edits, page: &SettingsPage, gate: &WriteGate) -> Refreshed {
        let next = refreshed(edits, page, gate, !self.saved.is_empty());
        self.out = match &next {
            Refreshed::Save(group) => Some(group.edit),
            _ => None,
        };
        next
    }

    /// The edits saved so far.
    pub fn saved(&self) -> Vec<Edit> {
        self.saved.clone()
    }

    /// The write that is out went under this number.
    pub fn sent_as(&mut self, group: u64) {
        self.group = group;
    }

    /// The Save is given up before it ended (the device logs in again, the
    /// connection or the live video is gone). While rows are left, the rows
    /// it saved come back to `edits`, checked against the tab's last answer
    /// `page`: that answer may be a read made before their writes, and the
    /// tab then shows the values the Save was for, as edits not saved, not
    /// the old ones. With no row left (only the read after the last write
    /// was out) they stay gone, as after a Save that ended.
    pub fn given_up(&self, edits: &mut Edits, page: &SettingsPage) {
        if edits.is_empty() {
            return;
        }
        for edit in &self.saved {
            edits.set(*edit, page);
        }
    }

    /// The device's answers to the write sent under `group`; `None` when
    /// that is not the write that is out (an earlier one's, answered late).
    /// A saved row's edit leaves `edits` at once, so a read after it never
    /// offers it again, even when the camera did not keep the value.
    pub fn answered(&mut self, group: u64, results: &[WriteOutcome], edits: &mut Edits) -> Option<SaveStep> {
        if group != self.group {
            return None;
        }
        let row = self.out.take()?;
        // The row's first write carries its value (the picture's 297 after
        // it sends the threshold back unchanged): once that one answered
        // 200, the row is saved, whatever the next one's answer.
        let row_saved = results.first().is_some_and(|r| r.reply == WriteReply::Code(200));
        if row_saved {
            edits.remove(&[row.field()]);
            self.saved.push(row);
        }
        let refused = results.iter().find_map(|r| match r.reply {
            WriteReply::Code(code) if code != 200 => Some((code, r.msg_id)),
            _ => None,
        });
        let unanswered = results.is_empty() || results.iter().any(|r| !matches!(r.reply, WriteReply::Code(_)));
        Some(if let Some((code, msg_id)) = refused {
            SaveStep::Refused { code, msg_id, saved: self.saved.clone(), row_saved }
        } else if unanswered {
            SaveStep::NoAnswer { saved: self.saved.clone() }
        } else {
            SaveStep::ReadAgain
        })
    }
}

/// The footer line for a refused write: the picture's 297 refused after its
/// own 25 answered 200 (`row_saved`) says the picture change went through.
pub fn refusal(code: u16, msg_id: u32, row_saved: bool) -> String {
    if msg_id == MSG_ID_SET_DAY_NIGHT_THRESHOLD && row_saved {
        format!("The picture change was saved; the camera refused the day/night threshold sent with it (code {code}).")
    } else {
        format!("The camera refused the change (code {code}).")
    }
}

/// The saved values the device did not keep, as the read after the Save
/// shows them: "Brightness: the camera has 128".
pub fn not_kept(saved: &[Edit], page: &SettingsPage) -> Vec<String> {
    saved
        .iter()
        .filter_map(|edit| {
            let now = current(page, edit.field());
            (now != Some(*edit)).then(|| match now {
                Some(value) => format!("{}: the camera has {}", edit.field().label(), value.text()),
                None => format!("{}: not read back", edit.field().label()),
            })
        })
        .collect()
}

/// A row's control.
#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    /// A switch, with the edit each position makes.
    Switch { active: bool, on: Edit, off: Edit },
    /// A list of choices, each with the edit it makes.
    Choice { options: Vec<(String, Edit)>, selected: usize },
    /// A whole number within a range.
    Number { field: Field, min: u32, max: u32, value: u32 },
}

/// How the controls of a tab are drawn.
#[derive(Debug, Clone, Copy)]
pub struct EditView<'a> {
    pub gate: WriteGate,
    pub edits: &'a Edits,
    /// False while a Save is out or the camera cannot be reached.
    pub enabled: bool,
}

/// The control of each writable field of `page`, showing the user's edit
/// where there is one.
pub fn controls(page: &SettingsPage, view: &EditView) -> Vec<(Field, Control)> {
    Field::ALL
        .into_iter()
        .filter(|f| writable(page, *f, &view.gate))
        .filter_map(|field| Some((field, control(page, field, view.edits.value(page, field)?)?)))
        .collect()
}

fn control(page: &SettingsPage, field: Field, value: Edit) -> Option<Control> {
    let switch = |active: bool, make: fn(bool) -> Edit| Control::Switch { active, on: make(true), off: make(false) };
    let choice = |options: Vec<Edit>| {
        let selected = options.iter().position(|o| *o == value)?;
        Some(Control::Choice { options: options.into_iter().map(|o| (o.text(), o)).collect(), selected })
    };
    let number = |range: std::ops::RangeInclusive<u32>, v: u32| Control::Number {
        field,
        min: *range.start(),
        max: *range.end(),
        value: v,
    };
    match value {
        Edit::NamePosition(_) => choice(name_positions(page).into_iter().map(Edit::NamePosition).collect()),
        Edit::DateShown(on) => Some(switch(on, Edit::DateShown)),
        Edit::Watermark(on) => Some(switch(on, Edit::Watermark)),
        Edit::Flip(on) => Some(switch(on, Edit::Flip)),
        Edit::Denoise(on) => Some(switch(on, Edit::Denoise)),
        Edit::StatusLed(led) => Some(Control::Switch {
            active: led == StatusLed::On,
            on: Edit::StatusLed(StatusLed::On),
            off: Edit::StatusLed(StatusLed::Off),
        }),
        Edit::DayNight(_) => choice(DayNightMode::ALL.into_iter().map(Edit::DayNight).collect()),
        Edit::Infrared(_) => choice(InfraredLights::ALL.into_iter().map(Edit::Infrared).collect()),
        Edit::Brightness(v) => Some(number(BRIGHTNESS_RANGE, v)),
        Edit::Volume(v) => Some(number(VOLUME_RANGE, v)),
        Edit::DenoiseLevel(v) => Some(number(DENOISE_LEVELS, v)),
    }
}

/// Which tabs can hold controls.
pub fn editable_page(page: CameraPage) -> bool {
    matches!(page, CameraPage::Display | CameraPage::Image | CameraPage::LightSound)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use reoling::{
        AiDenoise, AudioCfg, CameraDisplayPage, CameraImagePage, CameraLightSoundPage, DayNight, DayNightThreshold,
        InputAdvanceCfg, LedState, MinMaxCur, OsdDatetime, OsdName, RawBody, RawReplies, ReadFailure, VideoInput,
    };

    // Replies as captured from a Home Hub Pro (firmware v3.3.0.605) on
    // 2026-10-30, and the writes the official app sent from them.
    const OSD_READ: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<OsdDatetime version=\"1.1\">\n<channelId>1</channelId>\n<enable>1</enable>\n<topLeftX>65537</topLeftX>\n<topLeftY>1</topLeftY>\n<width>0</width>\n<height>0</height>\n<language>Chinese</language>\n</OsdDatetime>\n<OsdChannelName version=\"1.1\">\n<channelId>1</channelId>\n<name>Camera1</name>\n<enable>1</enable>\n<topLeftX>65536</topLeftX>\n<topLeftY>65536</topLeftY>\n<enWatermark>0</enWatermark>\n<enBgcolor>0</enBgcolor>\n</OsdChannelName>\n</body>\n";
    const PICTURE_READ: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<VideoInput version=\"1.1\">\n<channelId>1</channelId>\n<bright>128</bright>\n<contrast>128</contrast>\n<saturation>128</saturation>\n<hue>128</hue>\n<sharpen>128</sharpen>\n<corridorMode>close</corridorMode>\n<corridorAbility>0</corridorAbility>\n</VideoInput>\n<InputAdvanceCfg version=\"1.1\">\n<channelId>1</channelId>\n<digitalChannel>1</digitalChannel>\n<separateCfg>\n<encType>0</encType>\n<constantFrameRate>0</constantFrameRate>\n</separateCfg>\n<PowerLineFrequency>\n<mode>50hz</mode>\n<enable>0</enable>\n</PowerLineFrequency>\n<Exposure>\n<mode>auto</mode>\n<Gainctl>\n<defMin>1</defMin>\n<defMax>100</defMax>\n<curMin>1</curMin>\n<curMax>62</curMax>\n</Gainctl>\n<Shutterctl>\n<defMin>0</defMin>\n<defMax>125</defMax>\n<curMin>0</curMin>\n<curMax>125</curMax>\n</Shutterctl>\n<shutterLevel>1/30</shutterLevel>\n<gainLevel>50</gainLevel>\n</Exposure>\n<Scene>\n<mode>auto</mode>\n<modeList>auto, manual</modeList>\n<Redgain>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</Redgain>\n<Bluegain>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</Bluegain>\n</Scene>\n<DayNight>\n<mode>auto</mode>\n<IrcutMode>ir</IrcutMode>\n<Threshold>medium</Threshold>\n</DayNight>\n<BLC>\n<enable>0</enable>\n<mode>backLight</mode>\n<backlight>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</backlight>\n<dynamicrange>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</dynamicrange>\n</BLC>\n<bdDayColor>\n<mode>auto</mode>\n<bright>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</bright>\n<dark>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</dark>\n</bdDayColor>\n<bdNight>\n<mode>auto</mode>\n<bright>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</bright>\n<dark>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</dark>\n</bdNight>\n<bdNightColor>\n<mode>auto</mode>\n<bright>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</bright>\n<dark>\n<min>0</min>\n<max>255</max>\n<cur>128</cur>\n</dark>\n</bdNightColor>\n<mirror>0</mirror>\n<flip>0</flip>\n<Iris>\n<enable>0</enable>\n<state>success</state>\n<focusAutoiris>0</focusAutoiris>\n</Iris>\n<nr3d>\n<value>high</value>\n<enable>1</enable>\n</nr3d>\n<firstFrameStrategy>0</firstFrameStrategy>\n<constantFrameRate>2</constantFrameRate>\n<constantFrameRateAbility>7</constantFrameRateAbility>\n<binning_mode>0</binning_mode>\n</InputAdvanceCfg>\n</body>\n";
    const THRESHOLD_READ: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<DayNightThreshold version=\"1.1\">\n<channelId>1</channelId>\n<threshold>custom</threshold>\n<stat>night</stat>\n<thresholdval>\n<min>0</min>\n<max>100</max>\n<cur>50</cur>\n</thresholdval>\n</DayNightThreshold>\n</body>\n";
    const THRESHOLD_WRITE: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<DayNightThreshold version=\"1.1\">\n<channelId>1</channelId>\n<threshold>custom</threshold>\n<thresholdVal>\n<cur>50</cur>\n</thresholdVal>\n</DayNightThreshold>\n</body>\n";
    const LED_READ: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<LedState version=\"1.1\">\n<channelId>1</channelId>\n<ledVersion>0</ledVersion>\n<IRLedBrightness>0</IRLedBrightness>\n<state>auto</state>\n<lightState>open</lightState>\n<doorbellLightState>close</doorbellLightState>\n</LedState>\n</body>\n";
    const LED_STATUS_OFF: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<LedState version=\"1.1\">\n<channelId>1</channelId>\n<state>auto</state>\n<lightState>close</lightState>\n<doorbellLightState>close</doorbellLightState>\n<IRLedBrightness>0</IRLedBrightness>\n<mode>0</mode>\n</LedState>\n</body>\n";
    const AUDIO_READ: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<audioCfg version=\"1.1\">\n<channelId>1</channelId>\n<timeout>0</timeout>\n<audioSelect>0</audioSelect>\n<volume>92</volume>\n<preAlarm>0</preAlarm>\n<audioListId>0</audioListId>\n<visitorLoudspeaker>0</visitorLoudspeaker>\n</audioCfg>\n</body>\n";
    // The RLC-1224A's reply, put on channel 1 here.
    const DENOISE_READ: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<aiDenoise Version=\"1.1\">\n<channelId>1</channelId>\n<enable>1</enable>\n<level>4</level>\n</aiDenoise>\n</body>\n";

    // A Hub's channel with the camera the OSD, picture, light and volume
    // writes were captured on, and one with the noise-reduction camera.
    pub(crate) const HUB: WriteGate =
        WriteGate { kind: DeviceKind::Hub, abilities: None, camera: Some(CapturedCamera::E1OutdoorPoe) };
    const HUB_RLC: WriteGate = WriteGate { camera: Some(CapturedCamera::Rlc1224a), ..HUB };

    fn raw(replies: &[(u32, &str)]) -> RawReplies {
        RawReplies::new(replies.iter().map(|(id, text)| (*id, RawBody::new(*text))).collect())
    }

    pub(crate) fn display() -> SettingsPage {
        SettingsPage::CameraDisplay {
            channel: 1,
            page: CameraDisplayPage {
                datetime: Ok(OsdDatetime {
                    channel_id: Some(1),
                    enable: Some(1),
                    top_left_x: Some(65537),
                    top_left_y: Some(1),
                    language: Some("Chinese".into()),
                }),
                name: Ok(OsdName {
                    channel_id: Some(1),
                    name: Some("Camera1".into()),
                    enable: Some(1),
                    top_left_x: Some(65536),
                    top_left_y: Some(65536),
                    watermark: Some(0),
                    background: Some(0),
                }),
                raw: raw(&[(44, OSD_READ)]),
            },
        }
    }

    fn image() -> SettingsPage {
        SettingsPage::CameraImage {
            channel: 1,
            page: CameraImagePage {
                video_input: Ok(VideoInput { channel_id: Some(1), bright: Some(128), ..Default::default() }),
                advanced: Ok(InputAdvanceCfg {
                    channel_id: Some(1),
                    flip: Some(0),
                    mirror: Some(0),
                    day_night: Some(DayNight { mode: Some("auto".into()), ..Default::default() }),
                    ..Default::default()
                }),
                threshold: Ok(DayNightThreshold {
                    channel_id: Some(1),
                    threshold: Some("custom".into()),
                    stat: Some("night".into()),
                    threshold_value: Some(MinMaxCur { min: Some(0), max: Some(100), cur: Some(50) }),
                }),
                raw: raw(&[(26, PICTURE_READ), (296, THRESHOLD_READ)]),
            },
        }
    }

    fn light_sound() -> SettingsPage {
        SettingsPage::CameraLightSound {
            channel: 1,
            page: CameraLightSoundPage {
                led: Ok(LedState {
                    channel_id: Some(1),
                    led_version: Some(0),
                    ir_led_brightness: Some(0),
                    state: Some("auto".into()),
                    light_state: Some("open".into()),
                    doorbell_light_state: Some("close".into()),
                }),
                spotlight: Err(ReadFailure::NotSent),
                audio: Ok(AudioCfg { channel_id: Some(1), volume: Some(92) }),
                denoise: Ok(AiDenoise { channel_id: Some(1), enable: Some(1), level: Some(4) }),
                record_audio: Err(ReadFailure::NoReply),
                raw: raw(&[(208, LED_READ), (264, AUDIO_READ), (439, DENOISE_READ)]),
            },
        }
    }

    fn fields(page: &SettingsPage, gate: &WriteGate) -> Vec<Field> {
        Field::ALL.into_iter().filter(|f| writable(page, *f, gate)).collect()
    }

    #[test]
    fn only_a_hubs_captured_rows_are_writable() {
        assert_eq!(fields(&display(), &HUB), [Field::NamePosition, Field::DateShown, Field::Watermark]);
        assert_eq!(fields(&image(), &HUB), [Field::Brightness, Field::Flip, Field::DayNight]);
        assert_eq!(fields(&light_sound(), &HUB), [Field::StatusLed, Field::Infrared, Field::Volume]);
        // Each write on the model it was captured on only.
        assert_eq!(fields(&light_sound(), &HUB_RLC), [Field::Denoise, Field::DenoiseLevel]);
        for page in [display(), image()] {
            assert!(fields(&page, &HUB_RLC).is_empty());
        }
        // A model no write was captured on, or not known yet: read-only.
        let unknown = WriteGate { camera: None, ..HUB };
        for page in [display(), image(), light_sound()] {
            assert!(fields(&page, &unknown).is_empty());
        }
        // An NVR's cameras, and a stand-alone camera, stay read-only.
        for kind in [DeviceKind::Nvr, DeviceKind::Camera] {
            for gate in [WriteGate { kind, ..HUB }, WriteGate { kind, ..HUB_RLC }] {
                for page in [display(), image(), light_sound()] {
                    assert!(fields(&page, &gate).is_empty(), "{kind:?}");
                }
            }
        }
        // The rows the read-only page leaves out by ability.
        let plain = WriteGate { abilities: Some(ChannelAbilities::default()), ..HUB };
        assert_eq!(fields(&light_sound(), &plain), [Field::Infrared]);
    }

    #[test]
    fn a_value_or_reply_the_capture_does_not_show_stays_read_only() {
        // Day/night "color", IR "open", the name at top centre: not captured.
        let SettingsPage::CameraImage { channel, mut page } = image() else { unreachable!() };
        page.advanced.as_mut().unwrap().day_night = Some(DayNight { mode: Some("color".into()), ..Default::default() });
        let color = SettingsPage::CameraImage { channel, page };
        assert_eq!(fields(&color, &HUB), [Field::Brightness, Field::Flip]);
        let SettingsPage::CameraLightSound { channel, mut page } = light_sound() else { unreachable!() };
        page.led.as_mut().unwrap().state = Some("open".into());
        let ir_on = SettingsPage::CameraLightSound { channel, page };
        assert!(!writable(&ir_on, Field::Infrared, &HUB));
        assert!(writable(&ir_on, Field::StatusLed, &HUB));
        let SettingsPage::CameraDisplay { channel, mut page } = display() else { unreachable!() };
        page.name.as_mut().unwrap().top_left_x = Some(65537);
        page.name.as_mut().unwrap().top_left_y = Some(1);
        assert!(!writable(&SettingsPage::CameraDisplay { channel, page }, Field::NamePosition, &HUB));
        // The date at top left: the name has nowhere else to go.
        let SettingsPage::CameraDisplay { channel, mut page } = display() else { unreachable!() };
        page.datetime.as_mut().unwrap().top_left_x = Some(0);
        page.datetime.as_mut().unwrap().top_left_y = Some(0);
        let crowded = SettingsPage::CameraDisplay { channel, page };
        assert_eq!(name_positions(&crowded), [OsdPosition::BottomRight]);
        assert!(!writable(&crowded, Field::NamePosition, &HUB));
        // A reply of another shape (another model): nothing on that root.
        let SettingsPage::CameraLightSound { channel, mut page } = light_sound() else { unreachable!() };
        let led = LED_READ.replace("<ledVersion>0</ledVersion>\n", "");
        page.raw = raw(&[(208, led.as_str()), (264, AUDIO_READ)]);
        let other = SettingsPage::CameraLightSound { channel, page };
        assert_eq!(fields(&other, &HUB), [Field::Volume]);
        // No 296 reply: the picture write never goes without its threshold.
        let SettingsPage::CameraImage { channel, mut page } = image() else { unreachable!() };
        page.raw = raw(&[(26, PICTURE_READ)]);
        assert!(fields(&SettingsPage::CameraImage { channel, page }, &HUB).is_empty());
    }

    #[test]
    fn edits_track_only_what_differs_from_the_device() {
        let page = display();
        let mut edits = Edits::default();
        edits.set(Edit::Watermark(true), &page);
        edits.set(Edit::NamePosition(OsdPosition::TopLeft), &page);
        assert_eq!(edits.list(), [Edit::Watermark(true), Edit::NamePosition(OsdPosition::TopLeft)]);
        // Back to the device's own value: no longer an edit.
        edits.set(Edit::Watermark(false), &page);
        assert_eq!(edits.list(), [Edit::NamePosition(OsdPosition::TopLeft)]);
        // A second choice replaces the first.
        edits.set(Edit::NamePosition(OsdPosition::BottomRight), &page);
        assert!(edits.is_empty());
        edits.set(Edit::DateShown(false), &page);
        edits.clear();
        assert!(edits.is_empty());
        // After a read: what the device now has goes, the rest stays.
        let mut edits = Edits::default();
        edits.set(Edit::Watermark(true), &page);
        edits.set(Edit::DateShown(false), &page);
        let SettingsPage::CameraDisplay { channel, page: mut read } = display() else { unreachable!() };
        read.name.as_mut().unwrap().watermark = Some(1);
        edits.settle(&SettingsPage::CameraDisplay { channel, page: read }, &HUB);
        assert_eq!(edits.list(), [Edit::DateShown(false)]);
        edits.settle(&display(), &WriteGate { kind: DeviceKind::Nvr, ..HUB });
        assert!(edits.is_empty(), "not writable any more");
    }

    #[test]
    fn a_failed_read_keeps_the_edits() {
        let page = light_sound();
        let mut edits = Edits::default();
        edits.set(Edit::StatusLed(StatusLed::Off), &page);
        edits.set(Edit::Volume(50), &page);
        // The 208 read had no answer this time (the page keeps no reply), the
        // 264 one answered with the volume already at 50.
        let SettingsPage::CameraLightSound { channel, page: mut read } = light_sound() else { unreachable!() };
        read.led = Err(ReadFailure::NoReply);
        read.audio.as_mut().unwrap().volume = Some(50);
        read.raw = raw(&[(264, AUDIO_READ), (439, DENOISE_READ)]);
        let read = SettingsPage::CameraLightSound { channel, page: read };
        edits.settle(&read, &HUB);
        assert_eq!(edits.list(), [Edit::StatusLed(StatusLed::Off)], "kept, though not shown");
        let shown = controls(&read, &EditView { gate: HUB, edits: &edits, enabled: true });
        assert!(shown.iter().all(|(f, _)| *f == Field::Volume));
        // Answered again: the edit is the user's to save.
        edits.settle(&light_sound(), &HUB);
        assert_eq!(edits.list(), [Edit::StatusLed(StatusLed::Off)]);
    }

    /// The lines of a row's first write that differ from the write the same
    /// read gives with nothing changed: what the write changes. The writes
    /// after it (the picture's 297) are those of nothing changed.
    fn changes(page: &SettingsPage, group: &WriteGroup) -> Vec<String> {
        let plain = build(page, group.edit.field().root(), &Edits::default()).unwrap();
        assert_eq!(group.writes.len(), plain.len());
        assert_eq!(group.writes[1..], plain[1..]);
        let (ours, theirs) = (group.writes[0].body(), plain[0].body());
        assert_eq!(ours.lines().count(), theirs.lines().count());
        ours.lines().zip(theirs.lines()).filter(|(a, b)| a != b).map(|(a, _)| a.to_string()).collect()
    }

    /// Every write of the group answered with `reply`.
    pub(crate) fn answer(group: &WriteGroup, reply: WriteReply) -> Vec<WriteOutcome> {
        group.writes.iter().map(|w| WriteOutcome { msg_id: w.msg_id, reply }).collect()
    }

    fn ids(group: &WriteGroup) -> Vec<u32> {
        group.writes.iter().map(|w| w.msg_id).collect()
    }

    /// The light & sound page as read after the status LED was turned off.
    fn led_off() -> SettingsPage {
        let SettingsPage::CameraLightSound { channel, mut page } = light_sound() else { unreachable!() };
        page.led.as_mut().unwrap().light_state = Some("close".into());
        let led = LED_READ.replace("<lightState>open</lightState>", "<lightState>close</lightState>");
        page.raw = raw(&[(208, led.as_str()), (264, AUDIO_READ), (439, DENOISE_READ)]);
        SettingsPage::CameraLightSound { channel, page }
    }

    /// `page` with the volume at 50, and the 264 reply as `audio` (`None`:
    /// no answer to it).
    fn with_volume_50(page: SettingsPage, audio: Option<&str>) -> SettingsPage {
        let SettingsPage::CameraLightSound { channel, mut page } = page else { unreachable!() };
        let led = page.raw.get(208).map(|r| r.as_str().to_string()).unwrap();
        match audio {
            Some(audio) => {
                page.audio.as_mut().unwrap().volume = Some(50);
                page.raw = raw(&[(208, led.as_str()), (264, audio), (439, DENOISE_READ)]);
            }
            None => {
                page.audio = Err(ReadFailure::NoReply);
                page.raw = raw(&[(208, led.as_str()), (439, DENOISE_READ)]);
            }
        }
        SettingsPage::CameraLightSound { channel, page }
    }

    #[test]
    fn a_save_writes_from_the_read_it_makes_first() {
        let page = light_sound();
        let mut edits = Edits::default();
        edits.set(Edit::StatusLed(StatusLed::Off), &page);
        // Meanwhile the infrared lights were turned off elsewhere: the 209 is
        // built from the new read, so it keeps them off.
        let SettingsPage::CameraLightSound { channel, page: mut read } = light_sound() else { unreachable!() };
        let led = LED_READ.replace("<state>auto</state>", "<state>close</state>");
        read.led.as_mut().unwrap().state = Some("close".into());
        read.raw = raw(&[(208, led.as_str()), (264, AUDIO_READ), (439, DENOISE_READ)]);
        let fresh = SettingsPage::CameraLightSound { channel, page: read };
        let mut mine = edits.clone();
        let Refreshed::Save(group) = refreshed(&mut mine, &fresh, &HUB, false) else { panic!("saved") };
        assert_eq!(mine, edits);
        assert_eq!(group.edit, Edit::StatusLed(StatusLed::Off));
        let body = group.writes[0].body();
        assert!(body.contains("<state>close</state>\n<lightState>close</lightState>"), "{}", body.len());
        assert_eq!(changes(&fresh, &group), ["<lightState>close</lightState>"]);
        // The same edit against the page as first read would have sent the
        // old infrared value back.
        let stale = row_write(&page, Edit::StatusLed(StatusLed::Off)).unwrap();
        assert!(stale.writes[0].body().contains("<state>auto</state>"));

        // The read did not answer: nothing goes, the edit stays.
        let SettingsPage::CameraLightSound { channel, page: mut read } = light_sound() else { unreachable!() };
        read.led = Err(ReadFailure::NoReply);
        read.raw = raw(&[(264, AUDIO_READ)]);
        let unread = SettingsPage::CameraLightSound { channel, page: read };
        let mut mine = edits.clone();
        assert_eq!(refreshed(&mut mine, &unread, &HUB, false), Refreshed::NotRead);
        assert_eq!(mine, edits);

        // The camera has the value already: nothing goes.
        let mut mine = edits.clone();
        assert_eq!(refreshed(&mut mine, &led_off(), &HUB, false), Refreshed::Already);
        assert!(mine.is_empty());

        // One of two edits is there already: that row is skipped, the other
        // one goes.
        let mut two = edits.clone();
        two.set(Edit::Volume(50), &page);
        let mut mine = two.clone();
        let Refreshed::Save(group) = refreshed(&mut mine, &led_off(), &HUB, false) else { panic!("the volume") };
        assert_eq!((group.edit, ids(&group)), (Edit::Volume(50), vec![265]));
        assert_eq!(mine.list(), [Edit::Volume(50)]);

        // The row can no longer be written (another shape): nothing goes.
        let SettingsPage::CameraLightSound { channel, page: mut read } = light_sound() else { unreachable!() };
        let led = LED_READ.replace("<ledVersion>0</ledVersion>\n", "");
        read.raw = raw(&[(208, led.as_str()), (264, AUDIO_READ), (439, DENOISE_READ)]);
        let other = SettingsPage::CameraLightSound { channel, page: read };
        let mut mine = edits.clone();
        assert_eq!(refreshed(&mut mine, &other, &HUB, false), Refreshed::Changed);
        assert!(mine.is_empty());
        // Nor does the volume behind it.
        let mut mine = two.clone();
        assert_eq!(refreshed(&mut mine, &other, &HUB, false), Refreshed::Changed);
        assert_eq!(mine.list(), [Edit::Volume(50)]);
    }

    #[test]
    fn a_save_writes_one_row_at_a_time_each_from_a_read_after_the_write_before() {
        // Display: two rows of the one 45 make two 45s. The name goes first
        // (`Field::ALL`), then the date, from a read that has the name moved.
        let page = display();
        let target = SettingsTarget::Camera { channel: 1, page: CameraPage::Display };
        let mut edits = Edits::default();
        edits.set(Edit::DateShown(false), &page);
        edits.set(Edit::NamePosition(OsdPosition::TopLeft), &page);
        let mut run = SaveRun::new(target);
        let Refreshed::Save(first) = run.read(&mut edits, &page, &HUB) else { panic!("the name") };
        assert_eq!((first.edit, ids(&first)), (Edit::NamePosition(OsdPosition::TopLeft), vec![45]));
        assert_eq!(changes(&page, &first), ["<topLeftX>1</topLeftX>", "<topLeftY>1</topLeftY>"]);
        assert!(first.writes[0].body().contains("<OsdFlowData"));
        run.sent_as(1);
        assert_eq!(run.answered(1, &answer(&first, WriteReply::Code(200)), &mut edits), Some(SaveStep::ReadAgain));
        assert_eq!(edits.list(), [Edit::DateShown(false)]);
        let SettingsPage::CameraDisplay { channel, page: mut read } = display() else { unreachable!() };
        let name = read.name.as_mut().unwrap();
        (name.top_left_x, name.top_left_y) = (Some(1), Some(1));
        let at = "<topLeftX>1</topLeftX>\n<topLeftY>1</topLeftY>";
        let osd = OSD_READ.replace("<topLeftX>65536</topLeftX>\n<topLeftY>65536</topLeftY>", at);
        read.raw = raw(&[(44, osd.as_str())]);
        let moved = SettingsPage::CameraDisplay { channel, page: read };
        let Refreshed::Save(second) = run.read(&mut edits, &moved, &HUB) else { panic!("the date") };
        assert_eq!((second.edit, ids(&second)), (Edit::DateShown(false), vec![45]));
        assert_eq!(changes(&moved, &second), ["<enable>0</enable>"]);
        let body = second.writes[0].body();
        assert!(body.contains(at), "the name as the read after the first write has it");
        assert!(body.contains("<OsdDatetime version=\"1.1\">\n<channelId>1</channelId>\n<enable>0</enable>"));
        run.sent_as(2);
        assert_eq!(run.answered(2, &answer(&second, WriteReply::Code(200)), &mut edits), Some(SaveStep::ReadAgain));
        assert!(edits.is_empty());
        let SettingsPage::CameraDisplay { channel, page: mut read } = moved else { unreachable!() };
        read.datetime.as_mut().unwrap().enable = Some(0);
        let after = SettingsPage::CameraDisplay { channel, page: read };
        assert_eq!(run.read(&mut edits, &after, &HUB), Refreshed::Done);
        assert_eq!(run.saved(), [Edit::NamePosition(OsdPosition::TopLeft), Edit::DateShown(false)]);
        assert!(not_kept(&run.saved(), &after).is_empty());

        // The picture: each row its own 25, each with the 297 right behind.
        let page = image();
        let target = SettingsTarget::Camera { channel: 1, page: CameraPage::Image };
        let mut edits = Edits::default();
        edits.set(Edit::Flip(true), &page);
        edits.set(Edit::Brightness(140), &page);
        let mut run = SaveRun::new(target);
        let Refreshed::Save(first) = run.read(&mut edits, &page, &HUB) else { panic!("the brightness") };
        assert_eq!((first.edit, ids(&first)), (Edit::Brightness(140), vec![25, 297]));
        assert_eq!(changes(&page, &first), ["<bright>140</bright>"]);
        assert_eq!(first.writes[1].body(), THRESHOLD_WRITE);
        assert_eq!(run.answered(0, &answer(&first, WriteReply::Code(200)), &mut edits), Some(SaveStep::ReadAgain));
        let SettingsPage::CameraImage { channel, page: mut read } = image() else { unreachable!() };
        read.video_input.as_mut().unwrap().bright = Some(140);
        let picture = PICTURE_READ.replace("<bright>128</bright>", "<bright>140</bright>");
        read.raw = raw(&[(26, picture.as_str()), (296, THRESHOLD_READ)]);
        let brighter = SettingsPage::CameraImage { channel, page: read };
        let Refreshed::Save(second) = run.read(&mut edits, &brighter, &HUB) else { panic!("the flip") };
        assert_eq!((second.edit, ids(&second)), (Edit::Flip(true), vec![25, 297]));
        assert_eq!(changes(&brighter, &second), ["<flip>1</flip>"]);
        assert!(second.writes[0].body().contains("<bright>140</bright>"));
        assert_eq!(second.writes[1].body(), THRESHOLD_WRITE);

        // Rows of different writes go one at a time too: the lights, then
        // the volume, each from a read of its own.
        let page = light_sound();
        let target = SettingsTarget::Camera { channel: 1, page: CameraPage::LightSound };
        let mut edits = Edits::default();
        edits.set(Edit::Volume(50), &page);
        edits.set(Edit::StatusLed(StatusLed::Off), &page);
        let mut run = SaveRun::new(target);
        let Refreshed::Save(first) = run.read(&mut edits, &page, &HUB) else { panic!("the lights") };
        assert_eq!(first.writes[0].body(), LED_STATUS_OFF);
        assert_eq!(changes(&page, &first), ["<lightState>close</lightState>"]);
        assert_eq!(run.answered(0, &answer(&first, WriteReply::Code(200)), &mut edits), Some(SaveStep::ReadAgain));
        let Refreshed::Save(second) = run.read(&mut edits, &led_off(), &HUB) else { panic!("the volume") };
        assert_eq!((second.edit, ids(&second)), (Edit::Volume(50), vec![265]));
        assert_eq!(changes(&led_off(), &second), ["<volume>50</volume>"]);
        assert!(first.writes.iter().chain(&second.writes).all(|w| w.channel == 1));
        assert_eq!(run.answered(0, &answer(&second, WriteReply::Code(200)), &mut edits), Some(SaveStep::ReadAgain));
        let audio = AUDIO_READ.replace("<volume>92</volume>", "<volume>50</volume>");
        assert_eq!(run.read(&mut edits, &with_volume_50(led_off(), Some(audio.as_str())), &HUB), Refreshed::Done);
        assert_eq!(run.saved(), [Edit::StatusLed(StatusLed::Off), Edit::Volume(50)]);
    }

    #[test]
    fn a_row_the_camera_has_by_its_turn_is_skipped_and_one_changed_ends_the_save() {
        let page = light_sound();
        let target = SettingsTarget::Camera { channel: 1, page: CameraPage::LightSound };
        let mut two = Edits::default();
        two.set(Edit::StatusLed(StatusLed::Off), &page);
        two.set(Edit::Volume(50), &page);
        let start = |edits: &mut Edits| {
            let mut run = SaveRun::new(target);
            let Refreshed::Save(first) = run.read(edits, &page, &HUB) else { panic!("the lights") };
            assert_eq!(run.answered(0, &answer(&first, WriteReply::Code(200)), edits), Some(SaveStep::ReadAgain));
            run
        };
        // The volume is at 50 by the read after the lights: nothing more goes.
        let audio = AUDIO_READ.replace("<volume>92</volume>", "<volume>50</volume>");
        let mut edits = two.clone();
        let mut run = start(&mut edits);
        assert_eq!(run.read(&mut edits, &with_volume_50(led_off(), Some(audio.as_str())), &HUB), Refreshed::Done);
        assert!(edits.is_empty());
        assert_eq!(run.saved(), [Edit::StatusLed(StatusLed::Off)]);
        // That read has no answer for the volume: it is not written and stays.
        let mut edits = two.clone();
        let mut run = start(&mut edits);
        assert_eq!(run.read(&mut edits, &with_volume_50(led_off(), None), &HUB), Refreshed::NotRead);
        assert_eq!(edits.list(), [Edit::Volume(50)]);
        // Its reply has another shape now: the volume is no longer written.
        let other = AUDIO_READ.replace("<timeout>0</timeout>\n", "");
        let mut edits = two.clone();
        let mut run = start(&mut edits);
        let SettingsPage::CameraLightSound { channel, page: mut read } = led_off() else { unreachable!() };
        let led = read.raw.get(208).map(|r| r.as_str().to_string()).unwrap();
        read.raw = raw(&[(208, led.as_str()), (264, other.as_str()), (439, DENOISE_READ)]);
        let changed = SettingsPage::CameraLightSound { channel, page: read };
        assert_eq!(run.read(&mut edits, &changed, &HUB), Refreshed::Changed);
        assert!(edits.is_empty());
        assert_eq!(run.saved(), [Edit::StatusLed(StatusLed::Off)]);
    }

    #[test]
    fn a_row_is_skipped_only_on_the_read_made_at_its_turn() {
        // The volume is at 50 on the read Save makes first, but the lights go
        // first. By the read after them it is back at 92 (changed elsewhere):
        // it is written then, built from that read.
        let page = light_sound();
        let target = SettingsTarget::Camera { channel: 1, page: CameraPage::LightSound };
        let mut two = Edits::default();
        two.set(Edit::StatusLed(StatusLed::Off), &page);
        two.set(Edit::Volume(50), &page);
        let audio = AUDIO_READ.replace("<volume>92</volume>", "<volume>50</volume>");
        let at_50 = with_volume_50(light_sound(), Some(audio.as_str()));
        let mut edits = two.clone();
        let mut run = SaveRun::new(target);
        let Refreshed::Save(first) = run.read(&mut edits, &at_50, &HUB) else { panic!("the lights") };
        assert_eq!(first.edit, Edit::StatusLed(StatusLed::Off));
        assert_eq!(edits, two, "the volume waits for its turn");
        assert_eq!(run.answered(0, &answer(&first, WriteReply::Code(200)), &mut edits), Some(SaveStep::ReadAgain));
        let Refreshed::Save(second) = run.read(&mut edits, &led_off(), &HUB) else { panic!("the volume") };
        assert_eq!((second.edit, ids(&second)), (Edit::Volume(50), vec![265]));
        assert_eq!(changes(&led_off(), &second), ["<volume>50</volume>"]);
        assert_eq!(run.answered(0, &answer(&second, WriteReply::Code(200)), &mut edits), Some(SaveStep::ReadAgain));
        let after = with_volume_50(led_off(), Some(audio.as_str()));
        assert_eq!(run.read(&mut edits, &after, &HUB), Refreshed::Done);
        assert_eq!(run.saved(), [Edit::StatusLed(StatusLed::Off), Edit::Volume(50)]);
        // A Save that stops on a read leaves the edits settled on it: a later
        // row the camera has by then goes, an unanswered one stays.
        let SettingsPage::CameraLightSound { channel, page: mut read } = at_50 else { unreachable!() };
        read.led = Err(ReadFailure::NoReply);
        read.raw = raw(&[(264, audio.as_str()), (439, DENOISE_READ)]);
        let mut edits = two.clone();
        let mut run = SaveRun::new(target);
        let unread = SettingsPage::CameraLightSound { channel, page: read };
        assert_eq!(run.read(&mut edits, &unread, &HUB), Refreshed::NotRead);
        assert_eq!(edits.list(), [Edit::StatusLed(StatusLed::Off)]);
    }

    #[test]
    fn a_save_given_up_with_rows_left_shows_its_saved_rows_as_edits() {
        let page = light_sound();
        let target = SettingsTarget::Camera { channel: 1, page: CameraPage::LightSound };
        let mut edits = Edits::default();
        edits.set(Edit::StatusLed(StatusLed::Off), &page);
        edits.set(Edit::Volume(50), &page);
        let mut run = SaveRun::new(target);
        let Refreshed::Save(first) = run.read(&mut edits, &page, &HUB) else { panic!("the lights") };
        assert_eq!(run.answered(0, &answer(&first, WriteReply::Code(200)), &mut edits), Some(SaveStep::ReadAgain));
        assert_eq!(edits.list(), [Edit::Volume(50)]);
        // Given up while the read for the volume is out: the tab's answer is
        // still the read before the lights' write, which has the LED on.
        let mut shown = edits.clone();
        run.given_up(&mut shown, &page);
        assert_eq!(shown.list(), [Edit::Volume(50), Edit::StatusLed(StatusLed::Off)]);
        // An answer that has the LED off already shows it as the camera's.
        let mut shown = edits.clone();
        run.given_up(&mut shown, &led_off());
        assert_eq!(shown.list(), [Edit::Volume(50)]);
        // The volume saved too: only the read after it was out, and the
        // saved rows stay gone.
        let Refreshed::Save(second) = run.read(&mut edits, &led_off(), &HUB) else { panic!("the volume") };
        assert_eq!(run.answered(0, &answer(&second, WriteReply::Code(200)), &mut edits), Some(SaveStep::ReadAgain));
        let mut shown = edits.clone();
        run.given_up(&mut shown, &page);
        assert!(shown.is_empty());
    }

    #[test]
    fn a_row_the_camera_did_not_keep_is_not_written_again() {
        // The lights are answered 200, but the read after them still has the
        // LED on: the next write is the volume's, built from that read, and
        // the read after the save names the LED.
        let page = light_sound();
        let target = SettingsTarget::Camera { channel: 1, page: CameraPage::LightSound };
        let mut edits = Edits::default();
        edits.set(Edit::StatusLed(StatusLed::Off), &page);
        edits.set(Edit::Volume(50), &page);
        let mut run = SaveRun::new(target);
        let Refreshed::Save(first) = run.read(&mut edits, &page, &HUB) else { panic!("the lights") };
        assert_eq!(run.answered(0, &answer(&first, WriteReply::Code(200)), &mut edits), Some(SaveStep::ReadAgain));
        let Refreshed::Save(second) = run.read(&mut edits, &page, &HUB) else { panic!("the volume") };
        assert_eq!(second.edit, Edit::Volume(50));
        assert!(second.writes[0].body().contains("<volume>50</volume>"));
        assert_eq!(run.answered(0, &answer(&second, WriteReply::Code(200)), &mut edits), Some(SaveStep::ReadAgain));
        let audio = AUDIO_READ.replace("<volume>92</volume>", "<volume>50</volume>");
        let after = with_volume_50(light_sound(), Some(audio.as_str()));
        assert_eq!(run.read(&mut edits, &after, &HUB), Refreshed::Done);
        assert_eq!(not_kept(&run.saved(), &after), ["Status LED: the camera has On"]);
    }

    #[test]
    fn a_save_stops_at_the_first_answer_other_than_200() {
        // Two rows of the one 45: the name's write is refused, the date's
        // never goes, and both edits stay.
        let page = display();
        let mut both = Edits::default();
        both.set(Edit::NamePosition(OsdPosition::TopLeft), &page);
        both.set(Edit::DateShown(false), &page);
        let mut edits = both.clone();
        let mut run = SaveRun::new(SettingsTarget::Camera { channel: 1, page: CameraPage::Display });
        let Refreshed::Save(first) = run.read(&mut edits, &page, &HUB) else { panic!("the name") };
        assert_eq!((first.edit, ids(&first)), (Edit::NamePosition(OsdPosition::TopLeft), vec![45]));
        run.sent_as(1);
        let step = run.answered(1, &answer(&first, WriteReply::Code(400)), &mut edits);
        assert_eq!(step, Some(SaveStep::Refused { code: 400, msg_id: 45, saved: Vec::new(), row_saved: false }));
        assert_eq!(edits, both);

        let page = light_sound();
        let target = SettingsTarget::Camera { channel: 1, page: CameraPage::LightSound };
        let mut two = Edits::default();
        two.set(Edit::StatusLed(StatusLed::Off), &page);
        two.set(Edit::Volume(50), &page);
        // The lights are refused: nothing is saved, the volume never goes.
        let mut edits = two.clone();
        let mut run = SaveRun::new(target);
        let Refreshed::Save(first) = run.read(&mut edits, &page, &HUB) else { panic!("the lights") };
        run.sent_as(1);
        // An answer under another number (an earlier save's, late) is not
        // this write's.
        assert_eq!(run.answered(7, &answer(&first, WriteReply::Code(200)), &mut edits), None);
        assert_eq!(edits, two);
        let step = run.answered(1, &answer(&first, WriteReply::Code(400)), &mut edits);
        assert_eq!(step, Some(SaveStep::Refused { code: 400, msg_id: 209, saved: Vec::new(), row_saved: false }));
        assert_eq!(refusal(400, 209, false), "The camera refused the change (code 400).");
        assert_eq!(edits, two);
        assert_eq!(run.answered(1, &answer(&first, WriteReply::Code(200)), &mut edits), None, "answered already");
        // The lights saved, the volume refused: the lights stay saved.
        let mut edits = two.clone();
        let mut run = SaveRun::new(target);
        let Refreshed::Save(first) = run.read(&mut edits, &page, &HUB) else { panic!("the lights") };
        run.sent_as(2);
        assert_eq!(run.answered(2, &answer(&first, WriteReply::Code(200)), &mut edits), Some(SaveStep::ReadAgain));
        // Waiting for the read: no write is out.
        assert_eq!(run.answered(2, &answer(&first, WriteReply::Code(200)), &mut edits), None);
        let Refreshed::Save(second) = run.read(&mut edits, &led_off(), &HUB) else { panic!("the volume") };
        run.sent_as(3);
        assert_eq!(run.answered(2, &answer(&first, WriteReply::Code(200)), &mut edits), None, "the lights' again");
        let step = run.answered(3, &answer(&second, WriteReply::Code(400)), &mut edits);
        let saved = vec![Edit::StatusLed(StatusLed::Off)];
        assert_eq!(step, Some(SaveStep::Refused { code: 400, msg_id: 265, saved, row_saved: false }));
        assert_eq!(edits.list(), [Edit::Volume(50)]);

        // The picture pair: the 25 carries the row, so it is saved once the
        // 25 answered 200, even when its 297 then has no answer or is
        // refused; the status says which.
        let page = image();
        let target = SettingsTarget::Camera { channel: 1, page: CameraPage::Image };
        let mut flip = Edits::default();
        flip.set(Edit::Flip(true), &page);
        let start = |edits: &mut Edits| {
            let mut run = SaveRun::new(target);
            let Refreshed::Save(group) = run.read(edits, &page, &HUB) else { panic!("the flip") };
            assert_eq!(ids(&group), [25, 297]);
            run
        };
        let mut results = [
            WriteOutcome { msg_id: 25, reply: WriteReply::Code(200) },
            WriteOutcome { msg_id: 297, reply: WriteReply::NoReply },
        ];
        let mut edits = flip.clone();
        let step = start(&mut edits).answered(0, &results, &mut edits);
        assert_eq!(step, Some(SaveStep::NoAnswer { saved: vec![Edit::Flip(true)] }));
        assert!(edits.is_empty());
        results[1].reply = WriteReply::Code(400);
        let mut edits = flip.clone();
        let step = start(&mut edits).answered(0, &results, &mut edits);
        let saved = vec![Edit::Flip(true)];
        assert_eq!(step, Some(SaveStep::Refused { code: 400, msg_id: 297, saved, row_saved: true }));
        assert!(refusal(400, 297, true).starts_with("The picture change was saved;"));
        // The 25 itself refused, or not answered: nothing saved.
        results[0].reply = WriteReply::Code(401);
        let mut edits = flip.clone();
        let step = start(&mut edits).answered(0, &results, &mut edits);
        assert_eq!(step, Some(SaveStep::Refused { code: 401, msg_id: 25, saved: Vec::new(), row_saved: false }));
        results[0].reply = WriteReply::NoReply;
        let mut edits = flip.clone();
        let step = start(&mut edits).answered(0, &results, &mut edits);
        assert_eq!(step, Some(SaveStep::Refused { code: 400, msg_id: 297, saved: Vec::new(), row_saved: false }));
        assert_eq!(refusal(400, 297, false), "The camera refused the change (code 400).");
        assert_eq!(edits, flip);
        let mut edits = flip.clone();
        let mut run = start(&mut edits);
        let none_sent = [25, 297].map(|msg_id| WriteOutcome { msg_id, reply: WriteReply::NotSent });
        assert_eq!(run.answered(0, &none_sent, &mut edits), Some(SaveStep::NoAnswer { saved: Vec::new() }));
        // A second picture row whose 297 is refused after its own 25 had no
        // answer: the first row is saved, this one is not, and the status
        // does not say the picture change was.
        let mut edits = flip.clone();
        edits.set(Edit::Brightness(140), &page);
        let mut run = SaveRun::new(target);
        let Refreshed::Save(first) = run.read(&mut edits, &page, &HUB) else { panic!("the brightness") };
        assert_eq!(run.answered(0, &answer(&first, WriteReply::Code(200)), &mut edits), Some(SaveStep::ReadAgain));
        let Refreshed::Save(second) = run.read(&mut edits, &page, &HUB) else { panic!("the flip") };
        assert_eq!(second.edit, Edit::Flip(true));
        let step = run.answered(0, &results, &mut edits);
        let saved = vec![Edit::Brightness(140)];
        assert_eq!(step, Some(SaveStep::Refused { code: 400, msg_id: 297, saved, row_saved: false }));
        assert_eq!(refusal(400, 297, false), "The camera refused the change (code 400).");
        assert_eq!(edits, flip);
    }

    #[test]
    fn the_read_after_a_save_tells_what_the_device_kept() {
        let saved = [Edit::Brightness(140), Edit::Flip(true)];
        let SettingsPage::CameraImage { channel, mut page } = image() else { unreachable!() };
        page.advanced.as_mut().unwrap().flip = Some(1);
        let after = SettingsPage::CameraImage { channel, page };
        assert_eq!(not_kept(&saved, &after), ["Brightness: the camera has 128"]);
        let SettingsPage::CameraImage { channel, mut page } = after else { unreachable!() };
        page.video_input = Ok(VideoInput { bright: Some(140), ..Default::default() });
        assert!(not_kept(&saved, &SettingsPage::CameraImage { channel, page: page.clone() }).is_empty());
        page.video_input = Err(ReadFailure::NoReply);
        assert_eq!(not_kept(&saved, &SettingsPage::CameraImage { channel, page }), ["Brightness: not read back"]);
    }

    #[test]
    fn controls_show_the_edit_or_the_device_value() {
        let page = display();
        let mut edits = Edits::default();
        let view = EditView { gate: HUB, edits: &edits, enabled: true };
        let all = controls(&page, &view);
        assert_eq!(all.len(), 3);
        let (_, Control::Choice { options, selected }) = &all[0] else { panic!("a choice") };
        let labels: Vec<&str> = options.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!((labels.as_slice(), *selected), (["Top left", "Bottom right"].as_slice(), 1));
        assert_eq!(all[1].1, Control::Switch { active: true, on: Edit::DateShown(true), off: Edit::DateShown(false) });
        edits.set(Edit::NamePosition(OsdPosition::TopLeft), &page);
        let view = EditView { gate: HUB, edits: &edits, enabled: true };
        let (_, Control::Choice { selected, .. }) = &controls(&page, &view)[0] else { panic!("a choice") };
        assert_eq!(*selected, 0);
        let page = image();
        let edits = Edits::default();
        let view = EditView { gate: HUB, edits: &edits, enabled: true };
        let all = controls(&page, &view);
        assert_eq!(all[0].1, Control::Number { field: Field::Brightness, min: 0, max: 255, value: 128 });
        let page = light_sound();
        let numbers = |gate: WriteGate| -> Vec<Control> {
            let all = controls(&page, &EditView { gate, edits: &edits, enabled: true });
            all.into_iter().map(|(_, c)| c).filter(|c| matches!(c, Control::Number { .. })).collect()
        };
        assert_eq!(numbers(HUB), [Control::Number { field: Field::Volume, min: 0, max: 100, value: 92 }]);
        assert_eq!(numbers(HUB_RLC), [Control::Number { field: Field::DenoiseLevel, min: 1, max: 5, value: 4 }]);
        assert_eq!(Edit::number(Field::Volume, 101), None);
        assert_eq!(Edit::number(Field::Flip, 1), None);
    }

    /// The light & sound page with noise reduction off on the device: the
    /// value shown and the reply a write is built from alike.
    fn denoise_off() -> SettingsPage {
        let SettingsPage::CameraLightSound { channel, mut page } = light_sound() else { unreachable!() };
        page.denoise.as_mut().unwrap().enable = Some(0);
        let off = DENOISE_READ.replace("<enable>1</enable>", "<enable>0</enable>");
        page.raw = raw(&[(208, LED_READ), (264, AUDIO_READ), (439, off.as_str())]);
        SettingsPage::CameraLightSound { channel, page }
    }

    #[test]
    fn the_noise_reduction_level_is_locked_while_noise_reduction_is_off() {
        let body = |page: &SettingsPage, edits: &Edits| {
            let Refreshed::Save(group) = refreshed(&mut edits.clone(), page, &HUB_RLC, false) else {
                panic!("a write")
            };
            group.writes[0].body().to_string()
        };
        // On: the level is free. A new level, then noise reduction turned
        // off: the level goes, and the write is enable 0 with the device's
        // own level, as captured.
        let on = light_sound();
        let mut edits = Edits::default();
        assert!(!locked(&on, Field::DenoiseLevel, &edits));
        edits.set(Edit::DenoiseLevel(3), &on);
        edits.set(Edit::Denoise(false), &on);
        assert_eq!(edits.list(), [Edit::Denoise(false)]);
        assert!(locked(&on, Field::DenoiseLevel, &edits));
        assert!(body(&on, &edits).contains("<enable>0</enable>\n<level>4</level>"));
        // Off: the level is locked and takes no edit. Turned on, a new level
        // goes after it: the first write turns noise reduction on with the
        // device's own level, the next, built from a read with it on, sets
        // the new level. Off again, nothing is left to save.
        let off = denoise_off();
        let mut edits = Edits::default();
        assert!(locked(&off, Field::DenoiseLevel, &edits));
        edits.set(Edit::DenoiseLevel(3), &off);
        assert!(edits.is_empty());
        edits.set(Edit::Denoise(true), &off);
        assert!(!locked(&off, Field::DenoiseLevel, &edits));
        edits.set(Edit::DenoiseLevel(3), &off);
        assert_eq!(edits.list(), [Edit::Denoise(true), Edit::DenoiseLevel(3)]);
        let target = SettingsTarget::Camera { channel: 1, page: CameraPage::LightSound };
        let start = |saving: &mut Edits| {
            let mut run = SaveRun::new(target);
            let Refreshed::Save(first) = run.read(saving, &off, &HUB_RLC) else { panic!("noise reduction") };
            assert_eq!((first.edit, ids(&first)), (Edit::Denoise(true), vec![440]));
            assert!(first.writes[0].body().contains("<enable>1</enable>\n<level>4</level>"));
            assert_eq!(run.answered(0, &answer(&first, WriteReply::Code(200)), saving), Some(SaveStep::ReadAgain));
            run
        };
        let mut saving = edits.clone();
        let mut run = start(&mut saving);
        let Refreshed::Save(then) = run.read(&mut saving, &on, &HUB_RLC) else { panic!("the level") };
        assert_eq!(then.edit, Edit::DenoiseLevel(3));
        assert!(then.writes[0].body().contains("<enable>1</enable>\n<level>3</level>"));
        assert_eq!(changes(&on, &then), ["<level>3</level>"]);
        // Still off by the read after that write: the level is locked again
        // and not written.
        let mut saving = edits.clone();
        let mut run = start(&mut saving);
        assert_eq!(run.read(&mut saving, &off, &HUB_RLC), Refreshed::Changed);
        assert!(saving.is_empty());
        edits.set(Edit::Denoise(false), &off);
        assert!(edits.is_empty());
        let view = EditView { gate: HUB_RLC, edits: &edits, enabled: true };
        let shown: Vec<Field> = controls(&off, &view).into_iter().map(|(f, _)| f).collect();
        assert_eq!(shown, [Field::Denoise, Field::DenoiseLevel], "the level is shown, locked");
        // Turned off elsewhere before a Save of a new level: nothing goes.
        let mut level = Edits::default();
        level.set(Edit::DenoiseLevel(3), &on);
        let mut mine = level.clone();
        assert_eq!(refreshed(&mut mine, &off, &HUB_RLC, false), Refreshed::Changed);
        assert!(mine.is_empty());
        // The 439 read failed: noise reduction is not known to be off, so
        // the level edit stays and nothing goes.
        let SettingsPage::CameraLightSound { channel, page: mut read } = light_sound() else { unreachable!() };
        read.denoise = Err(ReadFailure::NoReply);
        read.raw = raw(&[(208, LED_READ), (264, AUDIO_READ)]);
        let unread = SettingsPage::CameraLightSound { channel, page: read };
        assert!(!locked(&unread, Field::DenoiseLevel, &level));
        let mut mine = level.clone();
        assert_eq!(refreshed(&mut mine, &unread, &HUB_RLC, false), Refreshed::NotRead);
        assert_eq!(mine, level);
        // A 439 answered 200 without its root tells no more than a failed
        // read: the edit stays, nothing goes.
        let SettingsPage::CameraLightSound { channel, page: mut read } = light_sound() else { unreachable!() };
        read.denoise = Err(ReadFailure::Missing);
        let empty = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body/>\n";
        read.raw = raw(&[(208, LED_READ), (264, AUDIO_READ), (439, empty)]);
        let rootless = SettingsPage::CameraLightSound { channel, page: read };
        let mut mine = level.clone();
        assert_eq!(refreshed(&mut mine, &rootless, &HUB_RLC, false), Refreshed::NotRead);
        assert_eq!(mine, level);
        mine.settle(&rootless, &HUB_RLC);
        assert_eq!(mine, level);
    }
}
