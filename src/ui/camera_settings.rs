//! The camera settings window: which camera it is (product picture, model,
//! firmware) and its video encoding per stream — resolution, frame rate,
//! bitrate and I-frame interval, offered only as the device itself allows.

use crate::ui::product_image::{self, Category};
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, DropDown, Grid, Image, Label, Orientation, StringList, Window,
};
use reoling::encoding::{
    current_profile, profiles_for, EncodeProfile, EncodedStream, Encoding, GopRange, StreamEncoding,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub struct Handlers {
    /// Ask the device for this channel's settings again.
    pub on_reload: Box<dyn Fn(u8)>,
    /// Write these settings (a 57 body) to this channel.
    pub on_save: Box<dyn Fn(u8, String)>,
}

/// One editable stream's controls and the values behind each list.
struct StreamRow {
    stream: EncodedStream,
    resolution: DropDown,
    frame_rate: DropDown,
    bitrate: DropDown,
    gop: DropDown,
    codec: Label,
    resolutions: RefCell<Vec<(u32, u32)>>,
    frame_rates: RefCell<Vec<u32>>,
    bitrates: RefCell<Vec<u32>>,
    gops: RefCell<Vec<u32>>,
}

pub struct CameraSettings {
    window: Window,
    picture: Image,
    title: Label,
    model: Label,
    firmware: Label,
    status: Label,
    save: Button,
    rows: Vec<Rc<StreamRow>>,
    third: Label,
    handlers: Handlers,
    channel: Cell<Option<u8>>,
    encoding: RefCell<Option<Encoding>>,
    /// The device's profiles that cover this channel.
    profiles: RefCell<Vec<EncodeProfile>>,
    /// The main resolution's profile currently offered.
    profile: RefCell<Option<EncodeProfile>>,
    /// Programmatic list changes must not count as the user's.
    filling: Cell<bool>,
}

fn stream_label(stream: EncodedStream) -> &'static str {
    match stream {
        EncodedStream::Main => "Clear (main)",
        EncodedStream::Sub => "Fluent (sub)",
        EncodedStream::Third => "Balanced (extern)",
    }
}

fn codec_name(codec: u32) -> &'static str {
    if codec == 1 { "H.265" } else { "H.264" }
}

fn fill(dropdown: &DropDown, items: &[String], selected: usize) {
    let refs: Vec<&str> = items.iter().map(String::as_str).collect();
    dropdown.set_model(Some(&StringList::new(&refs)));
    if !items.is_empty() {
        dropdown.set_selected(selected.min(items.len() - 1) as u32);
    }
    dropdown.set_sensitive(items.len() > 1);
}

/// `value`'s place in `values`, adding it if the device's list lacks it (so
/// opening the page never silently changes a setting).
fn position(values: &mut Vec<u32>, value: u32) -> usize {
    match values.iter().position(|v| *v == value) {
        Some(i) => i,
        None => {
            values.push(value);
            values.len() - 1
        }
    }
}

impl CameraSettings {
    pub fn new(handlers: Handlers) -> Rc<Self> {
        let window = Window::builder().title("Camera settings").resizable(false).build();
        window.connect_close_request(|w| {
            w.set_visible(false);
            gtk4::glib::Propagation::Stop
        });

        let root = GtkBox::new(Orientation::Vertical, 16);
        root.set_margin_top(18);
        root.set_margin_bottom(18);
        root.set_margin_start(18);
        root.set_margin_end(18);

        // Which camera this is.
        let header = GtkBox::new(Orientation::Horizontal, 16);
        // A product photo can be over a thousand pixels wide: an Image
        // shows any paintable at its pixel size.
        let picture = Image::new();
        picture.set_pixel_size(112);
        let about = GtkBox::new(Orientation::Vertical, 4);
        about.set_valign(Align::Center);
        let title = Label::builder().xalign(0.0).css_classes(["title-2"]).build();
        let model = Label::builder().xalign(0.0).build();
        // Selectable so it can be copied, but never focused: a focused
        // selectable label shows a caret (and selects all) on open.
        let firmware = Label::builder().xalign(0.0).css_classes(["dim-label"]).selectable(true).focusable(false).build();
        about.append(&title);
        about.append(&model);
        about.append(&firmware);
        header.append(&picture);
        header.append(&about);
        root.append(&header);

        // Encoding, one row per stream.
        let heading = Label::builder().label("Encoding").xalign(0.0).css_classes(["heading"]).build();
        root.append(&heading);
        let grid = Grid::builder().row_spacing(8).column_spacing(10).build();
        for (col, text) in ["Stream", "Resolution", "Frame rate", "Bitrate", "I-frame interval", "Codec"].iter().enumerate() {
            grid.attach(&Label::builder().label(*text).xalign(0.0).css_classes(["dim-label"]).build(), col as i32, 0, 1, 1);
        }
        let mut rows = Vec::new();
        for (i, stream) in [EncodedStream::Main, EncodedStream::Sub].into_iter().enumerate() {
            let r = (i + 1) as i32;
            let row = Rc::new(StreamRow {
                stream,
                resolution: DropDown::from_strings(&[]),
                frame_rate: DropDown::from_strings(&[]),
                bitrate: DropDown::from_strings(&[]),
                gop: DropDown::from_strings(&[]),
                codec: Label::builder().xalign(0.0).build(),
                resolutions: RefCell::new(Vec::new()),
                frame_rates: RefCell::new(Vec::new()),
                bitrates: RefCell::new(Vec::new()),
                gops: RefCell::new(Vec::new()),
            });
            grid.attach(&Label::builder().label(stream_label(stream)).xalign(0.0).build(), 0, r, 1, 1);
            grid.attach(&row.resolution, 1, r, 1, 1);
            grid.attach(&row.frame_rate, 2, r, 1, 1);
            grid.attach(&row.bitrate, 3, r, 1, 1);
            grid.attach(&row.gop, 4, r, 1, 1);
            grid.attach(&row.codec, 5, r, 1, 1);
            rows.push(row);
        }
        // The third stream is shown as it is: the official app only lets
        // some cameras change it.
        let third = Label::builder().xalign(0.0).css_classes(["dim-label"]).build();
        grid.attach(&Label::builder().label(stream_label(EncodedStream::Third)).xalign(0.0).build(), 0, 3, 1, 1);
        grid.attach(&third, 1, 3, 5, 1);
        root.append(&grid);

        let footer = GtkBox::new(Orientation::Horizontal, 8);
        let status = Label::builder().xalign(0.0).hexpand(true).wrap(true).build();
        let reload = Button::with_label("Reload");
        let save = Button::with_label("Save");
        save.add_css_class("suggested-action");
        footer.append(&status);
        footer.append(&reload);
        footer.append(&save);
        root.append(&footer);
        window.set_child(Some(&root));

        let this = Rc::new(Self {
            window,
            picture,
            title,
            model,
            firmware,
            status,
            save: save.clone(),
            rows,
            third,
            handlers,
            channel: Cell::new(None),
            encoding: RefCell::new(None),
            profiles: RefCell::new(Vec::new()),
            profile: RefCell::new(None),
            filling: Cell::new(false),
        });

        let weak = Rc::downgrade(&this);
        reload.connect_clicked(move |_| {
            if let Some(s) = weak.upgrade() {
                s.reload();
            }
        });
        let weak = Rc::downgrade(&this);
        save.connect_clicked(move |_| {
            if let Some(s) = weak.upgrade() {
                s.save();
            }
        });
        // A new main resolution means another profile: every stream's
        // values go back to that profile's defaults, as in the official app.
        let weak = Rc::downgrade(&this);
        this.rows[0].resolution.connect_selected_notify(move |dropdown| {
            if let Some(s) = weak.upgrade() {
                if !s.filling.get() {
                    s.main_resolution_chosen(dropdown.selected() as usize);
                }
            }
        });
        this
    }

    /// Opens the window for one channel of a device.
    pub fn open(&self, title: &str, channel: u8, model: &str, device_profiles: &[EncodeProfile]) {
        self.channel.set(Some(channel));
        self.title.set_text(title);
        self.set_model(model, None);
        *self.profiles.borrow_mut() = profiles_for(device_profiles, channel).into_iter().cloned().collect();
        *self.encoding.borrow_mut() = None;
        self.clear_rows();
        self.status.set_text("Reading the camera's settings…");
        self.save.set_sensitive(false);
        self.window.present();
        gtk4::prelude::GtkWindowExt::set_focus(&self.window, Some(&self.save));
        (self.handlers.on_reload)(channel);
    }

    pub fn is_open_for(&self, channel: u8) -> bool {
        self.window.is_visible() && self.channel.get() == Some(channel)
    }

    /// Whether the window still needs the channel's own allowed values
    /// (the device's login list didn't cover it).
    pub fn needs_profiles(&self) -> bool {
        self.profiles.borrow().is_empty()
    }

    pub fn set_model(&self, model: &str, firmware: Option<&str>) {
        self.model.set_text(if model.is_empty() { "Model unknown" } else { model });
        self.firmware.set_text(firmware.unwrap_or(""));
        product_image::show(&self.picture, model, Category::from_model(model));
    }

    /// The channel's own allowed values (730), when the login list had none.
    pub fn set_channel_profiles(&self, profiles: Vec<EncodeProfile>) {
        // Asked about this channel alone: every profile in the answer is for
        // it, whatever its channelBits say (a Home Hub answers with bit 0).
        *self.profiles.borrow_mut() = profiles;
        let encoding = self.encoding.borrow().clone();
        if let Some(e) = encoding {
            self.set_encoding(Some(e));
        }
    }

    pub fn set_encoding(&self, encoding: Option<Encoding>) {
        let Some(encoding) = encoding else {
            self.status.set_text("The camera did not give its encoding settings.");
            return;
        };
        let profiles = self.profiles.borrow().clone();
        let refs: Vec<&EncodeProfile> = profiles.iter().collect();
        let profile = current_profile(&refs, &encoding).cloned();
        *self.profile.borrow_mut() = profile.clone();
        self.third.set_text(&encoding.third.as_ref().map(describe).unwrap_or_else(|| "—".into()));
        self.filling.set(true);
        for row in &self.rows {
            let current = encoding.stream(row.stream).cloned().unwrap_or_default();
            self.fill_row(row, &current, profile.as_ref(), &profiles);
        }
        self.filling.set(false);
        *self.encoding.borrow_mut() = Some(encoding);
        if profile.is_some() {
            self.status.set_text("");
            self.save.set_sensitive(true);
        } else {
            self.status.set_text("The camera did not say which values it allows, so its settings are shown but cannot be changed here.");
            self.save.set_sensitive(false);
            for row in &self.rows {
                for d in [&row.resolution, &row.frame_rate, &row.bitrate, &row.gop] {
                    d.set_sensitive(false);
                }
            }
        }
    }

    /// The device's answer to a save.
    pub fn saved(&self, code: u16) {
        if code == 200 {
            self.status.set_text("Saved. The live picture restarts with the new settings.");
            self.reload_quietly();
        } else {
            self.status.set_text(&format!("The camera refused the change (code {code})."));
            self.save.set_sensitive(true);
        }
    }

    pub fn failed(&self, reason: &str) {
        self.status.set_text(reason);
        self.save.set_sensitive(true);
    }

    fn reload(&self) {
        let Some(channel) = self.channel.get() else { return };
        self.status.set_text("Reading the camera's settings…");
        (self.handlers.on_reload)(channel);
    }

    fn reload_quietly(&self) {
        if let Some(channel) = self.channel.get() {
            (self.handlers.on_reload)(channel);
        }
    }

    fn clear_rows(&self) {
        self.filling.set(true);
        for row in &self.rows {
            for d in [&row.resolution, &row.frame_rate, &row.bitrate, &row.gop] {
                fill(d, &[], 0);
            }
            row.codec.set_text("");
        }
        self.third.set_text("");
        self.filling.set(false);
    }

    fn fill_row(&self, row: &StreamRow, current: &StreamEncoding, profile: Option<&EncodeProfile>, all: &[EncodeProfile]) {
        let table = profile.and_then(|p| p.table(row.stream));
        // Resolutions: for main, every profile's main resolution; for the
        // others, what the current profile pairs with main.
        let mut resolutions: Vec<(u32, u32)> = match row.stream {
            EncodedStream::Main => {
                let mut v: Vec<(u32, u32)> = all
                    .iter()
                    .filter_map(|p| p.table(EncodedStream::Main).map(|t| (t.width, t.height)))
                    .collect();
                v.dedup();
                v
            }
            _ => table.map(|t| vec![(t.width, t.height)]).unwrap_or_default(),
        };
        let res_at = match resolutions.iter().position(|r| *r == (current.width, current.height)) {
            Some(i) => i,
            None => {
                resolutions.push((current.width, current.height));
                resolutions.len() - 1
            }
        };
        let mut frame_rates = table.map(|t| t.frame_rates.clone()).unwrap_or_default();
        let fps_at = position(&mut frame_rates, current.frame_rate);
        let mut bitrates = table.map(|t| t.bitrates.clone()).unwrap_or_default();
        let bitrate_at = position(&mut bitrates, current.bitrate);
        let (mut gops, gop_at) = match current.gop {
            // A sane interval range is a handful of seconds; anything
            // else from the device is not offered (and never allocated).
            Some(g) if g.max >= g.min && g.min > 0 && g.max - g.min <= 30 => {
                let mut v: Vec<u32> = (g.min..=g.max).collect();
                let at = position(&mut v, g.current);
                (v, at)
            }
            _ => (Vec::new(), 0),
        };
        gops.sort_unstable();
        fill(&row.resolution, &resolutions.iter().map(|(w, h)| format!("{w}×{h}")).collect::<Vec<_>>(), res_at);
        fill(&row.frame_rate, &frame_rates.iter().map(|f| format!("{f} fps")).collect::<Vec<_>>(), fps_at);
        fill(&row.bitrate, &bitrates.iter().map(|b| format!("{b} kbit/s")).collect::<Vec<_>>(), bitrate_at);
        fill(&row.gop, &gops.iter().map(|g| format!("{g}×")).collect::<Vec<_>>(), gop_at.min(gops.len().saturating_sub(1)));
        row.codec.set_text(codec_name(current.codec));
        *row.resolutions.borrow_mut() = resolutions;
        *row.frame_rates.borrow_mut() = frame_rates;
        *row.bitrates.borrow_mut() = bitrates;
        *row.gops.borrow_mut() = gops;
    }

    fn main_resolution_chosen(&self, index: usize) {
        let Some((w, h)) = self.rows[0].resolutions.borrow().get(index).copied() else { return };
        let profiles = self.profiles.borrow().clone();
        let Some(profile) = profiles
            .iter()
            .find(|p| p.table(EncodedStream::Main).is_some_and(|t| (t.width, t.height) == (w, h)))
            .cloned()
        else {
            return;
        };
        let encoding = self.encoding.borrow().clone();
        self.filling.set(true);
        for row in &self.rows {
            let Some(table) = profile.table(row.stream) else { continue };
            let current = encoding.as_ref().and_then(|e| e.stream(row.stream).cloned()).unwrap_or_default();
            let defaults = StreamEncoding {
                width: table.width,
                height: table.height,
                frame_rate: table.default_frame_rate,
                bitrate: table.default_bitrate,
                codec: current.codec,
                gop: current.gop.map(|g| GopRange { current: table.default_gop.unwrap_or(g.current), ..g }),
            };
            self.fill_row(row, &defaults, Some(&profile), &profiles);
        }
        self.filling.set(false);
        *self.profile.borrow_mut() = Some(profile);
    }

    /// What the rows now say, for one stream.
    fn chosen(&self, row: &StreamRow, current: &StreamEncoding) -> StreamEncoding {
        let pick = |values: &RefCell<Vec<u32>>, d: &DropDown, fallback: u32| {
            values.borrow().get(d.selected() as usize).copied().unwrap_or(fallback)
        };
        let (width, height) = row
            .resolutions
            .borrow()
            .get(row.resolution.selected() as usize)
            .copied()
            .unwrap_or((current.width, current.height));
        StreamEncoding {
            width,
            height,
            frame_rate: pick(&row.frame_rates, &row.frame_rate, current.frame_rate),
            bitrate: pick(&row.bitrates, &row.bitrate, current.bitrate),
            codec: current.codec,
            gop: current.gop.map(|g| GopRange { current: pick(&row.gops, &row.gop, g.current), ..g }),
        }
    }

    fn save(&self) {
        let (Some(channel), Some(encoding)) = (self.channel.get(), self.encoding.borrow().clone()) else { return };
        let mut body_source = encoding.clone();
        let mut changed = false;
        let mut body = String::new();
        for row in &self.rows {
            let Some(current) = encoding.stream(row.stream).cloned() else { continue };
            let wanted = self.chosen(row, &current);
            if wanted != current {
                changed = true;
            }
            body = body_source.with_changes(row.stream, &wanted);
            if let Some(next) = Encoding::parse(&body) {
                body_source = next;
            }
        }
        if !changed && std::env::var("REOLING_ENCODING_NOOP_WRITE").is_err() {
            self.status.set_text("Nothing changed.");
            return;
        }
        self.save.set_sensitive(false);
        self.status.set_text("Saving…");
        (self.handlers.on_save)(channel, body);
    }
}

fn describe(s: &StreamEncoding) -> String {
    format!("{}×{}, {} fps, {} kbit/s, {}", s.width, s.height, s.frame_rate, s.bitrate, codec_name(s.codec))
}
