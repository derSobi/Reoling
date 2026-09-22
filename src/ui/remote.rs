//! The camera's remote control: a small window of its own (independent of the
//! main window) with the direction pad, zoom and focus. It acts on whatever
//! channel the main window is showing.

use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, Entry, GestureClick, Grid, Label, ListBox, ListBoxRow, Orientation,
    PropagationPhase, Scale, Window,
};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

/// How long a slider must rest before its position is sent, so a drag does
/// not flood the camera.
const SETTLE: Duration = Duration::from_millis(120);

pub struct Handlers {
    /// A direction is held down: `left`, `right`, `up` or `down`.
    pub on_move: Box<dyn Fn(&'static str)>,
    /// The direction was let go.
    pub on_stop: Box<dyn Fn()>,
    pub on_zoom: Box<dyn Fn(u32)>,
    pub on_focus: Box<dyn Fn(u32)>,
    pub on_add_preset: Box<dyn Fn(String)>,
    pub on_goto_preset: Box<dyn Fn(u8)>,
    pub on_delete_preset: Box<dyn Fn(u8)>,
}

/// A slider with its name, its value, and − / + around it.
struct Adjuster {
    row: GtkBox,
    scale: Scale,
    value: Label,
    /// Set while the code (not the user) moves the slider.
    updating: Cell<bool>,
    generation: Cell<u64>,
}

impl Adjuster {
    fn new(name: &str, send: impl Fn(u32) + 'static) -> Rc<Self> {
        let scale = Scale::with_range(Orientation::Horizontal, 0.0, 1.0, 1.0);
        scale.set_draw_value(false);
        scale.set_hexpand(true);
        let value = Label::new(Some("–"));
        value.add_css_class("dim-label");
        let label = Label::new(Some(name));
        label.set_hexpand(true);
        label.set_halign(gtk4::Align::Start);
        let title = GtkBox::new(Orientation::Horizontal, 6);
        title.append(&label);
        title.append(&value);
        let less = Button::with_label("−");
        let more = Button::with_label("+");
        less.add_css_class("flat");
        more.add_css_class("flat");
        let controls = GtkBox::new(Orientation::Horizontal, 4);
        controls.append(&less);
        controls.append(&scale);
        controls.append(&more);
        let row = GtkBox::new(Orientation::Vertical, 2);
        row.append(&title);
        row.append(&controls);

        let this = Rc::new(Self {
            row,
            scale,
            value,
            updating: Cell::new(false),
            generation: Cell::new(0),
        });

        for (button, sign) in [(less, -1.0), (more, 1.0)] {
            let weak = Rc::downgrade(&this);
            button.connect_clicked(move |_| {
                if let Some(a) = weak.upgrade() {
                    let adjustment = a.scale.adjustment();
                    let step = ((adjustment.upper() - adjustment.lower()) / 50.0).max(1.0);
                    a.scale.set_value(a.scale.value() + sign * step);
                }
            });
        }

        let send = Rc::new(send);
        let weak = Rc::downgrade(&this);
        this.scale.connect_value_changed(move |scale| {
            let Some(a) = weak.upgrade() else { return };
            let position = scale.value() as u32;
            a.value.set_text(&position.to_string());
            if a.updating.get() {
                return;
            }
            // Send the position once the slider has rested a moment.
            let generation = a.generation.get() + 1;
            a.generation.set(generation);
            let (weak, send) = (Rc::downgrade(&a), Rc::clone(&send));
            glib::timeout_add_local_once(SETTLE, move || {
                if let Some(a) = weak.upgrade() {
                    if a.generation.get() == generation {
                        send(position);
                    }
                }
            });
        });
        this
    }

    fn set_range(&self, min: u32, max: u32, current: u32) {
        if max <= min {
            return;
        }
        let current = current.clamp(min, max);
        self.updating.set(true);
        self.scale.set_range(f64::from(min), f64::from(max));
        self.scale.set_value(f64::from(current));
        self.value.set_text(&current.to_string());
        self.updating.set(false);
    }
}

pub struct RemoteControl {
    window: Window,
    target: Label,
    pad: Grid,
    zoom: Rc<Adjuster>,
    focus: Rc<Adjuster>,
    presets: ListBox,
    preset_name: Entry,
    handlers: Rc<Handlers>,
}

impl RemoteControl {
    pub fn new(handlers: Handlers) -> Rc<Self> {
        let handlers = Rc::new(handlers);
        let window = Window::builder().title("Camera control").resizable(false).build();
        // Closing the remote only hides it: it comes back where it was.
        window.connect_close_request(|w| {
            w.set_visible(false);
            glib::Propagation::Stop
        });

        let content = GtkBox::new(Orientation::Vertical, 12);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);
        let target = Label::new(Some("No camera"));
        target.add_css_class("heading");
        content.append(&target);

        let pad = Grid::builder().row_spacing(6).column_spacing(6).halign(gtk4::Align::Center).build();
        for (row, col, label, command) in [
            (0, 1, "↑", "up"),
            (1, 0, "←", "left"),
            (1, 2, "→", "right"),
            (2, 1, "↓", "down"),
        ] {
            // Moves while held: the camera goes on until told to stop.
            let button = Button::with_label(label);
            button.set_size_request(56, 48);
            let hold = GestureClick::new();
            hold.set_propagation_phase(PropagationPhase::Capture);
            let h = Rc::clone(&handlers);
            hold.connect_pressed(move |_, _, _, _| (h.on_move)(command));
            let h = Rc::clone(&handlers);
            hold.connect_released(move |_, _, _, _| (h.on_stop)());
            let h = Rc::clone(&handlers);
            hold.connect_stopped(move |_| (h.on_stop)());
            button.add_controller(hold);
            pad.attach(&button, col, row, 1, 1);
        }
        content.append(&pad);

        let h = Rc::clone(&handlers);
        let zoom = Adjuster::new("Zoom", move |p| (h.on_zoom)(p));
        let h = Rc::clone(&handlers);
        let focus = Adjuster::new("Focus", move |p| (h.on_focus)(p));
        content.append(&zoom.row);
        content.append(&focus.row);

        // Presets: a scrollable list, each with Go / Delete, and an entry to
        // save the current position as a new one.
        let preset_heading = Label::new(Some("Presets"));
        preset_heading.add_css_class("heading");
        preset_heading.set_halign(gtk4::Align::Start);
        content.append(&preset_heading);
        let presets = ListBox::new();
        presets.add_css_class("boxed-list");
        content.append(&presets);
        let add_row = GtkBox::new(Orientation::Horizontal, 4);
        let preset_name = Entry::builder().placeholder_text("New preset name").hexpand(true).build();
        let add = Button::from_icon_name("list-add-symbolic");
        add.set_tooltip_text(Some("Save the current position as a preset"));
        add_row.append(&preset_name);
        add_row.append(&add);
        content.append(&add_row);
        window.set_child(Some(&content));

        let this = Rc::new(Self { window, target, pad, zoom, focus, presets, preset_name, handlers });

        let weak = Rc::downgrade(&this);
        let commit = move || {
            let Some(r) = weak.upgrade() else { return };
            let name = r.preset_name.text().trim().to_string();
            if name.is_empty() {
                return;
            }
            (r.handlers.on_add_preset)(name);
            r.preset_name.set_text("");
        };
        let c = commit.clone();
        add.connect_clicked(move |_| c());
        this.preset_name.connect_activate(move |_| commit());
        this
    }

    /// Shows the window (or brings it forward).
    pub fn present(&self) {
        self.window.present();
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    /// Names the camera being controlled.
    pub fn set_target(&self, name: &str) {
        self.target.set_text(name);
    }

    /// What the camera can do right now (nothing while there is no stream).
    pub fn set_enabled(&self, move_pad: bool, zoom: bool, focus: bool) {
        self.pad.set_sensitive(move_pad);
        self.zoom.row.set_sensitive(zoom);
        self.focus.row.set_sensitive(focus);
    }

    /// Zoom range and position, from the camera.
    pub fn set_zoom_range(&self, min: u32, max: u32, current: u32) {
        self.zoom.set_range(min, max, current);
    }

    /// Focus range and position, from the camera.
    pub fn set_focus_range(&self, min: u32, max: u32, current: u32) {
        self.focus.set_range(min, max, current);
    }

    /// The camera's saved presets, replacing whatever was listed before.
    pub fn set_presets(self: &Rc<Self>, presets: &[reoling::PtzPreset]) {
        while let Some(child) = self.presets.first_child() {
            self.presets.remove(&child);
        }
        for preset in presets {
            let row = ListBoxRow::new();
            row.set_selectable(false);
            row.set_activatable(false);
            let line = GtkBox::new(Orientation::Horizontal, 6);
            line.set_margin_top(4);
            line.set_margin_bottom(4);
            line.set_margin_start(8);
            line.set_margin_end(8);
            let name = Label::new(Some(&preset.name));
            name.set_hexpand(true);
            name.set_halign(gtk4::Align::Start);
            name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            let go = Button::from_icon_name("go-jump-symbolic");
            go.set_tooltip_text(Some("Move to this preset"));
            go.add_css_class("flat");
            let remove = Button::from_icon_name("user-trash-symbolic");
            remove.set_tooltip_text(Some("Delete"));
            remove.add_css_class("flat");
            line.append(&name);
            line.append(&go);
            line.append(&remove);
            row.set_child(Some(&line));
            self.presets.append(&row);

            let (id, weak) = (preset.id, Rc::downgrade(self));
            go.connect_clicked(move |_| {
                if let Some(r) = weak.upgrade() {
                    (r.handlers.on_goto_preset)(id);
                }
            });
            let (id, weak) = (preset.id, Rc::downgrade(self));
            remove.connect_clicked(move |_| {
                if let Some(r) = weak.upgrade() {
                    (r.handlers.on_delete_preset)(id);
                }
            });
        }
    }
}
