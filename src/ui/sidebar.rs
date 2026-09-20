//! The device list: one card per device with its connection status and the
//! channel to watch. Colours are whatever the active theme gives the standard
//! `success`/`warning`/`error`/`dim-label` classes.

use crate::ui::device_store::Device;
use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, DropDown, Frame, Label, ListBox, ListBoxRow, MenuButton, Orientation, Popover,
    ScrolledWindow, SelectionMode, SpinButton,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum Status {
    Idle,
    Connecting,
    Connected,
    Failed(String),
}

type ChannelHandler = Box<dyn Fn(&str, u8)>;

pub struct Handlers {
    pub on_add: Box<dyn Fn()>,
    pub on_select: Box<dyn Fn(&str)>,
    pub on_channel: ChannelHandler,
    pub on_relogin: Box<dyn Fn(&str)>,
    pub on_remove: Box<dyn Fn(&str)>,
}

struct Card {
    row: ListBoxRow,
    name: Label,
    dot: Label,
    status: Label,
    spin: SpinButton,
    /// Shown instead of `spin` once the device has listed its channels.
    channels: DropDown,
    channel_ids: Rc<RefCell<Vec<u8>>>,
    updating: Rc<Cell<bool>>,
}

pub struct Sidebar {
    root: GtkBox,
    list: ListBox,
    cards: RefCell<HashMap<String, Card>>,
    handlers: Handlers,
}

const STATUS_CLASSES: [&str; 4] = ["dim-label", "success", "warning", "error"];

impl Sidebar {
    pub fn new(handlers: Handlers) -> Rc<Self> {
        let root = GtkBox::new(Orientation::Vertical, 8);
        root.set_size_request(260, -1);

        let head = GtkBox::new(Orientation::Horizontal, 8);
        head.set_margin_top(8);
        head.set_margin_start(12);
        head.set_margin_end(8);
        let title = Label::new(Some("Devices"));
        title.add_css_class("title-4");
        title.set_hexpand(true);
        title.set_halign(gtk4::Align::Start);
        let add = Button::from_icon_name("list-add-symbolic");
        add.set_tooltip_text(Some("Add device"));
        add.add_css_class("flat");
        head.append(&title);
        head.append(&add);
        root.append(&head);

        let list = ListBox::new();
        list.set_selection_mode(SelectionMode::Single);
        list.add_css_class("navigation-sidebar");
        let scroller = ScrolledWindow::builder().vexpand(true).child(&list).build();
        scroller.set_hscrollbar_policy(gtk4::PolicyType::Never);
        root.append(&scroller);

        let this = Rc::new(Self { root, list, cards: RefCell::new(HashMap::new()), handlers });

        let weak = Rc::downgrade(&this);
        add.connect_clicked(move |_| {
            if let Some(s) = weak.upgrade() {
                (s.handlers.on_add)();
            }
        });
        let weak = Rc::downgrade(&this);
        this.list.connect_row_activated(move |_, row| {
            if let Some(s) = weak.upgrade() {
                (s.handlers.on_select)(&row.widget_name());
            }
        });
        this
    }

    pub fn widget(&self) -> &GtkBox {
        &self.root
    }

    pub fn add_device(self: &Rc<Self>, device: &Device) {
        let row = ListBoxRow::new();
        row.set_widget_name(&device.key);

        let card = GtkBox::new(Orientation::Vertical, 6);
        card.set_margin_top(6);
        card.set_margin_bottom(6);
        card.set_margin_start(8);
        card.set_margin_end(8);

        let title_row = GtkBox::new(Orientation::Horizontal, 4);
        let name = Label::new(Some(&device.name));
        name.add_css_class("heading");
        name.set_halign(gtk4::Align::Start);
        name.set_hexpand(true);
        name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        title_row.append(&name);

        let menu = MenuButton::new();
        menu.set_icon_name("emblem-system-symbolic");
        menu.add_css_class("flat");
        let popover = Popover::new();
        let menu_box = GtkBox::new(Orientation::Vertical, 2);
        let relogin = Button::with_label("Change login…");
        let remove = Button::with_label("Remove device");
        relogin.add_css_class("flat");
        remove.add_css_class("flat");
        menu_box.append(&relogin);
        menu_box.append(&remove);
        popover.set_child(Some(&menu_box));
        menu.set_popover(Some(&popover));
        title_row.append(&menu);
        card.append(&title_row);

        let status_row = GtkBox::new(Orientation::Horizontal, 6);
        let dot = Label::new(Some("●"));
        let status = Label::new(None);
        status.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        status.set_halign(gtk4::Align::Start);
        status_row.append(&dot);
        status_row.append(&status);
        card.append(&status_row);

        let channel_row = GtkBox::new(Orientation::Horizontal, 6);
        let channel_label = Label::new(Some("Channel"));
        channel_label.add_css_class("dim-label");
        channel_label.set_hexpand(true);
        channel_label.set_halign(gtk4::Align::Start);
        let spin = SpinButton::with_range(0.0, 255.0, 1.0);
        spin.set_value(f64::from(device.channel));
        let channels = DropDown::new(None::<gtk4::StringList>, None::<gtk4::Expression>);
        channels.set_visible(false);
        channel_row.append(&channel_label);
        channel_row.append(&spin);
        channel_row.append(&channels);
        card.append(&channel_row);

        let frame = Frame::new(None);
        frame.set_child(Some(&card));
        frame.set_margin_top(4);
        frame.set_margin_bottom(4);
        row.set_child(Some(&frame));
        row.set_selectable(true);
        self.list.append(&row);

        let key = device.key.clone();
        let weak = Rc::downgrade(self);
        spin.connect_value_changed(move |spin| {
            if let Some(s) = weak.upgrade() {
                (s.handlers.on_channel)(&key, spin.value() as u8);
            }
        });
        let channel_ids: Rc<RefCell<Vec<u8>>> = Rc::default();
        let updating: Rc<Cell<bool>> = Rc::default();
        let key = device.key.clone();
        let weak = Rc::downgrade(self);
        let (ids, guard) = (Rc::clone(&channel_ids), Rc::clone(&updating));
        channels.connect_selected_notify(move |dd| {
            if guard.get() {
                return;
            }
            let channel = ids.borrow().get(dd.selected() as usize).copied();
            if let (Some(channel), Some(s)) = (channel, weak.upgrade()) {
                (s.handlers.on_channel)(&key, channel);
            }
        });
        let key = device.key.clone();
        let weak = Rc::downgrade(self);
        let pop = popover.clone();
        relogin.connect_clicked(move |_| {
            pop.popdown();
            if let Some(s) = weak.upgrade() {
                (s.handlers.on_relogin)(&key);
            }
        });
        let key = device.key.clone();
        let weak = Rc::downgrade(self);
        remove.connect_clicked(move |_| {
            if let Some(s) = weak.upgrade() {
                (s.handlers.on_remove)(&key);
            }
        });

        self.cards.borrow_mut().insert(device.key.clone(), Card { row, name, dot, status, spin, channels, channel_ids, updating });
        self.set_status(&device.key, &Status::Idle);
    }

    pub fn remove_device(&self, key: &str) {
        if let Some(card) = self.cards.borrow_mut().remove(key) {
            self.list.remove(&card.row);
        }
    }

    pub fn set_name(&self, key: &str, name: &str) {
        if let Some(card) = self.cards.borrow().get(key) {
            card.name.set_text(name);
        }
    }

    pub fn select(&self, key: &str) {
        if let Some(card) = self.cards.borrow().get(key) {
            self.list.select_row(Some(&card.row));
        }
    }

    pub fn set_channel(&self, key: &str, channel: u8) {
        if let Some(card) = self.cards.borrow().get(key) {
            if card.channels.is_visible() {
                let index = card.channel_ids.borrow().iter().position(|c| *c == channel);
                if let Some(index) = index {
                    card.channels.set_selected(index as u32);
                }
            } else {
                card.spin.set_value(f64::from(channel));
            }
        }
    }

    /// Lists the device's connected channels by name instead of the numeric
    /// selector. Without any, the numeric selector stays.
    pub fn set_channels(&self, key: &str, channels: &[reoling::ChannelInfo], current: u8) {
        let cards = self.cards.borrow();
        let Some(card) = cards.get(key) else { return };
        let online: Vec<&reoling::ChannelInfo> = channels.iter().filter(|c| c.online).collect();
        if online.is_empty() {
            return;
        }
        let names: Vec<String> = online
            .iter()
            .map(|c| {
                if c.name.is_empty() {
                    format!("Channel {}", u16::from(c.channel_id) + 1)
                } else {
                    c.name.clone()
                }
            })
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        card.updating.set(true);
        *card.channel_ids.borrow_mut() = online.iter().map(|c| c.channel_id).collect();
        card.channels.set_model(Some(&gtk4::StringList::new(&names)));
        let index = online.iter().position(|c| c.channel_id == current).unwrap_or(0);
        card.channels.set_selected(index as u32);
        card.updating.set(false);
        card.spin.set_visible(false);
        card.channels.set_visible(true);
    }

    pub fn set_status(&self, key: &str, status: &Status) {
        let cards = self.cards.borrow();
        let Some(card) = cards.get(key) else { return };
        let (class, text) = match status {
            Status::Idle => ("dim-label", "Not connected".to_string()),
            Status::Connecting => ("warning", "Connecting…".to_string()),
            Status::Connected => ("success", "Connected".to_string()),
            Status::Failed(reason) => ("error", reason.clone()),
        };
        for c in STATUS_CLASSES {
            card.dot.remove_css_class(c);
            card.status.remove_css_class(c);
        }
        card.dot.add_css_class(class);
        card.status.add_css_class(class);
        card.status.set_text(&text);
        card.status.set_tooltip_text(Some(&text));
    }
}
