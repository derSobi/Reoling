//! The camera's remote control: a small window of its own (independent of the
//! main window) with the direction pad, zoom and focus, plus three pages —
//! Calibration (a direct action), Monitor Point and Preset Points — reached
//! through their own buttons and left with "back", the same drill-down
//! structure the official app uses (`.plans/official_app_screenshots/PTZ-1`)
//! instead of showing everything inlined at once, which made this window far
//! taller than it needed to be. It acts on whatever channel the main window
//! is showing.

use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, Entry, GestureClick, Grid, Image as ImageWidget, Label, ListBox,
    ListBoxRow, Orientation, PropagationPhase, Scale, Spinner, Stack, StackTransitionType,
    Switch, Window,
};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

/// How long a slider must rest before its position is sent, so a drag does
/// not flood the camera.
const SETTLE: Duration = Duration::from_millis(120);

/// Decodes a JPEG/PNG and scales it down to fit within `max_width` ×
/// `max_height`, preserving aspect ratio and never upscaling. Returns the
/// scaled pixbuf and its actual pixel size, or `None` if the bytes don't
/// decode.
fn decode_and_scale(bytes: &[u8], max_width: i32, max_height: i32) -> Option<(gtk4::gdk_pixbuf::Pixbuf, i32, i32)> {
    use gtk4::gdk_pixbuf::{prelude::*, InterpType, PixbufLoader};

    let loader = PixbufLoader::new();
    let pixbuf = loader
        .write(bytes)
        .and_then(|()| loader.close())
        .ok()
        .and_then(|()| loader.pixbuf())?;

    let (width, height) = (pixbuf.width(), pixbuf.height());
    let scale = (f64::from(max_width) / f64::from(width.max(1)))
        .min(f64::from(max_height) / f64::from(height.max(1)))
        .min(1.0);
    let target_width = ((f64::from(width) * scale) as i32).max(1);
    let target_height = ((f64::from(height) * scale) as i32).max(1);
    let thumbnail = pixbuf
        .scale_simple(target_width, target_height, InterpType::Bilinear)
        .unwrap_or(pixbuf);
    Some((thumbnail, target_width, target_height))
}

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
    /// The saved thumbnail was asked for again (opening the page, or a click
    /// on it — the official app treats its thumbnail as its own refresh
    /// button, and a preset saved without one only gets it after a refresh).
    pub on_refresh_monitor_point_image: Box<dyn Fn()>,
    /// A preset's thumbnail was asked for again (a click on it, same as
    /// Monitor Point's own thumbnail) — never fetched automatically for
    /// every preset at once: the camera's own image-file transfer only
    /// tolerates one at a time, and a burst of concurrent requests (once
    /// tried here) got the camera to drop the connection.
    pub on_refresh_preset_image: Box<dyn Fn(u8)>,
    /// The Preset Points page opened, or its "refresh all" was pressed —
    /// the official app re-reads every thumbnail from the camera each time.
    pub on_refresh_all_presets: Box<dyn Fn()>,
    pub on_rename_preset: Box<dyn Fn(u8, String)>,
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

/// A sub-page's header: "← Title", matching the official app's own
/// drill-down panels.
fn page_header(stack: &Stack, title: &str) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 6);
    let back = Button::from_icon_name("go-previous-symbolic");
    back.add_css_class("flat");
    back.set_tooltip_text(Some("Back"));
    let stack = stack.clone();
    back.connect_clicked(move |_| stack.set_visible_child_name("main"));
    let heading = Label::new(Some(title));
    heading.add_css_class("heading");
    heading.set_hexpand(true);
    heading.set_halign(gtk4::Align::Start);
    row.append(&back);
    row.append(&heading);
    row
}

pub struct RemoteControl {
    window: Window,
    stack: Stack,
    pad: Grid,
    zoom: Rc<Adjuster>,
    focus: Rc<Adjuster>,
    calibrate: Button,
    calibrating: Cell<bool>,
    calibration_generation: Cell<u64>,
    /// What `set_enabled` last said, kept apart from `calibrating` so
    /// clearing the calibration-busy state can restore exactly that rather
    /// than guessing from the buttons' current (already-overwritten)
    /// sensitivity.
    calibration_capable: Cell<bool>,
    monitor_point_capable: Cell<bool>,
    presets_capable: Cell<bool>,
    calibration_spinner: Spinner,
    calibration_status: Label,
    open_monitor_point: Button,
    open_presets: Button,
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
    preset_list: std::cell::RefCell<Vec<reoling::PtzPreset>>,
    /// The pictures received this session (not saved anywhere: the official
    /// app re-reads them on every opening too), so a switch between the
    /// list and thumbnail views can rebuild the rows.
    preset_jpegs: std::cell::RefCell<std::collections::HashMap<u8, Vec<u8>>>,
    thumbnail_mode: Cell<bool>,
    mode_button: Button,
    /// Each listed preset's thumbnail widget, by preset id, so a later
    /// `set_preset_image` can update one in place without rebuilding the
    /// whole list.
    preset_images: std::cell::RefCell<std::collections::HashMap<u8, ImageWidget>>,
    /// The open "Adjust Preset Point" dialog's picture, with its preset id.
    edit_image: std::cell::RefCell<Option<(u8, ImageWidget)>>,
    handlers: Rc<Handlers>,
}

impl RemoteControl {
    pub fn new(handlers: Handlers) -> Rc<Self> {
        let handlers = Rc::new(handlers);
        let window = Window::builder().title("Camera control").resizable(false).default_width(250).build();
        // Closing the remote only hides it: it comes back where it was.
        window.connect_close_request(|w| {
            w.set_visible(false);
            glib::Propagation::Stop
        });

        let stack = Stack::new();
        // Each page keeps its own natural size instead of the window sizing
        // to whichever page is largest — the same page-to-page size change
        // the official app's own panel has.
        stack.set_hhomogeneous(false);
        stack.set_vhomogeneous(false);
        stack.set_transition_type(StackTransitionType::SlideLeftRight);
        window.set_child(Some(&stack));

        // --- Main page: target camera, pad, zoom, focus, and the three
        // buttons that open the pages below. ---
        let main_page = GtkBox::new(Orientation::Vertical, 12);
        main_page.set_margin_top(12);
        main_page.set_margin_bottom(12);
        main_page.set_margin_start(12);
        main_page.set_margin_end(12);

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
        main_page.append(&pad);

        let h = Rc::clone(&handlers);
        let zoom = Adjuster::new("Zoom", move |p| (h.on_zoom)(p));
        let h = Rc::clone(&handlers);
        let focus = Adjuster::new("Focus", move |p| (h.on_focus)(p));
        main_page.append(&zoom.row);
        main_page.append(&focus.row);

        // Calibration: a direct action (not a page) — while it runs, the
        // official app disables the whole panel and shows a spinner with an
        // explanatory message, since the camera gives no separate "finished"
        // signal (only that the request was accepted); `set_calibrating`
        // reproduces that, cleared once `main_window` sees the reply.
        let calibrate = Button::with_label("Calibration");
        let calibration_spinner = Spinner::new();
        calibration_spinner.set_visible(false);
        let calibration_status = Label::new(None);
        calibration_status.set_wrap(true);
        calibration_status.set_max_width_chars(24);
        calibration_status.set_justify(gtk4::Justification::Center);
        calibration_status.set_visible(false);
        calibration_status.add_css_class("dim-label");
        let calibration_row = GtkBox::new(Orientation::Horizontal, 6);
        calibration_row.set_halign(gtk4::Align::Center);
        calibration_row.append(&calibration_spinner);
        calibration_row.append(&calibration_status);
        main_page.append(&calibration_row);
        main_page.append(&calibrate);

        let open_monitor_point = Button::with_label("Monitor Point");
        let open_presets = Button::with_label("Preset");
        for button in [&open_monitor_point, &open_presets] {
            button.set_halign(gtk4::Align::Fill);
        }
        main_page.append(&open_monitor_point);
        main_page.append(&open_presets);

        stack.add_named(&main_page, Some("main"));

        // --- Monitor Point page. ---
        let monitor_page = GtkBox::new(Orientation::Vertical, 10);
        monitor_page.set_margin_top(12);
        monitor_page.set_margin_bottom(12);
        monitor_page.set_margin_start(12);
        monitor_page.set_margin_end(12);
        monitor_page.append(&page_header(&stack, "Monitor Point"));
        let hint = Label::new(Some(
            "The device will auto return to the initial monitoring position after deviating.",
        ));
        hint.add_css_class("dim-label");
        hint.set_wrap(true);
        hint.set_max_width_chars(24);
        hint.set_halign(gtk4::Align::Start);
        monitor_page.append(&hint);

        // `Image`, not `Picture`: it sizes itself to the pixbuf's own pixel
        // dimensions instead of stretching to fill the available width, so
        // the thumbnail this project pre-scales in `set_monitor_point_image`
        // actually stays small. Wrapped in a flat button: clicking it, like
        // the official app's own thumbnail, asks the camera for it again —
        // a preset or point saved without a picture only gets one this way.
        let monitor_image = ImageWidget::new();
        monitor_image.set_halign(gtk4::Align::Start);
        monitor_image.set_visible(false);
        let refresh_image = Button::new();
        refresh_image.add_css_class("flat");
        refresh_image.set_halign(gtk4::Align::Start);
        refresh_image.set_tooltip_text(Some("Refresh the picture"));
        refresh_image.set_child(Some(&monitor_image));
        let h = Rc::clone(&handlers);
        refresh_image.connect_clicked(move |_| (h.on_refresh_monitor_point_image)());
        monitor_page.append(&refresh_image);

        let monitor_status = Label::new(Some("Not read yet"));
        monitor_status.add_css_class("dim-label");
        monitor_status.set_halign(gtk4::Align::Start);
        monitor_page.append(&monitor_status);

        let go_to_monitor = Button::with_label("Return to Monitor Point");
        monitor_page.append(&go_to_monitor);

        let enable_row = GtkBox::new(Orientation::Horizontal, 6);
        let enable_label = Label::new(Some("Auto"));
        enable_label.set_hexpand(true);
        enable_label.set_halign(gtk4::Align::Start);
        let monitor_enabled = Switch::new();
        monitor_enabled.set_valign(gtk4::Align::Center);
        enable_row.append(&enable_label);
        enable_row.append(&monitor_enabled);
        monitor_page.append(&enable_row);

        // Shared with the switch, the timeout slider, and the two buttons
        // above/below: the last-known Monitor Point state, so any one of
        // them can send the OTHER's current value along with its own change
        // (see `Handlers::on_monitor_point_config`'s doc comment).
        let monitor_state: Rc<Cell<reoling::MonitorPoint>> = Rc::default();
        let updating_monitor_enabled: Rc<Cell<bool>> = Rc::default();

        let (h, state) = (Rc::clone(&handlers), Rc::clone(&monitor_state));
        let monitor_timeout = Adjuster::with_fixed_step("Interval (seconds)", 1.0, move |timeout| {
            state.set(reoling::MonitorPoint { timeout_seconds: timeout, ..state.get() });
            (h.on_monitor_point_config)(state.get().enabled, timeout);
        });
        monitor_timeout.set_range(10, 300, 60);
        monitor_page.append(&monitor_timeout.row);

        let (h, state, updating) = (Rc::clone(&handlers), Rc::clone(&monitor_state), Rc::clone(&updating_monitor_enabled));
        monitor_enabled.connect_state_set(move |_, enabled| {
            if !updating.get() {
                state.set(reoling::MonitorPoint { enabled, ..state.get() });
                (h.on_monitor_point_config)(enabled, state.get().timeout_seconds);
            }
            glib::Propagation::Proceed
        });

        let (h, state) = (Rc::clone(&handlers), Rc::clone(&monitor_state));
        go_to_monitor.connect_clicked(move |_| (h.on_go_to_monitor_point)(state.get().timeout_seconds));

        // Separated from the settings above, like the official app's own
        // page, since it is a destructive-ish action (moves the saved
        // point), not a settings change.
        let reset_monitor = Button::with_label("Reset Monitor Point");
        reset_monitor.set_margin_top(12);
        reset_monitor.set_tooltip_text(Some("Save the camera's current position as Monitor Point"));
        let (h, state) = (Rc::clone(&handlers), Rc::clone(&monitor_state));
        reset_monitor.connect_clicked(move |_| (h.on_reset_monitor_point)(state.get().enabled, state.get().timeout_seconds));
        monitor_page.append(&reset_monitor);

        stack.add_named(&monitor_page, Some("monitor_point"));

        // --- Preset Points page. ---
        let preset_page = GtkBox::new(Orientation::Vertical, 10);
        preset_page.set_margin_top(12);
        preset_page.set_margin_bottom(12);
        preset_page.set_margin_start(12);
        preset_page.set_margin_end(12);
        preset_page.append(&page_header(&stack, "Preset Points"));
        // View toggle, refresh all, add — the official app's own three.
        let mode_button = Button::from_icon_name("view-list-symbolic");
        mode_button.add_css_class("flat");
        mode_button.set_tooltip_text(Some("Switch between thumbnails and a list"));
        let refresh_all = Button::from_icon_name("view-refresh-symbolic");
        refresh_all.add_css_class("flat");
        refresh_all.set_tooltip_text(Some("Refresh all pictures"));
        let add = Button::from_icon_name("list-add-symbolic");
        add.add_css_class("flat");
        add.set_tooltip_text(Some("Save the current position as a preset"));
        let toolbar = GtkBox::new(Orientation::Horizontal, 2);
        toolbar.set_halign(gtk4::Align::End);
        toolbar.append(&mode_button);
        toolbar.append(&refresh_all);
        toolbar.append(&add);
        preset_page.append(&toolbar);
        let presets = ListBox::new();
        presets.add_css_class("boxed-list");
        presets.set_selection_mode(gtk4::SelectionMode::None);
        preset_page.append(&presets);
        stack.add_named(&preset_page, Some("presets"));

        stack.set_visible_child_name("main");

        let this = Rc::new(Self {
            window,
            stack,
            pad,
            zoom,
            focus,
            calibrate,
            calibrating: Cell::new(false),
            calibration_generation: Cell::new(0),
            calibration_capable: Cell::new(false),
            monitor_point_capable: Cell::new(false),
            presets_capable: Cell::new(false),
            calibration_spinner,
            calibration_status,
            open_monitor_point,
            open_presets,
            monitor_image,
            monitor_enabled,
            monitor_timeout,
            monitor_status,
            monitor_state,
            updating_monitor_enabled,
            presets,
            preset_list: std::cell::RefCell::new(Vec::new()),
            preset_jpegs: std::cell::RefCell::new(std::collections::HashMap::new()),
            thumbnail_mode: Cell::new(true),
            mode_button,
            preset_images: std::cell::RefCell::new(std::collections::HashMap::new()),
            edit_image: std::cell::RefCell::new(None),
            handlers,
        });

        let weak = Rc::downgrade(&this);
        this.open_monitor_point.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.stack.set_visible_child_name("monitor_point");
                (r.handlers.on_refresh_monitor_point_image)();
            }
        });
        let weak = Rc::downgrade(&this);
        this.open_presets.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.stack.set_visible_child_name("presets");
                (r.handlers.on_refresh_all_presets)();
            }
        });
        let weak = Rc::downgrade(&this);
        this.calibrate.connect_clicked(move |_| {
            let Some(r) = weak.upgrade() else { return };
            (r.handlers.on_calibrate)();
            r.set_calibrating(true);
            // Safety net: if the reply never arrives, the panel would
            // otherwise stay disabled forever.
            let generation = r.calibration_generation.get() + 1;
            r.calibration_generation.set(generation);
            let weak = Rc::downgrade(&r);
            glib::timeout_add_local_once(Duration::from_secs(20), move || {
                if let Some(r) = weak.upgrade() {
                    if r.calibration_generation.get() == generation {
                        r.set_calibrating(false);
                    }
                }
            });
        });

        let weak = Rc::downgrade(&this);
        this.mode_button.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.thumbnail_mode.set(!r.thumbnail_mode.get());
                r.rebuild_presets();
            }
        });
        let weak = Rc::downgrade(&this);
        refresh_all.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                (r.handlers.on_refresh_all_presets)();
            }
        });
        let weak = Rc::downgrade(&this);
        add.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                r.open_edit_dialog(None);
            }
        });
        this
    }

    /// Shows the window (or brings it forward), always starting from the
    /// main page.
    pub fn present(&self) {
        self.stack.set_visible_child_name("main");
        self.window.present();
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    /// Names the camera being controlled — the window's own title, so it is
    /// still visible whichever page is showing.
    pub fn set_target(&self, name: &str) {
        self.window.set_title(Some(name));
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
        self.calibration_capable.set(calibration);
        self.monitor_point_capable.set(monitor_point);
        // Presets, like the pad, move the camera — the same requirement.
        self.presets_capable.set(move_pad);
        if !self.calibrating.get() {
            self.calibrate.set_sensitive(calibration);
            self.open_monitor_point.set_sensitive(monitor_point);
            self.open_presets.set_sensitive(move_pad);
        }
        if !monitor_point {
            self.monitor_status.set_text("This camera has no Monitor Point");
            self.clear_monitor_point_image();
        } else if self.monitor_status.text() == "This camera has no Monitor Point" {
            self.monitor_status.set_text("Not read yet");
        }
    }

    /// Disables the whole panel with a spinner while calibration runs, or
    /// clears that back to normal — see the comment on the Calibration
    /// button's construction for why there is no real "finished" signal to
    /// wait for instead.
    pub fn set_calibrating(&self, busy: bool) {
        self.calibrating.set(busy);
        self.pad.set_sensitive(!busy);
        self.zoom.row.set_sensitive(!busy);
        self.focus.row.set_sensitive(!busy);
        self.calibrate.set_sensitive(!busy && self.calibration_capable.get());
        self.open_monitor_point.set_sensitive(!busy && self.monitor_point_capable.get());
        self.open_presets.set_sensitive(!busy && self.presets_capable.get());
        self.calibration_spinner.set_visible(busy);
        self.calibration_spinner.set_spinning(busy);
        self.calibration_status.set_visible(busy);
        if busy {
            self.calibration_status
                .set_text("Calibrating… PTZ is unavailable until this finishes.");
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
        const MAX_WIDTH: i32 = 220;
        const MAX_HEIGHT: i32 = 130;

        let Some((thumbnail, target_width, target_height)) = decode_and_scale(jpeg, MAX_WIDTH, MAX_HEIGHT) else {
            eprintln!("could not decode Monitor Point's thumbnail");
            return;
        };

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

    /// A single preset's saved thumbnail, once its download has finished.
    /// Kept for the session so the view can be rebuilt; a no-op for a
    /// preset no longer listed.
    pub fn set_preset_image(&self, preset_id: u8, jpeg: &[u8]) {
        if !self.preset_list.borrow().iter().any(|p| p.id == preset_id) {
            return;
        }
        self.preset_jpegs.borrow_mut().insert(preset_id, jpeg.to_vec());
        if let Some(image) = self.preset_images.borrow().get(&preset_id) {
            Self::show_thumbnail(image, jpeg, 150, 84);
        }
        if let Some((id, image)) = self.edit_image.borrow().as_ref() {
            if *id == preset_id {
                Self::show_thumbnail(image, jpeg, 210, 118);
            }
        }
    }

    fn show_thumbnail(image: &ImageWidget, jpeg: &[u8], max_width: i32, max_height: i32) {
        let Some((thumbnail, w, h)) = decode_and_scale(jpeg, max_width, max_height) else {
            eprintln!("could not decode a preset thumbnail");
            return;
        };
        image.set_pixel_size(w.max(h));
        image.set_from_pixbuf(Some(&thumbnail));
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
        *self.preset_list.borrow_mut() = presets.to_vec();
        self.preset_jpegs.borrow_mut().retain(|id, _| presets.iter().any(|p| p.id == *id));
        self.rebuild_presets();
    }

    fn rebuild_presets(self: &Rc<Self>) {
        while let Some(child) = self.presets.first_child() {
            self.presets.remove(&child);
        }
        self.preset_images.borrow_mut().clear();
        let thumbnails = self.thumbnail_mode.get();
        self.mode_button
            .set_icon_name(if thumbnails { "view-list-symbolic" } else { "image-x-generic-symbolic" });
        let presets = self.preset_list.borrow().clone();
        for preset in &presets {
            let row = ListBoxRow::new();
            row.set_selectable(false);
            row.set_activatable(false);
            let line = GtkBox::new(Orientation::Horizontal, 4);
            line.set_margin_top(4);
            line.set_margin_bottom(4);
            line.set_margin_start(6);
            line.set_margin_end(6);

            // The whole card — picture and name included — moves the camera
            // to the preset, like the official app's.
            let name = Label::new(Some(&preset.name));
            name.set_halign(gtk4::Align::Start);
            name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            name.set_max_width_chars(16);
            let card = Button::new();
            card.add_css_class("flat");
            card.set_hexpand(true);
            card.set_tooltip_text(Some("Move to this preset"));
            if thumbnails {
                let image = ImageWidget::from_icon_name("image-x-generic-symbolic");
                image.set_pixel_size(48);
                if let Some(jpeg) = self.preset_jpegs.borrow().get(&preset.id) {
                    Self::show_thumbnail(&image, jpeg, 150, 84);
                }
                let content = GtkBox::new(Orientation::Vertical, 2);
                content.append(&image);
                content.append(&name);
                card.set_child(Some(&content));
                self.preset_images.borrow_mut().insert(preset.id, image);
            } else {
                card.set_child(Some(&name));
            }

            let edit = Button::from_icon_name("document-edit-symbolic");
            edit.set_tooltip_text(Some("Edit"));
            edit.add_css_class("flat");
            let remove = Button::from_icon_name("user-trash-symbolic");
            remove.set_tooltip_text(Some("Delete"));
            remove.add_css_class("flat");
            line.append(&card);
            if thumbnails {
                let side = GtkBox::new(Orientation::Vertical, 2);
                side.set_valign(gtk4::Align::Center);
                side.append(&edit);
                side.append(&remove);
                line.append(&side);
            } else {
                line.append(&edit);
                line.append(&remove);
            }
            row.set_child(Some(&line));
            self.presets.append(&row);

            let (id, weak) = (preset.id, Rc::downgrade(self));
            card.connect_clicked(move |_| {
                if let Some(r) = weak.upgrade() {
                    (r.handlers.on_goto_preset)(id);
                }
            });
            let (edited, weak) = (preset.clone(), Rc::downgrade(self));
            edit.connect_clicked(move |_| {
                if let Some(r) = weak.upgrade() {
                    r.open_edit_dialog(Some(edited.clone()));
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

    /// "Adjust Preset Point" (`Some`) or a new preset (`None`): a name, and
    /// for an existing one its picture, which a click refreshes.
    fn open_edit_dialog(self: &Rc<Self>, preset: Option<reoling::PtzPreset>) {
        let dialog = Window::builder()
            .title(if preset.is_some() { "Adjust Preset Point" } else { "New Preset Point" })
            .transient_for(&self.window)
            .modal(true)
            .resizable(false)
            .default_width(240)
            .build();
        let body = GtkBox::new(Orientation::Vertical, 10);
        body.set_margin_top(12);
        body.set_margin_bottom(12);
        body.set_margin_start(12);
        body.set_margin_end(12);

        let name = Entry::builder().placeholder_text("Name").text(preset.as_ref().map_or("", |p| p.name.as_str())).build();
        if let Some(p) = &preset {
            let image = ImageWidget::from_icon_name("view-refresh-symbolic");
            image.set_pixel_size(48);
            if let Some(jpeg) = self.preset_jpegs.borrow().get(&p.id) {
                Self::show_thumbnail(&image, jpeg, 210, 118);
            }
            let picture = Button::new();
            picture.add_css_class("flat");
            picture.set_tooltip_text(Some("Refresh the picture"));
            picture.set_child(Some(&image));
            let (id, weak) = (p.id, Rc::downgrade(self));
            picture.connect_clicked(move |_| {
                if let Some(r) = weak.upgrade() {
                    (r.handlers.on_refresh_preset_image)(id);
                }
            });
            body.append(&picture);
            *self.edit_image.borrow_mut() = Some((p.id, image));
        }
        body.append(&name);

        let cancel = Button::with_label("Cancel");
        let confirm = Button::with_label("Confirm");
        confirm.add_css_class("suggested-action");
        let buttons = GtkBox::new(Orientation::Horizontal, 8);
        buttons.set_homogeneous(true);
        buttons.append(&cancel);
        buttons.append(&confirm);
        body.append(&buttons);
        dialog.set_child(Some(&body));

        let weak = Rc::downgrade(self);
        dialog.connect_close_request(move |_| {
            if let Some(r) = weak.upgrade() {
                *r.edit_image.borrow_mut() = None;
            }
            glib::Propagation::Proceed
        });
        let d = dialog.clone();
        cancel.connect_clicked(move |_| d.close());
        let (d, weak, name) = (dialog.clone(), Rc::downgrade(self), name.clone());
        confirm.connect_clicked(move |_| {
            if let Some(r) = weak.upgrade() {
                let text = name.text().trim().to_string();
                match &preset {
                    Some(p) if !text.is_empty() && text != p.name => (r.handlers.on_rename_preset)(p.id, text),
                    None if !text.is_empty() => (r.handlers.on_add_preset)(text),
                    _ => {}
                }
            }
            d.close();
        });
        dialog.present();
    }
}
