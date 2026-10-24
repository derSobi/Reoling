//! The settings window of an NVR or Home Hub itself (not of one of its
//! cameras): what the device tells about itself, its storage, network,
//! clock, lights and sounds, alert switches, accounts and scheduled reboot,
//! all read-only, one tab each, read when the tab is first shown and again
//! on Reload.

use crate::ui::product_image::{self, Category};
use crate::ui::settings_pages::{self, Context};
use crate::ui::settings_view::{ReadOnlyPage, NOT_CONNECTED, READING, WAITING_FOR_DOWNLOAD};
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, Image, Label, Orientation, Stack, StackSwitcher, Window};
use reoling::{ChannelInfo, DeviceKind, DevicePage, SettingsPage, SettingsTarget};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub struct Handlers {
    /// Ask the device (by its key) for one tab; `false` when there is no
    /// connection to ask over.
    pub on_read_page: Box<dyn Fn(&str, DevicePage) -> bool>,
}

fn page_name(page: DevicePage) -> &'static str {
    match page {
        DevicePage::Info => "info",
        DevicePage::Storage => "storage",
        DevicePage::Network => "network",
        DevicePage::Time => "time",
        DevicePage::LightSound => "light-sound",
        DevicePage::Alerts => "alerts",
        DevicePage::Users => "users",
        DevicePage::Maintenance => "maintenance",
    }
}

fn page_title(page: DevicePage) -> &'static str {
    match page {
        DevicePage::Info => "Info",
        DevicePage::Storage => "Storage",
        DevicePage::Network => "Network",
        DevicePage::Time => "Time",
        DevicePage::LightSound => "Light & sound",
        DevicePage::Alerts => "Alerts",
        DevicePage::Users => "Users",
        DevicePage::Maintenance => "Maintenance",
    }
}

fn page_of(name: &str) -> Option<DevicePage> {
    DevicePage::ALL.into_iter().find(|p| page_name(*p) == name)
}

/// Which tabs a kind of device has: a Hub has its own lights and sounds, an
/// NVR device-wide alert switches.
pub fn has_tab(kind: DeviceKind, page: DevicePage) -> bool {
    match page {
        DevicePage::LightSound => kind == DeviceKind::Hub,
        DevicePage::Alerts => kind == DeviceKind::Nvr,
        _ => true,
    }
}

pub struct DeviceSettings {
    window: Window,
    picture: Image,
    title: Label,
    model: Label,
    firmware: Label,
    status: Label,
    reload: Button,
    stack: Stack,
    pages: Vec<(DevicePage, ReadOnlyPage)>,
    handlers: Handlers,
    /// The device shown (its key) and the masked UID for Info.
    key: RefCell<Option<String>>,
    uid_hint: RefCell<Option<String>>,
    /// The device's channels, by name, for the Alerts tab.
    channels: RefCell<Vec<ChannelInfo>>,
    /// Tabs read at least once since the window was opened.
    loaded: RefCell<HashSet<DevicePage>>,
    /// The last answer per tab, shown again when the channel list changes.
    answers: RefCell<HashMap<DevicePage, SettingsPage>>,
}

impl DeviceSettings {
    pub fn new(handlers: Handlers) -> Rc<Self> {
        let window = Window::builder().title("Device settings").default_width(720).default_height(640).build();
        window.connect_close_request(|w| {
            w.set_visible(false);
            gtk4::glib::Propagation::Stop
        });

        let root = GtkBox::new(Orientation::Vertical, 16);
        root.set_margin_top(18);
        root.set_margin_bottom(18);
        root.set_margin_start(18);
        root.set_margin_end(18);

        // Which device this is.
        let header = GtkBox::new(Orientation::Horizontal, 16);
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

        let stack = Stack::new();
        stack.set_vexpand(true);
        stack.set_transition_type(gtk4::StackTransitionType::None);
        let switcher = StackSwitcher::new();
        switcher.set_stack(Some(&stack));
        switcher.set_halign(Align::Center);
        root.append(&switcher);
        root.append(&stack);

        let mut pages = Vec::new();
        for page in DevicePage::ALL {
            let view = ReadOnlyPage::new();
            stack.add_titled(view.widget(), Some(page_name(page)), page_title(page));
            pages.push((page, view));
        }

        let footer = GtkBox::new(Orientation::Horizontal, 8);
        let status = Label::builder().xalign(0.0).hexpand(true).wrap(true).build();
        let reload = Button::with_label("Reload");
        footer.append(&status);
        footer.append(&reload);
        root.append(&footer);
        window.set_child(Some(&root));

        let this = Rc::new(Self {
            window,
            picture,
            title,
            model,
            firmware,
            status,
            reload: reload.clone(),
            stack: stack.clone(),
            pages,
            handlers,
            key: RefCell::new(None),
            uid_hint: RefCell::new(None),
            channels: RefCell::new(Vec::new()),
            loaded: RefCell::new(HashSet::new()),
            answers: RefCell::new(HashMap::new()),
        });

        let weak = Rc::downgrade(&this);
        reload.connect_clicked(move |_| {
            if let Some(s) = weak.upgrade() {
                if let Some(page) = s.current_page() {
                    s.read_page(page);
                }
            }
        });
        // A tab is read the first time it is shown.
        let weak = Rc::downgrade(&this);
        stack.connect_visible_child_name_notify(move |_| {
            if let Some(s) = weak.upgrade() {
                s.tab_changed();
            }
        });
        this
    }

    /// Opens the window for a device, on its Info tab. `uid_hint` is the
    /// masked UID the Info tab shows for a device added by UID.
    pub fn open(&self, key: &str, name: &str, model: &str, kind: DeviceKind, uid_hint: Option<&str>) {
        *self.key.borrow_mut() = Some(key.to_string());
        *self.uid_hint.borrow_mut() = uid_hint.map(str::to_string);
        self.title.set_text(if name.is_empty() { "Device" } else { name });
        self.model.set_text(if model.is_empty() { "Model unknown" } else { model });
        self.firmware.set_text("");
        product_image::show(&self.picture, model, Category::from_model(model));
        self.loaded.borrow_mut().clear();
        self.answers.borrow_mut().clear();
        for (_, view) in &self.pages {
            view.clear();
        }
        self.status.set_text("");
        self.reload.set_sensitive(true);
        // Info first, then the tabs this kind of device lacks are hidden:
        // hiding the tab on show would make the stack pick another one,
        // which would then be read for nothing.
        self.stack.set_visible_child_name(page_name(DevicePage::Info));
        for (page, view) in &self.pages {
            self.stack.page(view.widget()).set_visible(has_tab(kind, *page));
        }
        self.window.present();
        gtk4::prelude::GtkWindowExt::set_focus(&self.window, Some(&self.reload));
        // Already on Info when the window last closed: no change notice.
        self.tab_changed();
    }

    pub fn is_open_for(&self, key: &str) -> bool {
        self.window.is_visible() && self.key.borrow().as_deref() == Some(key)
    }

    /// One tab's answer; ignored unless it is a device page.
    pub fn set_page(&self, page: &SettingsPage) {
        let SettingsTarget::Device { page: which } = page.target() else { return };
        if let SettingsPage::DeviceInfo(info) = page {
            if let Ok(v) = &info.version {
                if let Some(firmware) = v.firmware_version.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
                    self.firmware.set_text(firmware);
                }
                if let Some(model) = v.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
                    self.model.set_text(model);
                }
            }
        }
        self.answers.borrow_mut().insert(which, page.clone());
        self.render(page);
    }

    /// A download holds the connection; the tab is read once it is over.
    pub fn page_deferred(&self, target: &SettingsTarget) {
        let SettingsTarget::Device { page } = target else { return };
        if let Some(view) = self.view(*page) {
            view.set_status(WAITING_FOR_DOWNLOAD);
        }
    }

    /// The device's channels, for the Alerts tab's per-channel lines. The
    /// device re-sends its list from time to time: the tab is only rebuilt
    /// when a channel's number, name or presence changed.
    pub fn set_channels(&self, channels: &[ChannelInfo]) {
        let same = |a: &ChannelInfo, b: &ChannelInfo| a.channel_id == b.channel_id && a.name == b.name && a.online == b.online;
        let unchanged = {
            let old = self.channels.borrow();
            old.len() == channels.len() && old.iter().zip(channels).all(|(a, b)| same(a, b))
        };
        if unchanged {
            return;
        }
        *self.channels.borrow_mut() = channels.to_vec();
        let alerts = self.answers.borrow().get(&DevicePage::Alerts).cloned();
        if let Some(page) = alerts {
            self.render(&page);
        }
    }

    /// The device disconnected: nothing more can be read until it is back.
    pub fn lost(&self, reason: &str) {
        self.status.set_text(&format!("Not connected: {reason}"));
        self.reload.set_sensitive(false);
        self.loaded.borrow_mut().clear();
    }

    /// The device is back: the tabs can be read again, the one on show now.
    pub fn reconnected(&self) {
        self.status.set_text("");
        self.reload.set_sensitive(true);
        self.tab_changed();
    }

    fn render(&self, page: &SettingsPage) {
        let SettingsTarget::Device { page: which } = page.target() else { return };
        let Some(view) = self.view(which) else { return };
        let channels = self.channels.borrow();
        let uid_hint = self.uid_hint.borrow().clone();
        let ctx = Context { abilities: None, channels: &channels, uid_hint, local_now: local_now() };
        view.show(&settings_pages::build(page, &ctx));
    }

    fn view(&self, page: DevicePage) -> Option<&ReadOnlyPage> {
        self.pages.iter().find(|(p, _)| *p == page).map(|(_, v)| v)
    }

    fn current_page(&self) -> Option<DevicePage> {
        self.stack.visible_child_name().and_then(|name| page_of(&name))
    }

    fn tab_changed(&self) {
        let Some(page) = self.current_page() else { return };
        if !self.loaded.borrow().contains(&page) && self.reload.is_sensitive() {
            self.read_page(page);
        }
    }

    fn read_page(&self, page: DevicePage) {
        let Some(key) = self.key.borrow().clone() else { return };
        let Some(view) = self.view(page) else { return };
        if !(self.handlers.on_read_page)(&key, page) {
            // Not marked loaded: the tab asks again the next time it is shown.
            view.set_status(NOT_CONNECTED);
            return;
        }
        self.loaded.borrow_mut().insert(page);
        view.set_status(READING);
    }
}

/// The local time now as civil seconds (Unix time plus the local UTC
/// offset), for the dates the Users tab computes from a duration.
fn local_now() -> Option<i64> {
    let now = gtk4::glib::DateTime::now_local().ok()?;
    Some(now.to_unix() + now.utc_offset().as_seconds())
}
