//! The camera's remote control: a small window of its own (independent of the
//! main window) with the direction pad and the zoom. It acts on whatever
//! channel the main window is showing.

use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, GestureClick, Grid, Label, Orientation, PropagationPhase, Scale, Window,
};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

/// How long the zoom slider must rest before its position is sent, so a
/// drag does not flood the camera.
const ZOOM_SETTLE: Duration = Duration::from_millis(120);

pub struct Handlers {
    /// A direction is held down: `left`, `right`, `up` or `down`.
    pub on_move: Box<dyn Fn(&'static str)>,
    /// The direction was let go.
    pub on_stop: Box<dyn Fn()>,
    pub on_zoom: Box<dyn Fn(u32)>,
}

pub struct RemoteControl {
    window: Window,
    target: Label,
    pad: Grid,
    zoom_box: GtkBox,
    zoom: Scale,
    /// Set while the code (not the user) moves the slider.
    updating: Cell<bool>,
    zoom_generation: Cell<u64>,
    handlers: Handlers,
}

impl RemoteControl {
    pub fn new(handlers: Handlers) -> Rc<Self> {
        let window = Window::builder().title("Camera control").resizable(false).build();
        // Closing the remote only hides it: it comes back with the same
        // place and state.
        window.connect_close_request(|w| {
            w.set_visible(false);
            glib::Propagation::Stop
        });

        let content = GtkBox::new(Orientation::Vertical, 12);
        for set in [
            GtkBox::set_margin_top,
            GtkBox::set_margin_bottom,
            GtkBox::set_margin_start,
            GtkBox::set_margin_end,
        ] {
            set(&content, 12);
        }
        let target = Label::new(Some("No camera"));
        target.add_css_class("heading");
        content.append(&target);

        let pad = Grid::builder().row_spacing(6).column_spacing(6).halign(gtk4::Align::Center).build();
        let zoom = Scale::with_range(Orientation::Horizontal, 0.0, 1.0, 1.0);
        zoom.set_draw_value(false);
        zoom.set_hexpand(true);
        let zoom_box = GtkBox::new(Orientation::Vertical, 4);
        let zoom_label = Label::new(Some("Zoom"));
        zoom_label.set_halign(gtk4::Align::Start);
        zoom_box.append(&zoom_label);
        zoom_box.append(&zoom);
        content.append(&pad);
        content.append(&zoom_box);
        window.set_child(Some(&content));

        let this = Rc::new(Self {
            window,
            target,
            pad,
            zoom_box,
            zoom,
            updating: Cell::new(false),
            zoom_generation: Cell::new(0),
            handlers,
        });

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
            let weak = Rc::downgrade(&this);
            hold.connect_pressed(move |_, _, _, _| {
                if let Some(r) = weak.upgrade() {
                    (r.handlers.on_move)(command);
                }
            });
            let weak = Rc::downgrade(&this);
            hold.connect_released(move |_, _, _, _| {
                if let Some(r) = weak.upgrade() {
                    (r.handlers.on_stop)();
                }
            });
            let weak = Rc::downgrade(&this);
            hold.connect_stopped(move |_| {
                if let Some(r) = weak.upgrade() {
                    (r.handlers.on_stop)();
                }
            });
            button.add_controller(hold);
            this.pad.attach(&button, col, row, 1, 1);
        }

        let weak = Rc::downgrade(&this);
        this.zoom.connect_value_changed(move |scale| {
            let Some(r) = weak.upgrade() else { return };
            if r.updating.get() {
                return;
            }
            // Send the position once the slider has rested a moment.
            let generation = r.zoom_generation.get() + 1;
            r.zoom_generation.set(generation);
            let position = scale.value() as u32;
            let weak = Rc::downgrade(&r);
            glib::timeout_add_local_once(ZOOM_SETTLE, move || {
                if let Some(r) = weak.upgrade() {
                    if r.zoom_generation.get() == generation {
                        (r.handlers.on_zoom)(position);
                    }
                }
            });
        });
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
    pub fn set_enabled(&self, move_pad: bool, zoom: bool) {
        self.pad.set_sensitive(move_pad);
        self.zoom_box.set_sensitive(zoom);
    }

    /// The zoom range and where it stands, from the camera.
    pub fn set_zoom_range(&self, min: u32, max: u32, current: u32) {
        if max <= min {
            return;
        }
        self.updating.set(true);
        self.zoom.set_range(f64::from(min), f64::from(max));
        self.zoom.set_value(f64::from(current.clamp(min, max)));
        self.updating.set(false);
    }
}
