//! The camera's remote control: a small window of its own (independent of the
//! main window) with the direction pad, zoom and focus. It acts on whatever
//! channel the main window is showing.

use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, Entry, GestureClick, Grid, Label, ListBox, ListBoxRow, Orientation,
    Image as ImageWidget, PropagationPhase, Scale, Separator, Switch, Window,
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
    /// Re-calibrates the pan/tilt mechanism.
    pub on_calibrate: Box<dyn Fn()>,
    /// Auto Return's on/off and timeout changed — never the saved position.
    pub on_monitor_point_config: Box<dyn Fn(bool, u32)>,
    /// Saves the camera's current position as Monitor Point, keeping
    /// whatever Auto Return on/off and timeout are showing.
    pub on_reset_monitor_point: Box<dyn Fn(bool, u32)>,
    pub on_go_to_monitor_point: Box<dyn Fn(u32)>,
}

/// A slider with its name, its value, and − / + around it.
struct Adjuster {
    row: GtkBox,
    scale: Scale,
    value: Label,
    /// Set while the code (not the user) moves the slider.
    updating: Cell<bool>,
    generation: Cell<u64>,
    /// The −/+ buttons' step. `None` means one proportional to the current
    /// range (for zoom/focus, whose range can be in the thousands); `Some`
    /// pins it regardless of range (Monitor Point's timeout wants exactly
    /// 1 second, not a fraction of 10..300).
    fixed_step: Option<f64>,
}

impl Adjuster {
    fn new(name: &str, send: impl Fn(u32) + 'static) -> Rc<Self> {
        Self::with_step(name, None, send)
    }

    /// Like `new`, but the −/+ buttons always move by exactly `step`.
    fn with_fixed_step(name: &str, step: f64, send: impl Fn(u32) + 'static) -> Rc<Self> {
        Self::with_step(name, Some(step), send)
    }

    fn with_step(name: &str, fixed_step: Option<f64>, send: impl Fn(u32) + 'static) -> Rc<Self> {
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
            fixed_step,
        });

        for (button, sign) in [(less, -1.0), (more, 1.0)] {
            let weak = Rc::downgrade(&this);
            button.connect_clicked(move |_| {
                if let Some(a) = weak.upgrade() {
                    let adjustment = a.scale.adjustment();
                    let step = a
                        .fixed_step
                        .unwrap_or_else(|| ((adjustment.upper() - adjustment.lower()) / 50.0).max(1.0));
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
    calibrate: Button,
    monitor_point: GtkBox,
    monitor_image: ImageWidget,
    monitor_enabled: Switch,
    monitor_timeout: Rc<Adjuster>,
    monitor_status: Label,
    /// The state a read of Monitor Point last reported, so a plain toggle or
    /// timeout change can be sent together with the other's current value —
    /// see `Handlers::on_monitor_point_config`'s own doc comment.
    monitor_state: Rc<Cell<reoling::MonitorPoint>>,
    /// Set while the code (not the user) moves the switch.
    updating_monitor_enabled: Rc<Cell<bool>>,
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
        content.append(&Separator::new(Orientation::Horizontal));

        let h = Rc::clone(&handlers);
        let calibrate = Button::with_label("Calibration");
        calibrate.connect_clicked(move |_| (h.on_calibrate)());
        content.append(&calibrate);

        // Monitor Point ("PTZ Guard" on the wire): a saved home position,
        // with an optional automatic return after a timeout.
        let monitor_heading = Label::new(Some("Monitor Point"));
        monitor_heading.add_css_class("heading");
        monitor_heading.set_halign(gtk4::Align::Start);
        content.append(&monitor_heading);
        let monitor_point = GtkBox::new(Orientation::Vertical, 8);

        // `Image`, not `Picture`: it sizes itself to the pixbuf's own pixel
        // dimensions instead of stretching to fill the available width, so
        // the thumbnail this project pre-scales in `set_monitor_point_image`
        // actually stays small.
        let monitor_image = ImageWidget::new();
        monitor_image.set_halign(gtk4::Align::Start);
        monitor_image.set_visible(false);
        monitor_point.append(&monitor_image);

        let monitor_status = Label::new(Some("Not read yet"));
        monitor_status.add_css_class("dim-label");
        monitor_status.set_halign(gtk4::Align::Start);
        monitor_point.append(&monitor_status);

        let enable_row = GtkBox::new(Orientation::Horizontal, 6);
        let enable_label = Label::new(Some("Auto Return"));
        enable_label.set_hexpand(true);
        enable_label.set_halign(gtk4::Align::Start);
        let monitor_enabled = Switch::new();
        monitor_enabled.set_valign(gtk4::Align::Center);
        enable_row.append(&enable_label);
        enable_row.append(&monitor_enabled);
        monitor_point.append(&enable_row);

        // Shared with the switch, the timeout slider, and the two buttons
        // below: the last-known Monitor Point state, so any one of them can
        // send the OTHER's current value along with its own change (see
        // `Handlers::on_monitor_point_config`'s doc comment).
        let monitor_state: Rc<Cell<reoling::MonitorPoint>> = Rc::default();
        let updating_monitor_enabled: Rc<Cell<bool>> = Rc::default();

        let (h, state) = (Rc::clone(&handlers), Rc::clone(&monitor_state));
        let monitor_timeout = Adjuster::with_fixed_step("Return after (seconds)", 1.0, move |timeout| {
            state.set(reoling::MonitorPoint { timeout_seconds: timeout, ..state.get() });
            (h.on_monitor_point_config)(state.get().enabled, timeout);
        });
        monitor_timeout.set_range(10, 300, 60);
        monitor_point.append(&monitor_timeout.row);

        let (h, state, updating) = (Rc::clone(&handlers), Rc::clone(&monitor_state), Rc::clone(&updating_monitor_enabled));
        monitor_enabled.connect_state_set(move |_, enabled| {
            if !updating.get() {
                state.set(reoling::MonitorPoint { enabled, ..state.get() });
                (h.on_monitor_point_config)(enabled, state.get().timeout_seconds);
            }
            glib::Propagation::Proceed
        });

        let go_to_monitor = Button::with_label("Return to Monitor Point");
        let (h, state) = (Rc::clone(&handlers), Rc::clone(&monitor_state));
        go_to_monitor.connect_clicked(move |_| (h.on_go_to_monitor_point)(state.get().timeout_seconds));
        monitor_point.append(&go_to_monitor);

        let reset_monitor = Button::with_label("Reset Monitor Point");
        reset_monitor.set_tooltip_text(Some("Save the camera's current position as Monitor Point"));
        let (h, state) = (Rc::clone(&handlers), Rc::clone(&monitor_state));
        reset_monitor.connect_clicked(move |_| (h.on_reset_monitor_point)(state.get().enabled, state.get().timeout_seconds));
        monitor_point.append(&reset_monitor);

        content.append(&monitor_point);
        content.append(&Separator::new(Orientation::Horizontal));

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

        let this = Rc::new(Self {
            window,
            target,
            pad,
            zoom,
            focus,
            calibrate,
            monitor_point,
            monitor_image,
            monitor_enabled,
            monitor_timeout,
            monitor_status,
            monitor_state,
            updating_monitor_enabled,
            presets,
            preset_name,
            handlers,
        });

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
    #[allow(clippy::too_many_arguments)]
    pub fn set_enabled(
        &self,
        move_pad: bool,
        zoom: bool,
        focus: bool,
        calibration: bool,
        monitor_point: bool,
    ) {
        self.pad.set_sensitive(move_pad);
        self.zoom.row.set_sensitive(zoom);
        self.focus.row.set_sensitive(focus);
        self.calibrate.set_sensitive(calibration);
        self.monitor_point.set_sensitive(monitor_point);
        if !monitor_point {
            self.monitor_status.set_text("This camera has no Monitor Point");
            self.clear_monitor_point_image();
        } else if self.monitor_status.text() == "This camera has no Monitor Point" {
            self.monitor_status.set_text("Not read yet");
        }
    }

    /// Monitor Point's state, from the camera.
    pub fn set_monitor_point(&self, state: reoling::MonitorPoint) {
        self.monitor_state.set(state);
        self.updating_monitor_enabled.set(true);
        self.monitor_enabled.set_state(state.enabled);
        self.monitor_enabled.set_active(state.enabled);
        self.updating_monitor_enabled.set(false);
        if state.timeout_seconds > 0 {
            self.monitor_timeout.set_range(10, 300, state.timeout_seconds);
        }
        self.monitor_status.set_text(if state.valid {
            "Monitor Point is set"
        } else {
            "No Monitor Point saved yet — move the camera and press Reset Monitor Point"
        });
        if !state.valid {
            self.clear_monitor_point_image();
        }
    }

    /// Monitor Point's saved thumbnail, once its download has finished.
    /// Scaled down to a small, fixed-bound thumbnail before display — a
    /// `Picture` left to its own natural size would show the source image
    /// at full resolution in this narrow window.
    pub fn set_monitor_point_image(&self, jpeg: &[u8]) {
        use gtk4::gdk_pixbuf::{prelude::*, InterpType, PixbufLoader};

        const MAX_WIDTH: i32 = 220;
        const MAX_HEIGHT: i32 = 130;

        let loader = PixbufLoader::new();
        let pixbuf = loader
            .write(jpeg)
            .and_then(|()| loader.close())
            .ok()
            .and_then(|()| loader.pixbuf());
        let Some(pixbuf) = pixbuf else {
            eprintln!("could not decode Monitor Point's thumbnail");
            return;
        };

        let (width, height) = (pixbuf.width(), pixbuf.height());
        let scale = (f64::from(MAX_WIDTH) / f64::from(width.max(1)))
            .min(f64::from(MAX_HEIGHT) / f64::from(height.max(1)))
            .min(1.0);
        let target_width = ((f64::from(width) * scale) as i32).max(1);
        let target_height = ((f64::from(height) * scale) as i32).max(1);
        let thumbnail = pixbuf
            .scale_simple(target_width, target_height, InterpType::Bilinear)
            .unwrap_or(pixbuf);

        // `Image` renders every pixbuf through its icon-size pipeline in
        // GTK4 — without this, it shrinks the (already pre-scaled)
        // thumbnail down to the theme's normal icon size regardless of its
        // real pixel dimensions.
        self.monitor_image.set_pixel_size(target_width.max(target_height));
        self.monitor_image.set_from_pixbuf(Some(&thumbnail));
        self.monitor_image.set_visible(true);
    }

    pub fn clear_monitor_point_image(&self) {
        self.monitor_image.set_from_pixbuf(None);
        self.monitor_image.set_visible(false);
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
