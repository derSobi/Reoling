//! The device list: one card per device with its connection status. A device
//! with several channels (NVR / Home Hub) unfolds to list them by name; a
//! click on a channel, or on the card of a single camera, plays it. Colours
//! are whatever the active theme gives the standard `success`/`warning`/
//! `error`/`dim-label` classes.

use crate::ui::device_store::Device;
use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, Frame, GestureClick, Label, ListBox, ListBoxRow, MenuButton,
    Orientation, Popover, Revealer, ScrolledWindow, SelectionMode, ToggleButton,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum Status {
    Connecting,
    Connected,
    /// Could not connect (or the connection dropped); the reason is the
    /// tooltip.
    Failed(String),
    /// No usable password.
    LoginNeeded(String),
}

type ChannelHandler = Box<dyn Fn(&str, u8)>;

pub struct Handlers {
    pub on_add: Box<dyn Fn()>,
    /// Click on the card itself.
    pub on_select: Box<dyn Fn(&str)>,
    /// Click on one of its channels.
    pub on_channel: ChannelHandler,
    pub on_relogin: Box<dyn Fn(&str)>,
    pub on_remove: Box<dyn Fn(&str)>,
}

struct Card {
    row: ListBoxRow,
    name: Label,
    dot: Label,
    status: Label,
    unfold: ToggleButton,
    channel_list: GtkBox,
    /// The channel buttons, in list order, with their channel numbers.
    channel_buttons: RefCell<Vec<(u8, ToggleButton)>>,
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
        this
    }

    pub fn widget(&self) -> &GtkBox {
        &self.root
    }

    pub fn add_device(self: &Rc<Self>, device: &Device) {
        let row = ListBoxRow::new();
        row.set_activatable(false);

        let card = GtkBox::new(Orientation::Vertical, 4);
        card.set_margin_top(6);
        card.set_margin_bottom(6);
        card.set_margin_start(8);
        card.set_margin_end(8);

        // Header and status: a click anywhere on them selects the device.
        let head = GtkBox::new(Orientation::Vertical, 4);
        let title_row = GtkBox::new(Orientation::Horizontal, 4);
        let unfold = ToggleButton::new();
        unfold.set_icon_name("pan-end-symbolic");
        unfold.add_css_class("flat");
        unfold.set_tooltip_text(Some("Channels"));
        unfold.set_visible(false);
        title_row.append(&unfold);
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
        head.append(&title_row);

        let status_row = GtkBox::new(Orientation::Horizontal, 6);
        let dot = Label::new(Some("●"));
        let status = Label::new(None);
        status.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        status.set_halign(gtk4::Align::Start);
        status_row.append(&dot);
        status_row.append(&status);
        head.append(&status_row);
        card.append(&head);

        let channel_list = GtkBox::new(Orientation::Vertical, 2);
        let revealer = Revealer::new();
        revealer.set_child(Some(&channel_list));
        card.append(&revealer);

        let frame = Frame::new(None);
        frame.set_child(Some(&card));
        frame.set_margin_top(4);
        frame.set_margin_bottom(4);
        row.set_child(Some(&frame));
        self.list.append(&row);

        let key = device.key.clone();
        let weak = Rc::downgrade(self);
        let click = GestureClick::new();
        click.connect_released(move |_, _, _, _| {
            if let Some(s) = weak.upgrade() {
                (s.handlers.on_select)(&key);
            }
        });
        head.add_controller(click);

        let revealer_for_unfold = revealer.clone();
        unfold.connect_toggled(move |t| {
            revealer_for_unfold.set_reveal_child(t.is_active());
            t.set_icon_name(if t.is_active() { "pan-down-symbolic" } else { "pan-end-symbolic" });
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

        self.cards.borrow_mut().insert(
            device.key.clone(),
            Card {
                row,
                name,
                dot,
                status,
                unfold,
                channel_list,
                channel_buttons: RefCell::new(Vec::new()),
            },
        );
        self.set_status(&device.key, &Status::Failed("Not connected yet".to_string()));
    }

    pub fn remove_device(&self, key: &str) {
        if let Some(card) = self.cards.borrow_mut().remove(key) {
            self.list.remove(&card.row);
        }
    }

    /// Highlights the device's card (the one being watched).
    pub fn select(&self, key: &str) {
        if let Some(card) = self.cards.borrow().get(key) {
            self.list.select_row(Some(&card.row));
        }
    }

    pub fn set_name(&self, key: &str, name: &str) {
        if let Some(card) = self.cards.borrow().get(key) {
            card.name.set_text(name);
        }
    }

    pub fn set_status(&self, key: &str, status: &Status) {
        let cards = self.cards.borrow();
        let Some(card) = cards.get(key) else { return };
        let (class, text, tooltip) = match status {
            Status::Connecting => ("warning", "Connecting…", None),
            Status::Connected => ("success", "Connected", None),
            Status::Failed(reason) => ("error", "Not connected", Some(reason.as_str())),
            Status::LoginNeeded(reason) => ("warning", "Login required", Some(reason.as_str())),
        };
        for c in STATUS_CLASSES {
            card.dot.remove_css_class(c);
            card.status.remove_css_class(c);
        }
        card.dot.add_css_class(class);
        card.status.add_css_class(class);
        card.status.set_text(text);
        card.status.set_tooltip_text(tooltip);
    }

    /// Lists the device's connected channels by name under its card. Without
    /// any, the card stays a plain single-camera card.
    pub fn set_channels(
        self: &Rc<Self>,
        key: &str,
        channels: &[reoling::ChannelInfo],
        current: u8,
    ) {
        let cards = self.cards.borrow();
        let Some(card) = cards.get(key) else { return };
        let online: Vec<&reoling::ChannelInfo> = channels.iter().filter(|c| c.online).collect();
        while let Some(child) = card.channel_list.first_child() {
            card.channel_list.remove(&child);
        }
        let mut buttons = Vec::new();
        for channel in &online {
            let label = if channel.name.is_empty() {
                format!("Channel {}", u16::from(channel.channel_id) + 1)
            } else {
                channel.name.clone()
            };
            let button = ToggleButton::new();
            let text = Label::new(Some(&label));
            text.set_halign(gtk4::Align::Start);
            text.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            button.set_child(Some(&text));
            button.add_css_class("flat");
            button.set_active(channel.channel_id == current);
            let (id, k) = (channel.channel_id, key.to_string());
            let weak = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                if let Some(s) = weak.upgrade() {
                    (s.handlers.on_channel)(&k, id);
                }
            });
            card.channel_list.append(&button);
            buttons.push((channel.channel_id, button));
        }
        card.unfold.set_visible(!online.is_empty());
        if !online.is_empty() && !card.unfold.is_active() {
            card.unfold.set_active(true);
        }
        *card.channel_buttons.borrow_mut() = buttons;
    }

    /// Marks the channel being watched.
    pub fn mark_channel(&self, key: &str, channel: u8) {
        if let Some(card) = self.cards.borrow().get(key) {
            for (id, button) in card.channel_buttons.borrow().iter() {
                button.set_active(*id == channel);
            }
        }
    }
}
