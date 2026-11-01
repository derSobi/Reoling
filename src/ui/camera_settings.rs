//! The camera settings window: which camera it is (product picture, model,
//! firmware), its video encoding per stream — resolution, frame rate,
//! bitrate and I-frame interval, offered only as the device itself allows —
//! and, in further tabs, everything else the camera tells about itself:
//! picture, display, detection, lights and sound, schedules, network. On a
//! Home Hub's camera the few settings Reoling can write (see
//! `settings_edit`) are controls with a Save and a Revert per tab; the rest
//! is shown read-only.

use crate::ui::product_image::{self, Category};
use crate::ui::settings_edit::{self, Edit, EditView, Edits, Field, Refreshed, SaveRun, SaveStep, WriteGate};
use crate::ui::settings_pages::{self, Context};
use crate::ui::settings_view::{OnEdit, ReadOnlyPage, LIVE_STOPPED, NOT_CONNECTED, READING, WAITING_FOR_DOWNLOAD};
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, DropDown, Grid, Image, Label, Orientation, Stack, StackSwitcher, StringList, Window,
};
use reoling::encoding::{
    current_profile, profiles_for, EncodeProfile, EncodedStream, Encoding, GopRange, StreamEncoding,
};
use reoling::protocol::bc::settings_write::CapturedCamera;
use reoling::{CameraPage, ChannelAbilities, DeviceKind, SettingsPage, SettingsTarget, SettingsWrite, WriteOutcome};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

pub struct Handlers {
    /// Ask the device for this channel's encoding settings again.
    pub on_reload: Box<dyn Fn(u8)>,
    /// Write these settings (a 57 body) to this channel.
    pub on_save: Box<dyn Fn(u8, String)>,
    /// Ask the device for one settings tab of this channel under the
    /// window's number for this read (the page comes back with it); `false`
    /// when there is no connection to ask over.
    pub on_read_page: Box<dyn Fn(u8, CameraPage, u64) -> bool>,
    /// Send one group of writes for this device's (by key) channel's tab
    /// under the window's number for it (the answers come back with it);
    /// `false` when there is no connection to send it over, or that device
    /// is not the one watched.
    pub on_write: Box<dyn Fn(&str, u8, CameraPage, u64, Vec<SettingsWrite>) -> bool>,
}

/// A writable tab's unsaved edits and its save.
#[derive(Default)]
struct EditTab {
    edits: Edits,
    /// Save was pressed and the tab is being read again first: the writes
    /// are built from that read (see `set_page`).
    refreshing: bool,
    run: Option<SaveRun>,
    /// A save ended and the tab is being read again: Save waits for that
    /// read, which every next write is built from.
    rereading: bool,
    /// What the save wrote, to compare with that read.
    check: Vec<Edit>,
    /// The number of the read the tab waits for (a Save's read first, the
    /// read after a save, a Reload's): only its page ends the wait.
    awaited: Option<u64>,
    /// The footer line on this tab.
    status: String,
    /// Its save was given up while the window was closed: what the tab
    /// says once the window is opened again for its channel (see
    /// `reopened`).
    dropped: Option<&'static str>,
}

impl EditTab {
    fn busy(&self) -> bool {
        self.saving() || self.rereading
    }

    /// Whether the page of the read sent under `read` is the one the tab
    /// waits for, or the tab waits for none; the wait is then over. The page
    /// of an older read can still come while a newer one is out (it was
    /// complete before the newer read went): it is only shown, nothing is
    /// built from it or checked against it.
    fn ends_wait(&mut self, read: u64) -> bool {
        if self.awaited.is_some_and(|awaited| awaited != read) {
            return false;
        }
        self.awaited = None;
        self.rereading = false;
        true
    }

    /// A Save is under way: its read first, or its writes.
    fn saving(&self) -> bool {
        self.refreshing || self.run.is_some()
    }

    /// A save or re-read under way is given up, the tab saying `reason`;
    /// whether it had one.
    fn give_up(&mut self, reason: &str) -> bool {
        let had = self.run.take().is_some() || self.rereading || self.refreshing;
        if had {
            self.status = reason.to_string();
        }
        self.refreshing = false;
        self.rereading = false;
        self.awaited = None;
        self.check.clear();
        had
    }

    /// The same while the window is closed: a save given up says `note`
    /// once the window is opened again (a re-read after a save that was
    /// over is only given up, as before).
    fn give_up_closed(&mut self, note: &'static str) {
        if self.saving() {
            self.dropped = Some(note);
        }
        self.give_up(note);
    }

    /// What the tab says when the window is opened again for its channel,
    /// which drops its save and its edits; `None` when it had neither.
    fn reopened(&self) -> Option<&'static str> {
        if self.saving() {
            Some(SAVE_DROPPED)
        } else if self.dropped.is_some() {
            self.dropped
        } else if !self.edits.is_empty() {
            Some(UNSAVED_DROPPED)
        } else {
            None
        }
    }
}

/// A Save's read first had no answer for what it writes.
const NOT_READ_BEFORE_SAVE: &str =
    "The camera did not answer the read before the save; nothing was written. Save to try again.";
/// A Save's read first showed the camera has every edited value.
const ALREADY_BEFORE_SAVE: &str = "The camera already has these values; nothing was written.";
/// A Save's read first showed other values than the tab did.
const CHANGED_BEFORE_SAVE: &str =
    "The camera's values changed since the tab was read; nothing was written. Check the tab and Save again.";
/// The same, and no edit is left to save.
const CHANGED_NOTHING_LEFT: &str = "The camera's values changed since the tab was read; nothing was written.";
/// The window was opened again while a save of its channel was under way.
const SAVE_DROPPED: &str =
    "The window was opened again during a save: what was not sent yet was dropped. The tab shows what the camera has.";
/// Reload, or the window opened again, while a tab had edits not saved.
const UNSAVED_DROPPED: &str = "Unsaved changes dropped.";
/// The device logs in again: its old connection, with whatever a save had
/// out on it, is gone.
const RELOGIN_DROPPED: &str =
    "The device is logging in again; what was not sent yet was dropped. Reload to see what the camera has.";
/// The live video was stopped (Stop, the Playback page) while the window
/// was closed during a save.
const STOPPED_DURING_SAVE: &str =
    "Live video was stopped during a save: what was not sent yet was dropped. The tab shows what the camera has.";
/// The connection was lost while the window was closed during a save.
const LOST_DURING_SAVE: &str =
    "The connection was lost during a save: what was not sent yet was dropped. The tab shows what the camera has.";

/// The stack name of the Encoding tab; the read-only tabs go by `page_name`.
const ENCODING_TAB: &str = "encoding";

fn page_name(page: CameraPage) -> &'static str {
    match page {
        CameraPage::Info => "info",
        CameraPage::Image => "image",
        CameraPage::Display => "display",
        CameraPage::Detection => "detection",
        CameraPage::LightSound => "light-sound",
        CameraPage::Schedules => "schedules",
        CameraPage::Network => "network",
    }
}

fn page_title(page: CameraPage) -> &'static str {
    match page {
        CameraPage::Info => "Info",
        CameraPage::Image => "Image",
        CameraPage::Display => "Display",
        CameraPage::Detection => "Detection",
        CameraPage::LightSound => "Light & sound",
        CameraPage::Schedules => "Schedules",
        CameraPage::Network => "Network",
    }
}

fn page_of(name: &str) -> Option<CameraPage> {
    CameraPage::ALL.into_iter().find(|p| page_name(*p) == name)
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
    me: Weak<CameraSettings>,
    window: Window,
    picture: Image,
    title: Label,
    model: Label,
    firmware: Label,
    /// The footer line: the Encoding tab's status (the read-only tabs have
    /// their own line each).
    status: Label,
    encoding_status: RefCell<String>,
    save: Button,
    /// Save and Revert of the writable tabs (Encoding keeps its own Save).
    apply: Button,
    revert: Button,
    reload: Button,
    stack: Stack,
    /// The read-only tabs, in tab order.
    pages: Vec<(CameraPage, ReadOnlyPage)>,
    /// Tabs read at least once since the window was opened.
    loaded: RefCell<HashSet<CameraPage>>,
    /// The last answer per tab, shown again when the abilities arrive.
    answers: RefCell<HashMap<CameraPage, SettingsPage>>,
    abilities: Cell<Option<ChannelAbilities>>,
    rows: Vec<Rc<StreamRow>>,
    third: Label,
    handlers: Handlers,
    /// The device the window was last opened for (its key): only its tabs'
    /// answers are taken, and its writes go to it alone.
    key: RefCell<Option<String>>,
    channel: Cell<Option<u8>>,
    /// The device the channel belongs to: only a Hub's cameras are written.
    kind: Cell<DeviceKind>,
    /// The channel's camera, when it is a model a write was captured on:
    /// only those writes are offered.
    camera: Cell<Option<CapturedCamera>>,
    /// The connection can carry a write; false once it is lost or the live
    /// video is stopped, until the window is opened anew.
    online: Cell<bool>,
    edit_tabs: RefCell<HashMap<CameraPage, EditTab>>,
    /// The last number a group of writes went under (never reused while
    /// the app runs).
    last_group: Cell<u64>,
    /// The last number a read of a tab went under (never reused either).
    last_read: Cell<u64>,
    /// The channel's live video is on, so the window has a connection to
    /// read over; false after Stop or on the Playback page, until the
    /// window is opened anew for the channel.
    watched: Cell<bool>,
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
        let window = Window::builder().title("Camera settings").default_width(760).default_height(640).build();
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

        // The tabs: Encoding (editable, as before) and the read-only pages.
        let stack = Stack::new();
        stack.set_vexpand(true);
        stack.set_transition_type(gtk4::StackTransitionType::None);
        let switcher = StackSwitcher::new();
        switcher.set_stack(Some(&stack));
        switcher.set_halign(Align::Center);
        root.append(&switcher);
        root.append(&stack);

        // Encoding, one row per stream.
        let encoding_page = GtkBox::new(Orientation::Vertical, 16);
        encoding_page.set_margin_top(4);
        encoding_page.set_valign(Align::Start);
        let heading = Label::builder().label("Encoding").xalign(0.0).css_classes(["heading"]).build();
        encoding_page.append(&heading);
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
        encoding_page.append(&grid);
        stack.add_titled(&encoding_page, Some(ENCODING_TAB), "Encoding");

        let mut pages = Vec::new();
        for page in CameraPage::ALL {
            let view = ReadOnlyPage::new();
            stack.add_titled(view.widget(), Some(page_name(page)), page_title(page));
            pages.push((page, view));
        }

        let footer = GtkBox::new(Orientation::Horizontal, 8);
        let status = Label::builder().xalign(0.0).hexpand(true).wrap(true).build();
        let reload = Button::with_label("Reload");
        let save = Button::with_label("Save");
        save.add_css_class("suggested-action");
        let revert = Button::with_label("Revert");
        revert.set_tooltip_text(Some("Drop the changes not saved yet"));
        revert.set_visible(false);
        let apply = Button::with_label("Save");
        apply.add_css_class("suggested-action");
        apply.set_visible(false);
        footer.append(&status);
        footer.append(&reload);
        footer.append(&revert);
        footer.append(&save);
        footer.append(&apply);
        root.append(&footer);
        window.set_child(Some(&root));

        let this = Rc::new_cyclic(|me| Self {
            me: me.clone(),
            window,
            picture,
            title,
            model,
            firmware,
            status,
            encoding_status: RefCell::new(String::new()),
            save: save.clone(),
            apply: apply.clone(),
            revert: revert.clone(),
            reload: reload.clone(),
            stack: stack.clone(),
            pages,
            loaded: RefCell::new(HashSet::new()),
            answers: RefCell::new(HashMap::new()),
            abilities: Cell::new(None),
            rows,
            third,
            handlers,
            key: RefCell::new(None),
            channel: Cell::new(None),
            kind: Cell::new(DeviceKind::Camera),
            camera: Cell::new(None),
            online: Cell::new(false),
            edit_tabs: RefCell::new(HashMap::new()),
            last_group: Cell::new(0),
            last_read: Cell::new(0),
            watched: Cell::new(true),
            encoding: RefCell::new(None),
            profiles: RefCell::new(Vec::new()),
            profile: RefCell::new(None),
            filling: Cell::new(false),
        });

        let weak = Rc::downgrade(&this);
        reload.connect_clicked(move |_| {
            if let Some(s) = weak.upgrade() {
                match s.current_page() {
                    Some(page) => s.reload_page(page),
                    None => s.reload(),
                }
            }
        });
        let weak = Rc::downgrade(&this);
        apply.connect_clicked(move |_| {
            if let Some(s) = weak.upgrade() {
                s.apply();
            }
        });
        let weak = Rc::downgrade(&this);
        revert.connect_clicked(move |_| {
            if let Some(s) = weak.upgrade() {
                s.revert();
            }
        });
        // A tab is read the first time it is shown; Save belongs to Encoding.
        let weak = Rc::downgrade(&this);
        stack.connect_visible_child_name_notify(move |_| {
            if let Some(s) = weak.upgrade() {
                s.tab_changed();
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

    /// Opens the window for one channel of a device (`key`), on the Encoding
    /// tab. `kind` is the device the channel belongs to (an NVR's cameras
    /// have no Network tab) and `abilities` what the channel's camera can
    /// do, as far as the device has said. A window already on screen is not
    /// raised (see `present`): it is opened again for every channel the
    /// sidebar switches to.
    pub fn open(
        &self,
        key: &str,
        title: &str,
        channel: u8,
        model: &str,
        device_profiles: &[EncodeProfile],
        kind: DeviceKind,
        abilities: Option<ChannelAbilities>,
    ) {
        // Opened again for the same camera: a save still under way is given
        // up (what it has not sent yet is dropped) and unsaved edits go, each
        // such tab saying so (the tab is read again when shown); a tab whose
        // save was given up while the window was closed says why (see
        // `EditTab::reopened`). Another camera's edits go without a word.
        let dropped: Vec<(CameraPage, &str)> = if self.is_for(key, channel) {
            let tabs = self.edit_tabs.borrow();
            tabs.iter().filter_map(|(page, t)| Some((*page, t.reopened()?))).collect()
        } else {
            Vec::new()
        };
        *self.key.borrow_mut() = Some(key.to_string());
        self.channel.set(Some(channel));
        self.watched.set(true);
        self.online.set(true);
        self.kind.set(kind);
        {
            let mut tabs = self.edit_tabs.borrow_mut();
            tabs.clear();
            for (page, status) in dropped {
                tabs.entry(page).or_default().status = status.to_string();
            }
        }
        self.abilities.set(abilities);
        self.title.set_text(title);
        self.camera.set(CapturedCamera::from_model(model));
        self.set_model(model, None);
        *self.profiles.borrow_mut() = profiles_for(device_profiles, channel).into_iter().cloned().collect();
        *self.encoding.borrow_mut() = None;
        self.clear_rows();
        self.loaded.borrow_mut().clear();
        self.answers.borrow_mut().clear();
        for (_, view) in &self.pages {
            view.clear();
        }
        // Encoding first, then the Network tab is hidden where it does not
        // apply: hiding the tab on show would make the stack pick another
        // one, which would then be read for nothing.
        self.stack.set_visible_child_name(ENCODING_TAB);
        for (page, view) in &self.pages {
            // An NVR answers a camera's network read with its own address
            // block, so that tab is left out there.
            let shown = *page != CameraPage::Network || kind != DeviceKind::Nvr;
            self.stack.page(view.widget()).set_visible(shown);
        }
        self.show_status("Reading the camera's settings…");
        self.save.set_sensitive(false);
        if !self.window.is_visible() {
            self.present();
        }
        (self.handlers.on_reload)(channel);
    }

    /// Raises the window, with the focus on Save. `open` leaves a window
    /// already on screen where it is: opened again for another channel
    /// while the sidebar is being clicked, it must not come over the main
    /// window and take the keyboard.
    pub fn present(&self) {
        self.window.present();
        gtk4::prelude::GtkWindowExt::set_focus(&self.window, Some(&self.save));
    }

    /// The channel's abilities, when they arrive after the window opened:
    /// the rows they gate are shown again.
    pub fn set_abilities(&self, channel: u8, abilities: ChannelAbilities) {
        if self.channel.get() != Some(channel) || self.abilities.get() == Some(abilities) {
            return;
        }
        self.abilities.set(Some(abilities));
        let answers: Vec<SettingsPage> = self.answers.borrow().values().cloned().collect();
        for page in answers {
            self.render(&page);
        }
        self.update_footer();
    }

    /// One tab's answer to the read sent under `read`; ignored unless it is
    /// this channel's. On a writable tab it may be the read a Save makes
    /// first, which the writes are then built from, or the read after a
    /// save: the edits the device now has go, and what it did not keep is
    /// said. Either is taken only from the read the tab waits for (see
    /// `EditTab::ends_wait`).
    pub fn set_page(&self, page: &SettingsPage, read: u64) {
        let SettingsTarget::Camera { channel, page: which } = page.target() else { return };
        if self.channel.get() != Some(channel) {
            return;
        }
        self.answers.borrow_mut().insert(which, page.clone());
        let mut groups = None;
        if settings_edit::editable_page(which) {
            let gate = self.gate();
            let mut tabs = self.edit_tabs.borrow_mut();
            let tab = tabs.entry(which).or_default();
            if tab.ends_wait(read) {
                if std::mem::take(&mut tab.refreshing) {
                    // The read a Save made first: the writes are built from it.
                    match settings_edit::refreshed(&mut tab.edits, page, &gate) {
                        Refreshed::Save(g) => groups = Some(g),
                        Refreshed::NotRead => tab.status = NOT_READ_BEFORE_SAVE.to_string(),
                        Refreshed::Already => tab.status = ALREADY_BEFORE_SAVE.to_string(),
                        Refreshed::Changed if tab.edits.is_empty() => tab.status = CHANGED_NOTHING_LEFT.to_string(),
                        Refreshed::Changed => tab.status = CHANGED_BEFORE_SAVE.to_string(),
                        Refreshed::Cannot(e) => tab.status = format!("Cannot save: {e}."),
                    }
                } else {
                    tab.edits.settle(page, &gate);
                }
                if !tab.check.is_empty() {
                    let kept = settings_edit::not_kept(&tab.check, page);
                    if !kept.is_empty() {
                        tab.status = format!("{} The camera kept other values: {}.", tab.status, kept.join("; "));
                    }
                    tab.check.clear();
                }
            }
        }
        // The save goes out before the tab is drawn, so its controls come
        // back turned off.
        if let Some(groups) = groups {
            self.send_save(which, groups);
        }
        self.render(page);
        self.update_footer();
    }

    /// A download holds the connection; the tab is read once it is over.
    pub fn page_deferred(&self, target: &SettingsTarget) {
        let SettingsTarget::Camera { channel, page } = target else { return };
        if self.channel.get() != Some(*channel) {
            return;
        }
        if let Some(view) = self.view(*page) {
            view.set_status(WAITING_FOR_DOWNLOAD);
        }
    }

    fn gate(&self) -> WriteGate {
        WriteGate { kind: self.kind.get(), abilities: self.abilities.get(), camera: self.camera.get() }
    }

    /// Whether a write can go out now (a connection, live video on).
    fn can_write(&self) -> bool {
        self.online.get() && self.watched.get()
    }

    fn render(&self, page: &SettingsPage) {
        let SettingsTarget::Camera { page: which, .. } = page.target() else { return };
        let Some(view) = self.view(which) else { return };
        let tab = {
            let tabs = self.edit_tabs.borrow();
            let none = Edits::default();
            let state = tabs.get(&which);
            let edit = settings_edit::editable_page(which).then(|| EditView {
                gate: self.gate(),
                edits: state.map_or(&none, |t| &t.edits),
                enabled: self.can_write() && !state.is_some_and(EditTab::busy),
            });
            let ctx = Context { abilities: self.abilities.get(), edit, ..Context::default() };
            settings_pages::build(page, &ctx)
        };
        let me = self.me.clone();
        let on_edit: OnEdit = Rc::new(move |edit| {
            if let Some(s) = me.upgrade() {
                s.edited(which, edit);
            }
        });
        view.show_editable(&tab, Some(on_edit));
    }

    /// The user changed a control of `which`.
    fn edited(&self, which: CameraPage, edit: Edit) {
        let Some(page) = self.answers.borrow().get(&which).cloned() else { return };
        {
            let mut tabs = self.edit_tabs.borrow_mut();
            let tab = tabs.entry(which).or_default();
            if tab.busy() {
                return;
            }
            tab.edits.set(edit, &page);
            tab.status = if tab.edits.is_empty() { String::new() } else { "Not saved yet.".to_string() };
        }
        // Noise reduction turned on or off frees or locks its level (and
        // drops a level not saved yet): the tab is drawn again once the
        // switch's own handler is over, the level showing what will be sent.
        if edit.field() == Field::Denoise {
            let me = self.me.clone();
            gtk4::glib::idle_add_local_once(move || {
                if let Some(s) = me.upgrade() {
                    s.redraw(which);
                }
            });
        }
        self.update_footer();
    }

    /// Draws `which` again from its last answer, with the edits as they are.
    fn redraw(&self, which: CameraPage) {
        let page = self.answers.borrow().get(&which).cloned();
        if let Some(page) = page {
            self.render(&page);
        }
    }

    /// Save on a writable tab: the tab is read again first, as the official
    /// app reads the OSD again before each OSD write, and the changed
    /// settings go out built from that read (see `set_page`), one write (or
    /// the picture's pair) after the other. A change made elsewhere since
    /// the tab was shown is thus kept, not written back.
    fn apply(&self) {
        let Some(which) = self.current_page() else { return };
        if !self.can_write() {
            return;
        }
        {
            let tabs = self.edit_tabs.borrow();
            let Some(tab) = tabs.get(&which) else { return };
            if tab.busy() || tab.edits.is_empty() {
                return;
            }
        }
        let status = if let Some(read) = self.read_page(which) {
            if let Some(tab) = self.edit_tabs.borrow_mut().get_mut(&which) {
                tab.refreshing = true;
                tab.awaited = Some(read);
            }
            if let Some(view) = self.view(which) {
                view.set_controls_sensitive(false);
            }
            "Reading the camera's current values…"
        } else {
            NOT_CONNECTED
        };
        if let Some(tab) = self.edit_tabs.borrow_mut().get_mut(&which) {
            tab.status = status.to_string();
        }
        self.update_footer();
    }

    /// Sends the first group of a save built from the read before it; the
    /// others follow as each is answered (see `written`).
    fn send_save(&self, which: CameraPage, groups: Vec<settings_edit::WriteGroup>) {
        let Some(channel) = self.channel.get() else { return };
        let target = SettingsTarget::Camera { channel, page: which };
        let status = match SaveRun::start(target, groups) {
            None => "Nothing changed.",
            Some((mut run, writes)) => {
                let group = self.next_group();
                if self.send_group(which, group, writes) {
                    run.sent_as(group);
                    if let Some(tab) = self.edit_tabs.borrow_mut().get_mut(&which) {
                        tab.run = Some(run);
                    }
                    "Saving…"
                } else {
                    NOT_CONNECTED
                }
            }
        };
        if let Some(tab) = self.edit_tabs.borrow_mut().get_mut(&which) {
            tab.status = status.to_string();
        }
    }

    fn next_group(&self) -> u64 {
        let group = self.last_group.get().wrapping_add(1);
        self.last_group.set(group);
        group
    }

    /// Sends one group of writes of `which` to the window's device and
    /// channel; `false` when it could not go out.
    fn send_group(&self, which: CameraPage, group: u64, writes: Vec<SettingsWrite>) -> bool {
        let (Some(key), Some(channel)) = (self.key.borrow().clone(), self.channel.get()) else { return false };
        self.can_write() && (self.handlers.on_write)(&key, channel, which, group, writes)
    }

    /// The device's answers to one group of a save; ignored unless they are
    /// for the group of this channel's save that is out.
    pub fn written(&self, target: SettingsTarget, group: u64, results: &[WriteOutcome]) {
        let SettingsTarget::Camera { channel, page: which } = target else { return };
        if self.channel.get() != Some(channel) {
            return;
        }
        let step = {
            let mut tabs = self.edit_tabs.borrow_mut();
            let Some(run) = tabs.get_mut(&which).and_then(|t| t.run.as_mut()) else { return };
            if run.target != target {
                return;
            }
            let Some(step) = run.answered(group, results) else { return };
            step
        };
        match step {
            SaveStep::Send(writes) => {
                let next = self.next_group();
                if let Some(run) = self.edit_tabs.borrow_mut().get_mut(&which).and_then(|t| t.run.as_mut()) {
                    run.sent_as(next);
                }
                if !self.send_group(which, next, writes) {
                    let saved = self.edit_tabs.borrow().get(&which).and_then(|t| t.run.as_ref()).map(SaveRun::saved);
                    self.save_ended(which, saved.unwrap_or_default(), NOT_CONNECTED.to_string());
                }
            }
            SaveStep::Done { saved } => self.save_ended(which, saved, "Saved (the camera answered 200).".to_string()),
            SaveStep::Refused { code, msg_id, saved } => {
                let status = settings_edit::refusal(code, msg_id, &saved);
                self.save_ended(which, saved, status);
            }
            SaveStep::NoAnswer { saved } => {
                self.save_ended(which, saved, "The camera did not answer; Reload to see what it has.".to_string());
            }
        }
    }

    /// A save is over: the saved edits go, the rest stay for another try,
    /// and the tab is read again (the next write is built from that read).
    fn save_ended(&self, which: CameraPage, saved: Vec<Edit>, status: String) {
        {
            let mut tabs = self.edit_tabs.borrow_mut();
            let tab = tabs.entry(which).or_default();
            tab.run = None;
            let fields: Vec<_> = saved.iter().map(|e| e.field()).collect();
            tab.edits.remove(&fields);
            tab.check = saved;
            tab.status = status;
            tab.rereading = true;
        }
        let asked = self.read_page(which);
        if let Some(tab) = self.edit_tabs.borrow_mut().get_mut(&which) {
            match asked {
                Some(read) => tab.awaited = Some(read),
                None => {
                    tab.rereading = false;
                    tab.check.clear();
                }
            }
        }
        self.update_footer();
    }

    /// Revert: the unsaved edits of the tab on show go.
    fn revert(&self) {
        let Some(which) = self.current_page() else { return };
        {
            let mut tabs = self.edit_tabs.borrow_mut();
            let Some(tab) = tabs.get_mut(&which) else { return };
            if tab.saving() {
                return;
            }
            tab.edits.clear();
            tab.status.clear();
        }
        self.redraw(which);
        self.update_footer();
    }

    /// Reload on a settings tab: read again, dropping unsaved edits. The
    /// controls wait for that read: a write is built from the newest one.
    fn reload_page(&self, which: CameraPage) {
        if let Some(tab) = self.edit_tabs.borrow_mut().get_mut(&which) {
            if tab.saving() {
                return;
            }
            tab.status = if tab.edits.is_empty() { String::new() } else { UNSAVED_DROPPED.to_string() };
            tab.edits.clear();
            tab.check.clear();
        }
        if let Some(read) = self.read_page(which).filter(|_| settings_edit::editable_page(which)) {
            {
                let mut tabs = self.edit_tabs.borrow_mut();
                let tab = tabs.entry(which).or_default();
                tab.rereading = true;
                tab.awaited = Some(read);
            }
            if let Some(view) = self.view(which) {
                view.set_controls_sensitive(false);
            }
        }
        self.update_footer();
    }

    /// The footer for the tab on show: Encoding's own Save, or Save and
    /// Revert on a tab with controls or with edits (kept, though a failed
    /// read shows no control for them), and the tab's line.
    fn update_footer(&self) {
        let current = self.current_page();
        let Some(which) = current else {
            self.save.set_visible(true);
            self.apply.set_visible(false);
            self.revert.set_visible(false);
            self.reload.set_sensitive(true);
            self.status.set_text(&self.encoding_status.borrow());
            return;
        };
        self.save.set_visible(false);
        let has_controls = self.view(which).is_some_and(ReadOnlyPage::has_controls);
        // Read out first: no borrow is held while the widgets change.
        let (dirty, running, busy, status) = {
            let tabs = self.edit_tabs.borrow();
            let tab = tabs.get(&which);
            (
                tab.is_some_and(|t| !t.edits.is_empty()),
                tab.is_some_and(EditTab::saving),
                tab.is_some_and(EditTab::busy),
                tab.map_or(String::new(), |t| t.status.clone()),
            )
        };
        self.apply.set_visible(has_controls || dirty);
        self.revert.set_visible(has_controls || dirty);
        self.apply.set_sensitive(dirty && !busy && self.can_write());
        self.revert.set_sensitive(dirty && !running);
        self.reload.set_sensitive(!running);
        self.status.set_text(&status);
    }

    fn view(&self, page: CameraPage) -> Option<&ReadOnlyPage> {
        self.pages.iter().find(|(p, _)| *p == page).map(|(_, v)| v)
    }

    /// The read-only tab on show, or `None` for Encoding.
    fn current_page(&self) -> Option<CameraPage> {
        self.stack.visible_child_name().and_then(|name| page_of(&name))
    }

    fn tab_changed(&self) {
        let current = self.current_page();
        self.update_footer();
        if let Some(page) = current {
            if !self.loaded.borrow().contains(&page) {
                self.read_page(page);
            }
        }
    }

    /// Asks the device for one settings tab; the number the read went
    /// under, or `None` when it could not.
    fn read_page(&self, page: CameraPage) -> Option<u64> {
        let channel = self.channel.get()?;
        let view = self.view(page)?;
        if !self.watched.get() {
            view.set_status(LIVE_STOPPED);
            return None;
        }
        let read = self.last_read.get().wrapping_add(1);
        self.last_read.set(read);
        if !(self.handlers.on_read_page)(channel, page, read) {
            // Not marked loaded: the tab asks again the next time it is shown.
            view.set_status(NOT_CONNECTED);
            return None;
        }
        self.loaded.borrow_mut().insert(page);
        view.set_status(READING);
        Some(read)
    }

    /// The footer line, which belongs to the Encoding tab.
    fn show_status(&self, text: &str) {
        *self.encoding_status.borrow_mut() = text.to_string();
        if self.current_page().is_none() {
            self.status.set_text(text);
        }
    }

    /// Whether the window was last opened for this device's (by key)
    /// `channel`, on screen or not (a save goes on while it is closed, as
    /// long as the channel's live video plays).
    pub fn is_for(&self, key: &str, channel: u8) -> bool {
        self.key.borrow().as_deref() == Some(key) && self.channel.get() == Some(channel)
    }

    /// Whether a tab's answer from this device (or the notice that it waits
    /// for a download) is the window's: it is on screen for that channel, or
    /// it was closed while that tab's save (the read before it, its writes,
    /// the read after it) or re-read is under way. A closed window takes no
    /// other.
    pub fn takes(&self, key: &str, target: SettingsTarget) -> bool {
        let SettingsTarget::Camera { channel, page } = target else { return false };
        self.is_for(key, channel)
            && (self.window.is_visible() || self.edit_tabs.borrow().get(&page).is_some_and(EditTab::busy))
    }

    pub fn is_open_for(&self, channel: u8) -> bool {
        self.window.is_visible() && self.channel.get() == Some(channel)
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    /// The watched channel's connection is gone: nothing can be read or
    /// saved until it is watched again (the window is then opened anew).
    pub fn lost(&self, reason: &str) {
        self.show_status(&format!("Not connected: {reason}"));
        self.save.set_sensitive(false);
        self.loaded.borrow_mut().clear();
        self.stop_writes(&format!("Not connected: {reason}"));
        if let Some(view) = self.current_page().and_then(|page| self.view(page)) {
            view.set_status(NOT_CONNECTED);
        }
    }

    /// No write can go out any more: a save under way is given up (what
    /// the device did is seen the next time the window reads the tab), and
    /// every control is turned off.
    fn stop_writes(&self, reason: &str) {
        self.online.set(false);
        self.give_up_saves(reason);
        // Not while `edit_tabs` is borrowed: a spin button losing the focus
        // here reports the number typed into it (see `edited`).
        for (_, view) in &self.pages {
            view.set_controls_sensitive(false);
        }
        self.update_footer();
    }

    /// Every save and re-read under way is given up, its tab saying
    /// `reason`; the tabs that had one.
    fn give_up_saves(&self, reason: &str) -> Vec<CameraPage> {
        let mut given_up = Vec::new();
        for (page, tab) in self.edit_tabs.borrow_mut().iter_mut() {
            if tab.give_up(reason) {
                given_up.push(*page);
            }
        }
        given_up
    }

    /// The watched channel's live video was stopped (the Stop button, the
    /// Playback page) while the window was closed: a save of the device's
    /// (by key) under way is given up, as `live_stopped` does on screen. Its
    /// answers would no longer be taken, so it would wait for good.
    pub fn stopped_while_closed(&self, key: &str) {
        self.give_up_closed(key, STOPPED_DURING_SAVE);
    }

    /// The watched channel's connection is gone while the window was
    /// closed: a save of the device's (by key) under way is given up, as
    /// `lost` does on screen. Nothing answers it any more.
    pub fn lost_while_closed(&self, key: &str) {
        self.give_up_closed(key, LOST_DURING_SAVE);
    }

    /// A closed window's save for device `key` is given up, each such tab
    /// saying `note` once the window is opened again for its channel (see
    /// `EditTab::reopened`); nothing is drawn now.
    fn give_up_closed(&self, key: &str, note: &'static str) {
        if self.window.is_visible() || self.key.borrow().as_deref() != Some(key) {
            return;
        }
        for tab in self.edit_tabs.borrow_mut().values_mut() {
            tab.give_up_closed(note);
        }
    }

    /// The device (by key) logs in again, with a new login: its old
    /// connection goes without a word, so what a save of the window had out
    /// on it never answers. That save is given up; the window stays usable,
    /// its reads and writes going over the new connection.
    pub fn reconnecting(&self, key: &str) {
        if self.key.borrow().as_deref() != Some(key) {
            return;
        }
        for page in self.give_up_saves(RELOGIN_DROPPED) {
            self.redraw(page);
        }
        self.update_footer();
    }

    /// The device (by key) was removed: nothing is saved to it any more.
    pub fn removed(&self, key: &str) {
        if self.key.borrow().as_deref() == Some(key) {
            self.stop_writes("Not connected: the device was removed");
        }
    }

    /// The channel's live video was stopped (the Stop button, the Playback
    /// page): the device stays connected, but the window reads and saves
    /// nothing more until the channel is watched again, when it is opened
    /// anew. The tabs say so, not "Not connected".
    pub fn live_stopped(&self) {
        self.watched.set(false);
        self.show_status(LIVE_STOPPED);
        self.save.set_sensitive(false);
        self.loaded.borrow_mut().clear();
        self.stop_writes(LIVE_STOPPED);
        if let Some(view) = self.current_page().and_then(|page| self.view(page)) {
            view.set_status(LIVE_STOPPED);
        }
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
        // Which writes are offered depends on the model: the tabs read so
        // far are drawn again once it is known.
        let camera = CapturedCamera::from_model(model);
        if self.camera.replace(camera) != camera {
            let answers: Vec<SettingsPage> = self.answers.borrow().values().cloned().collect();
            for page in answers {
                self.render(&page);
            }
            self.update_footer();
        }
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
            self.show_status("The camera did not give its encoding settings.");
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
            self.show_status("");
            self.save.set_sensitive(true);
        } else {
            self.show_status("The camera did not say which values it allows, so its settings are shown but cannot be changed here.");
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
            self.show_status("Saved. The live picture restarts with the new settings.");
            self.reload_quietly();
        } else {
            self.show_status(&format!("The camera refused the change (code {code})."));
            self.save.set_sensitive(true);
        }
    }

    pub fn failed(&self, reason: &str) {
        self.show_status(reason);
        self.save.set_sensitive(true);
    }

    fn reload(&self) {
        let Some(channel) = self.channel.get() else { return };
        if !self.watched.get() {
            self.show_status(LIVE_STOPPED);
            return;
        }
        self.show_status("Reading the camera's settings…");
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
            self.show_status("Nothing changed.");
            return;
        }
        self.save.set_sensitive(false);
        self.show_status("Saving…");
        (self.handlers.on_save)(channel, body);
    }
}

fn describe(s: &StreamEncoding) -> String {
    format!("{}×{}, {} fps, {} kbit/s, {}", s.width, s.height, s.frame_rate, s.bitrate, codec_name(s.codec))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reoling::{CameraInfoPage, ReadFailure};

    #[test]
    fn only_the_awaited_read_ends_a_tabs_wait() {
        // Save pressed while an older read of the tab was still out (Reload
        // pressed twice): that read's page, complete before the Save's read
        // went, does not stand in for it.
        let mut tab = EditTab { refreshing: true, awaited: Some(8), ..EditTab::default() };
        assert!(!tab.ends_wait(7));
        assert!(tab.refreshing && tab.busy());
        assert_eq!(tab.awaited, Some(8));
        assert!(tab.ends_wait(8));
        assert!(tab.refreshing, "the Save's read: `set_page` builds the writes from it");
        assert_eq!(tab.awaited, None);
        // The read after a save: an older page neither ends it nor is
        // checked against what the save wrote.
        let check = vec![Edit::DateShown(false)];
        let mut tab = EditTab { rereading: true, awaited: Some(12), check: check.clone(), ..EditTab::default() };
        assert!(!tab.ends_wait(11));
        assert!(tab.rereading && tab.busy());
        assert_eq!(tab.check, check);
        assert!(tab.ends_wait(12));
        assert!(!tab.rereading && !tab.busy());
        // A tab that waits for no read takes any page, as before.
        let mut tab = EditTab::default();
        assert!(tab.ends_wait(3));
        assert!(!tab.busy());
    }

    #[test]
    fn a_save_given_up_with_the_window_closed_says_so_when_it_opens() {
        // Two edits whose save has its first group out.
        let page = SettingsPage::CameraInfo { channel: 1, page: CameraInfoPage { version: Err(ReadFailure::NoReply) } };
        let mut edits = Edits::default();
        edits.set(Edit::DateShown(false), &page);
        edits.set(Edit::Watermark(true), &page);
        let groups = vec![
            settings_edit::WriteGroup { writes: Vec::new(), edits: vec![Edit::DateShown(false)] },
            settings_edit::WriteGroup { writes: Vec::new(), edits: vec![Edit::Watermark(true)] },
        ];
        let target = SettingsTarget::Camera { channel: 1, page: CameraPage::Display };
        let (mut run, _) = SaveRun::start(target, groups).expect("a first group");
        run.sent_as(5);
        // Stop or the Playback page with the window closed: the save is
        // given up, and the tab says why when the window is opened again.
        let mut tab = EditTab { edits: edits.clone(), run: Some(run.clone()), ..EditTab::default() };
        assert_eq!(tab.reopened(), Some(SAVE_DROPPED));
        tab.give_up_closed(STOPPED_DURING_SAVE);
        assert!(tab.run.is_none() && !tab.busy());
        assert_eq!(tab.reopened(), Some(STOPPED_DURING_SAVE));
        // The connection lost after that: nothing is left to give up, the
        // first reason stays.
        tab.give_up_closed(LOST_DURING_SAVE);
        assert_eq!(tab.reopened(), Some(STOPPED_DURING_SAVE));
        // The read a Save makes first counts as the save too.
        let mut tab = EditTab { edits: edits.clone(), refreshing: true, awaited: Some(4), ..EditTab::default() };
        tab.give_up_closed(LOST_DURING_SAVE);
        assert!(!tab.busy());
        assert_eq!(tab.awaited, None);
        assert_eq!(tab.reopened(), Some(LOST_DURING_SAVE));
        // The read after a save that was over is only given up: the window
        // opened again says what it said before.
        let check = vec![Edit::DateShown(false)];
        let mut tab =
            EditTab { edits: edits.clone(), rereading: true, awaited: Some(9), check, ..EditTab::default() };
        tab.give_up_closed(STOPPED_DURING_SAVE);
        assert!(!tab.busy() && tab.check.is_empty());
        assert_eq!(tab.reopened(), Some(UNSAVED_DROPPED));
        // With nothing under way the reopened tab says what it said before.
        let mut tab = EditTab { edits: edits.clone(), ..EditTab::default() };
        tab.give_up_closed(STOPPED_DURING_SAVE);
        assert_eq!(tab.reopened(), Some(UNSAVED_DROPPED));
        let mut tab = EditTab::default();
        tab.give_up_closed(STOPPED_DURING_SAVE);
        assert_eq!(tab.reopened(), None);
        // On screen (`lost`, `live_stopped`) the tab says why at once, and
        // opening the window again drops its edits as before.
        let mut tab = EditTab { edits, run: Some(run), ..EditTab::default() };
        assert!(tab.give_up("Not connected: placeholder"));
        assert_eq!(tab.status, "Not connected: placeholder");
        assert!(!tab.busy());
        assert_eq!(tab.reopened(), Some(UNSAVED_DROPPED));
    }
}
