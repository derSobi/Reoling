//! The application window: a device sidebar next to a live-view page, with a
//! playback page reserved behind the header's view switcher.

use crate::ui::bridge::{spawn_connection, AppEvent, SinkRequest, UidTransport};
use crate::ui::device_store::{self, Device};
use crate::ui::sidebar::{Handlers, Sidebar, Status};
use crate::ui::{dialogs, secrets};
use crate::ui::video_view::VideoView;
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, Button, DropDown, EventControllerKey,
    GestureClick, HeaderBar, IconTheme, Image, Label, Orientation, Overlay, Paned, ScaleButton,
    Stack, StackSwitcher, ToggleButton,
};
use reoling::{looks_multi_channel, StreamProfile};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

/// How long the window-close and signal handlers give the network thread to
/// get the disconnect packet out before the process goes away.
const DISCONNECT_GRACE: Duration = Duration::from_millis(200);

struct Active {
    key: String,
    shutdown: Arc<Notify>,
}

pub struct MainWindow {
    window: ApplicationWindow,
    header: HeaderBar,
    video: Rc<VideoView>,
    message: Label,
    sidebar: Rc<Sidebar>,
    sidebar_toggle: ToggleButton,
    bars: GtkBox,
    fullscreen_button: Button,
    stream: DropDown,
    stop: Button,
    previous: Button,
    next: Button,
    streaming: Cell<bool>,
    /// The stream the user prefers, and the ones the current device offers
    /// (what the dropdown lists, in the same order).
    profile: Cell<StreamProfile>,
    stream_options: RefCell<Vec<StreamProfile>>,
    refreshing_streams: Cell<bool>,

    devices: RefCell<Vec<Device>>,
    passwords: RefCell<HashMap<String, String>>,
    /// Device whose password should go into the keyring once login succeeds.
    save_on_login: RefCell<Option<String>>,
    active: RefCell<Option<Active>>,
    generation: Cell<u64>,
    uid_transport: UidTransport,
    sink_request_tx: tokio::sync::mpsc::Sender<SinkRequest>,
}

pub fn build(app: &Application, uid_transport: UidTransport) -> Rc<MainWindow> {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Reoling")
        .default_width(1100)
        .default_height(700)
        .build();

    let video = Rc::new(VideoView::new());

    // Every connection's network thread asks here for a `VideoSink` once it
    // knows the codec; building the pipeline touches the `Picture`, so it has
    // to happen on the GTK main thread (see `bridge::spawn_connection`).
    let (sink_request_tx, mut sink_request_rx) = tokio::sync::mpsc::channel::<SinkRequest>(8);
    let video_for_sink = Rc::clone(&video);
    glib::spawn_future_local(async move {
        while let Some((video_type, reply)) = sink_request_rx.recv().await {
            let _ = reply.send(video_for_sink.ensure_sink(video_type));
        }
    });

    let main = Rc::new_cyclic(|weak: &std::rc::Weak<MainWindow>| {
        let w = weak.clone();
        let on = |f: fn(&Rc<MainWindow>)| {
            let w = w.clone();
            move || {
                if let Some(m) = w.upgrade() {
                    f(&m)
                }
            }
        };
        let with_key = |f: fn(&Rc<MainWindow>, &str)| {
            let w = w.clone();
            move |key: &str| {
                if let Some(m) = w.upgrade() {
                    f(&m, key)
                }
            }
        };
        let w_channel = w.clone();
        let sidebar = Sidebar::new(Handlers {
            on_add: Box::new(on(MainWindow::add_device_dialog)),
            on_select: Box::new(with_key(MainWindow::select)),
            on_channel: Box::new(move |key, channel| {
                if let Some(m) = w_channel.upgrade() {
                    m.channel_changed(key, channel)
                }
            }),
            on_relogin: Box::new(with_key(MainWindow::relogin)),
            on_remove: Box::new(with_key(MainWindow::remove)),
        });

        let header = HeaderBar::new();
        let brand = GtkBox::new(Orientation::Horizontal, 8);
        let logo = Image::from_icon_name("de.dersobi.reoling");
        logo.set_pixel_size(24);
        let brand_name = Label::new(Some("Reoling"));
        brand_name.add_css_class("heading");
        brand.append(&logo);
        brand.append(&brand_name);
        header.pack_start(&brand);
        window.set_titlebar(Some(&header));

        let sidebar_toggle = ToggleButton::new();
        sidebar_toggle.set_icon_name(sidebar_icon());
        sidebar_toggle.set_tooltip_text(Some("Show or hide the device list"));
        sidebar_toggle.set_active(true);

        let pages = Stack::new();
        let switcher = StackSwitcher::new();
        switcher.set_stack(Some(&pages));
        header.set_title_widget(Some(&switcher));

        // Live view: video (with a status message over it), stream controls,
        // navigation/fullscreen row.
        let message = Label::new(Some("Add a device to start"));
        message.add_css_class("title-2");
        message.add_css_class("dim-label");
        message.set_can_target(false);
        message.set_halign(gtk4::Align::Center);
        message.set_valign(gtk4::Align::Center);
        message.set_wrap(true);
        message.set_justify(gtk4::Justification::Center);
        let overlay = Overlay::new();
        let picture = video.widget();
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        overlay.set_child(Some(picture));
        overlay.add_overlay(&message);

        let stop = Button::from_icon_name("media-playback-stop-symbolic");
        stop.set_tooltip_text(Some("Stop"));
        stop.set_sensitive(false);
        let stream = DropDown::from_strings(&["Main stream", "Sub stream"]);
        stream.set_selected(1);
        stream.set_tooltip_text(Some("Stream"));
        stream.set_sensitive(false);
        let controls = GtkBox::new(Orientation::Horizontal, 8);
        controls.set_margin_top(6);
        controls.set_margin_bottom(6);
        controls.set_margin_start(8);
        controls.set_margin_end(8);
        controls.append(&stop);
        let spacer = GtkBox::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        controls.append(&spacer);
        controls.append(&stream);

        // Bottom row. Buttons without a function yet stay in place, disabled.
        let previous = Button::from_icon_name("go-previous-symbolic");
        previous.set_tooltip_text(Some("Previous channel"));
        previous.set_sensitive(false);
        let next = Button::from_icon_name("go-next-symbolic");
        next.set_tooltip_text(Some("Next channel"));
        next.set_sensitive(false);
        let scrollview = Button::from_icon_name("media-playlist-repeat-symbolic");
        scrollview.set_tooltip_text(Some("Scrollview"));
        scrollview.set_sensitive(false);
        let volume = ScaleButton::new(
            0.0,
            1.0,
            0.05,
            &[
                "audio-volume-muted-symbolic",
                "audio-volume-high-symbolic",
                "audio-volume-low-symbolic",
                "audio-volume-medium-symbolic",
            ],
        );
        volume.set_tooltip_text(Some("Volume"));
        volume.set_sensitive(false);
        let split = Button::from_icon_name("view-grid-symbolic");
        split.set_tooltip_text(Some("Split view"));
        split.set_sensitive(false);
        let fullscreen_button = Button::from_icon_name("view-fullscreen-symbolic");
        fullscreen_button.set_tooltip_text(Some("Fullscreen"));
        let navigation = GtkBox::new(Orientation::Horizontal, 8);
        navigation.set_margin_top(6);
        navigation.set_margin_bottom(6);
        navigation.set_margin_start(8);
        navigation.set_margin_end(8);
        navigation.append(&sidebar_toggle);
        navigation.append(&previous);
        navigation.append(&next);
        navigation.append(&scrollview);
        let spacer = GtkBox::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        navigation.append(&spacer);
        navigation.append(&volume);
        navigation.append(&split);
        navigation.append(&fullscreen_button);

        let bars = GtkBox::new(Orientation::Vertical, 0);
        bars.append(&controls);
        bars.append(&navigation);

        let live = GtkBox::new(Orientation::Vertical, 0);
        live.append(&overlay);
        live.append(&bars);
        pages.add_titled(&live, Some("live"), "Live View");

        let playback = Label::new(Some("Playback is not available yet"));
        playback.add_css_class("title-2");
        playback.add_css_class("dim-label");
        pages.add_titled(&playback, Some("playback"), "Playback");

        let paned = Paned::new(Orientation::Horizontal);
        paned.set_start_child(Some(sidebar.widget()));
        paned.set_end_child(Some(&pages));
        paned.set_resize_start_child(false);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        window.set_child(Some(&paned));

        // Wiring that only needs the weak handle.
        let w = weak.clone();
        stop.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.stop_session();
                m.show_message("Stopped");
            }
        });
        let w = weak.clone();
        stream.connect_selected_notify(move |dropdown| {
            if let Some(m) = w.upgrade() {
                if m.refreshing_streams.get() {
                    return;
                }
                let chosen = m.stream_options.borrow().get(dropdown.selected() as usize).copied();
                if let Some(profile) = chosen {
                    m.profile.set(profile);
                }
                if let Some(key) = m.active_key() {
                    m.start_session(&key);
                }
            }
        });
        let w = weak.clone();
        previous.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.step_channel(-1)
            }
        });
        let w = weak.clone();
        next.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.step_channel(1)
            }
        });
        let w = weak.clone();
        fullscreen_button.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                let on = !m.window.is_fullscreen();
                m.set_fullscreen(on)
            }
        });
        let sidebar_for_toggle = Rc::clone(&sidebar);
        sidebar_toggle.connect_toggled(move |t| sidebar_for_toggle.widget().set_visible(t.is_active()));

        // Double-click on the video and Esc toggle/leave fullscreen.
        let w = weak.clone();
        let click = GestureClick::new();
        click.set_button(1);
        click.connect_pressed(move |_, presses, _, _| {
            if presses == 2 {
                if let Some(m) = w.upgrade() {
                    let on = !m.window.is_fullscreen();
                    m.set_fullscreen(on)
                }
            }
        });
        video.widget().add_controller(click);
        let w = weak.clone();
        let keys = EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk4::gdk::Key::Escape {
                if let Some(m) = w.upgrade() {
                    if m.window.is_fullscreen() {
                        m.set_fullscreen(false);
                        return glib::Propagation::Stop;
                    }
                }
            }
            glib::Propagation::Proceed
        });
        window.add_controller(keys);

        // Best-effort disconnect on close: the network thread needs a moment
        // to get the packet out before the process exits.
        let w = weak.clone();
        window.connect_close_request(move |_| {
            if let Some(m) = w.upgrade() {
                m.shutdown_active(true);
            }
            glib::Propagation::Proceed
        });

        MainWindow {
            window: window.clone(),
            header,
            video: Rc::clone(&video),
            message,
            sidebar,
            sidebar_toggle,
            bars,
            fullscreen_button,
            stream,
            stop,
            previous,
            next,
            streaming: Cell::new(false),
            // Start on the lightest stream; heavier ones are opt-in.
            profile: Cell::new(StreamProfile::Sub),
            stream_options: RefCell::new(vec![StreamProfile::Main, StreamProfile::Sub]),
            refreshing_streams: Cell::new(false),
            devices: RefCell::new(Vec::new()),
            passwords: RefCell::new(HashMap::new()),
            save_on_login: RefCell::new(None),
            active: RefCell::new(None),
            generation: Cell::new(0),
            uid_transport,
            sink_request_tx,
        }
    });

    for device in device_store::load() {
        main.sidebar.add_device(&device);
        main.devices.borrow_mut().push(device);
    }
    if !main.devices.borrow().is_empty() {
        main.show_message("Select a device");
    }

    // SIGINT/SIGTERM bypass the close-request signal; disconnect and quit.
    for signum in [2, 15] {
        let m = Rc::clone(&main);
        let app = app.clone();
        glib::source::unix_signal_add_local(signum, move || {
            m.shutdown_active(true);
            app.quit();
            glib::ControlFlow::Break
        });
    }

    window.present();
    main
}

fn sidebar_icon() -> &'static str {
    let has = |name: &str| {
        gtk4::gdk::Display::default()
            .map(|d| IconTheme::for_display(&d).has_icon(name))
            .unwrap_or(false)
    };
    if has("sidebar-show-symbolic") {
        "sidebar-show-symbolic"
    } else {
        "view-list-symbolic"
    }
}

impl MainWindow {
    fn quality(&self) -> StreamProfile {
        self.profile.get()
    }

    /// Lists in the dropdown the streams the current device/channel offers.
    /// Keeps the preferred stream if it is offered, else falls back to the
    /// lightest one. Returns whether the stream in use changed.
    fn refresh_streams(&self) -> bool {
        let offered = self
            .active_key()
            .and_then(|k| self.device(&k))
            .and_then(|d| d.channels.into_iter().find(|c| c.channel_id == d.channel))
            .map(|c| c.streams)
            .unwrap_or_else(|| vec![StreamProfile::Main, StreamProfile::Sub]);
        let before = self.profile.get();
        let chosen = if offered.contains(&before) {
            before
        } else {
            offered.last().copied().unwrap_or(StreamProfile::Sub)
        };
        let label = |p: &StreamProfile| match p {
            StreamProfile::Main => "Main stream",
            StreamProfile::Extern => "Extern stream",
            StreamProfile::Sub => "Sub stream",
        };
        self.refreshing_streams.set(true);
        let names: Vec<&str> = offered.iter().map(label).collect();
        self.stream.set_model(Some(&gtk4::StringList::new(&names)));
        self.stream.set_selected(offered.iter().position(|p| *p == chosen).unwrap_or(0) as u32);
        self.refreshing_streams.set(false);
        *self.stream_options.borrow_mut() = offered;
        self.profile.set(chosen);
        chosen != before
    }

    fn active_key(&self) -> Option<String> {
        self.active.borrow().as_ref().map(|a| a.key.clone())
    }

    fn show_message(&self, text: &str) {
        self.message.set_text(text);
        self.message.set_visible(true);
    }

    fn set_fullscreen(&self, on: bool) {
        if on {
            self.window.fullscreen();
        } else {
            self.window.unfullscreen();
        }
        self.header.set_visible(!on);
        self.bars.set_visible(!on);
        self.sidebar.widget().set_visible(!on && self.sidebar_toggle.is_active());
        self.fullscreen_button.set_icon_name(if on {
            "view-restore-symbolic"
        } else {
            "view-fullscreen-symbolic"
        });
    }

    fn add_device_dialog(self: &Rc<Self>) {
        let this = Rc::clone(self);
        dialogs::add_device(self.window.upcast_ref(), move |target| {
            let device = Device::new(target);
            this.sidebar.add_device(&device);
            this.devices.borrow_mut().push(device.clone());
            device_store::save(&this.devices.borrow());
            this.select(&device.key);
        });
    }

    fn device(&self, key: &str) -> Option<Device> {
        self.devices.borrow().iter().find(|d| d.key == key).cloned()
    }

    /// Click on a card: connect with the remembered password, the keyring's,
    /// or ask for the login.
    fn select(self: &Rc<Self>, key: &str) {
        let Some(device) = self.device(key) else { return };
        self.sidebar.select(key);
        if self.passwords.borrow().contains_key(key) {
            self.start_session(key);
            return;
        }
        let this = Rc::clone(self);
        let key = key.to_string();
        glib::spawn_future_local(async move {
            match secrets::lookup(key.clone()).await {
                Some(password) => {
                    this.passwords.borrow_mut().insert(key.clone(), password);
                    this.start_session(&key);
                }
                None => this.ask_login(device),
            }
        });
    }

    fn relogin(self: &Rc<Self>, key: &str) {
        if let Some(device) = self.device(key) {
            self.ask_login(device);
        }
    }

    fn ask_login(self: &Rc<Self>, device: Device) {
        let this = Rc::clone(self);
        let key = device.key.clone();
        dialogs::login(
            self.window.upcast_ref(),
            &device.name,
            &device.username,
            move |username, password, remember| {
                if let Some(d) = this.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
                    d.username = username;
                }
                device_store::save(&this.devices.borrow());
                this.passwords.borrow_mut().insert(key.clone(), password);
                if remember {
                    *this.save_on_login.borrow_mut() = Some(key.clone());
                } else {
                    glib::spawn_future_local(secrets::forget(key.clone()));
                }
                this.start_session(&key);
            },
        );
    }

    fn remove(self: &Rc<Self>, key: &str) {
        if self.active_key().as_deref() == Some(key) {
            self.stop_session();
            self.show_message("Select a device");
        }
        self.sidebar.remove_device(key);
        self.devices.borrow_mut().retain(|d| d.key != key);
        device_store::save(&self.devices.borrow());
        self.passwords.borrow_mut().remove(key);
        glib::spawn_future_local(secrets::forget(key.to_string()));
    }

    fn channel_changed(self: &Rc<Self>, key: &str, channel: u8) {
        if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
            d.channel = channel;
        }
        device_store::save(&self.devices.borrow());
        if self.active_key().as_deref() == Some(key) {
            self.refresh_streams();
            self.start_session(key);
        }
    }

    fn step_channel(&self, delta: i16) {
        let Some(key) = self.active_key() else { return };
        let Some(device) = self.device(&key) else { return };
        let next = (i16::from(device.channel) + delta).clamp(0, 255) as u8;
        self.sidebar.set_channel(&key, next);
    }

    /// Ends the current session (if any): tells the device we are leaving and
    /// drops the pipeline. Events from it are ignored from then on.
    fn stop_session(&self) {
        self.generation.set(self.generation.get() + 1);
        self.streaming.set(false);
        if let Some(active) = self.active.borrow_mut().take() {
            active.shutdown.notify_one();
            self.sidebar.set_status(&active.key, &Status::Idle);
        }
        self.video.reset();
        self.update_controls();
    }

    /// Controls that only make sense with a session, or with a multi-camera
    /// device (NVR / Home Hub), follow the current state.
    fn update_controls(&self) {
        let active = self.active_key();
        let multi = active
            .as_deref()
            .and_then(|k| self.device(k))
            .map(|d| d.multi_channel)
            .unwrap_or(false);
        self.stop.set_sensitive(active.is_some());
        self.stream.set_sensitive(self.streaming.get());
        self.previous.set_sensitive(multi);
        self.next.set_sensitive(multi);
    }

    /// Takes the device's own name, kind and channels once it has told us.
    /// Returns whether the stream to play changed as a result (the caller
    /// then restarts the session).
    fn apply_identity(&self, key: &str, identity: &reoling::DeviceIdentity) -> bool {
        let text = |s: &Option<String>| s.clone().unwrap_or_default().trim().to_string();
        let (name, model) = (text(&identity.name), text(&identity.model));
        let multi = looks_multi_channel(&name, &model) || identity.channels.len() > 1;
        if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
            if !name.is_empty() {
                d.name = name.clone();
            }
            if !name.is_empty() || !model.is_empty() {
                d.multi_channel = multi;
            }
            if !identity.channels.is_empty() {
                d.channels = identity.channels.clone();
            }
        }
        if !name.is_empty() {
            self.sidebar.set_name(key, &name);
        }
        device_store::save(&self.devices.borrow());
        let stream_changed = self.refresh_streams();
        self.update_controls();
        stream_changed
    }

    fn shutdown_active(&self, wait: bool) {
        if let Some(active) = self.active.borrow().as_ref() {
            active.shutdown.notify_one();
            if wait {
                std::thread::sleep(DISCONNECT_GRACE);
            }
        }
    }

    fn start_session(self: &Rc<Self>, key: &str) {
        self.stop_session();
        let Some(device) = self.device(key) else { return };
        let Some(password) = self.passwords.borrow().get(key).cloned() else { return };

        let generation = self.generation.get();
        self.sidebar.select(key);
        self.sidebar.set_status(key, &Status::Connecting);
        self.show_message("Connecting…");

        let (receiver, shutdown) = spawn_connection(
            device.target.clone(),
            device.username.clone(),
            password.clone(),
            device.channel,
            self.quality(),
            self.uid_transport,
            self.sink_request_tx.clone(),
        );
        *self.active.borrow_mut() = Some(Active { key: key.to_string(), shutdown });
        self.update_controls();

        let this = Rc::clone(self);
        let key = key.to_string();
        glib::spawn_future_local(async move {
            let mut failed = false;
            while let Ok(event) = receiver.recv().await {
                if this.generation.get() != generation {
                    return;
                }
                match event {
                    AppEvent::LoggedIn(identity) => {
                        if this.apply_identity(&key, &identity) {
                            // The stream we started is not one this device
                            // offers; begin again on one it does.
                            this.start_session(&key);
                            return;
                        }
                        this.sidebar.set_status(&key, &Status::Connected);
                        this.show_message("Starting video…");
                        if this.save_on_login.borrow().as_deref() == Some(key.as_str()) {
                            this.save_on_login.borrow_mut().take();
                            let name =
                                this.device(&key).map(|d| d.name).unwrap_or_default();
                            glib::spawn_future_local(secrets::store(
                                key.clone(),
                                name,
                                password.clone(),
                            ));
                        }
                    }
                    // Frames already went straight into GStreamer; this only
                    // tells us the first one arrived.
                    AppEvent::FrameDelivered => {
                        if this.message.is_visible() {
                            this.message.set_visible(false);
                            this.streaming.set(true);
                            this.update_controls();
                        }
                    }
                    AppEvent::Failed(reason) => {
                        failed = true;
                        // A rejected password must not be reused silently.
                        this.passwords.borrow_mut().remove(&key);
                        this.sidebar.set_status(&key, &Status::Failed(reason.clone()));
                        this.show_message(&reason);
                    }
                }
            }
            if this.generation.get() == generation && !failed {
                this.sidebar.set_status(&key, &Status::Idle);
                this.show_message("Stream ended");
            }
        });
    }
}
