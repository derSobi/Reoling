//! The application window: a device sidebar next to a live-view page, with a
//! playback page reserved behind the header's view switcher.

use crate::ui::bridge::{spawn_device, DeviceEvent, DeviceLink, SinkRequest, UidTransport};
use crate::ui::device_store::{self, Device};
use crate::ui::sidebar::{Handlers, Sidebar, Status};
use crate::ui::video_view::VideoView;
use crate::ui::settings::Settings;
use crate::ui::{dialogs, secrets};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, Button, DropDown, EventControllerKey,
    GestureClick, HeaderBar, IconTheme, Image, Label, Orientation, Overlay, Paned, ScaleButton,
    Revealer, RevealerTransitionType, Stack, StackSwitcher, ToggleButton,
};
use reoling::{looks_multi_channel, StreamProfile};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

/// How long the window-close and signal handlers give the network threads to
/// get the disconnect packets out before the process goes away.
/// Space to the left of the logo, matching what the header leaves above and
/// below it.
const LOGO_MARGIN: i32 = 5;

const DISCONNECT_GRACE: Duration = Duration::from_millis(200);

/// A device's live connection. `id` tells a replaced connection's late events
/// apart from the current one's.
struct Link {
    id: u64,
    link: DeviceLink,
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
    snapshot: Button,
    record: ToggleButton,
    /// Set while the code, not the user, flips the record button.
    updating_record: Cell<bool>,
    notice: Label,
    notice_generation: Cell<u64>,
    live: GtkBox,
    fullscreen_bar: Revealer,
    bar_generation: Cell<u64>,
    pointer_on_bar: Cell<bool>,
    /// The device whose stream the user stopped; the Play button restarts it.
    stopped: RefCell<Option<String>>,
    previous: Button,
    next: Button,
    /// Whether the picture on screen is really flowing.
    streaming: Cell<bool>,
    /// The stream the user prefers, and the ones the watched device offers
    /// (what the dropdown lists, in the same order).
    profile: Cell<StreamProfile>,
    stream_options: RefCell<Vec<StreamProfile>>,
    refreshing_streams: Cell<bool>,

    devices: RefCell<Vec<Device>>,
    passwords: RefCell<HashMap<String, String>>,
    /// Devices whose password goes into the keyring once they log in.
    remember: RefCell<HashSet<String>>,
    /// Every device is connected all the time; streaming is separate.
    links: RefCell<HashMap<String, Link>>,
    /// The devices that have logged in on their current link.
    connected: RefCell<HashSet<String>>,
    next_link_id: Cell<u64>,
    /// The device being watched, and the one to watch as soon as it is
    /// connected.
    playing: RefCell<Option<String>>,
    autoplay: RefCell<Option<String>>,
    /// Restored on the next start.
    last_played: RefCell<Option<String>>,
    settings: RefCell<Settings>,
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

    let settings = Settings::load();
    settings.apply();

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
            on_select: Box::new(with_key(MainWindow::activate)),
            on_channel: Box::new(move |key, channel| {
                if let Some(m) = w_channel.upgrade() {
                    m.pick_channel(key, channel)
                }
            }),
            on_relogin: Box::new(with_key(MainWindow::relogin)),
            on_remove: Box::new(with_key(MainWindow::remove)),
        });

        let header = HeaderBar::new();
        let brand = GtkBox::new(Orientation::Horizontal, 8);
        let logo = Image::from_icon_name("de.dersobi.reoling");
        logo.set_pixel_size(24);
        // The header centres it vertically; give it as much room on the left.
        logo.set_margin_start(LOGO_MARGIN);
        let brand_name = Label::new(Some("Reoling"));
        brand_name.add_css_class("heading");
        brand.append(&logo);
        brand.append(&brand_name);
        header.pack_start(&brand);
        let settings_button = Button::from_icon_name("preferences-system-symbolic");
        settings_button.set_tooltip_text(Some("Settings"));
        header.pack_end(&settings_button);
        let w = weak.clone();
        settings_button.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.settings_dialog();
            }
        });
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

        // A short confirmation ("Snapshot saved…") near the top of the video.
        let notice = Label::new(None);
        notice.add_css_class("osd");
        notice.set_halign(gtk4::Align::Center);
        notice.set_valign(gtk4::Align::Start);
        notice.set_margin_top(12);
        notice.set_visible(false);
        notice.set_can_target(false);
        overlay.add_overlay(&notice);

        // In fullscreen the controls slide in over the bottom of the video
        // whenever the pointer moves.
        let fullscreen_bar = Revealer::new();
        fullscreen_bar.set_transition_type(RevealerTransitionType::SlideUp);
        fullscreen_bar.set_valign(gtk4::Align::End);
        fullscreen_bar.set_visible(false);
        overlay.add_overlay(&fullscreen_bar);

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
        let snapshot = Button::from_icon_name("camera-photo-symbolic");
        snapshot.set_tooltip_text(Some("Save a snapshot"));
        snapshot.set_sensitive(false);
        let record = ToggleButton::new();
        record.set_icon_name("media-record-symbolic");
        record.set_tooltip_text(Some("Record"));
        record.set_sensitive(false);
        controls.append(&snapshot);
        controls.append(&record);
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
                m.stop_or_play();
            }
        });
        let w = weak.clone();
        snapshot.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.take_snapshot();
            }
        });
        let w = weak.clone();
        record.connect_toggled(move |button| {
            if let Some(m) = w.upgrade() {
                if !m.updating_record.get() {
                    m.toggle_recording(button.is_active());
                }
            }
        });
        // Fullscreen: any pointer movement brings the controls up for a while.
        let w = weak.clone();
        let motion = gtk4::EventControllerMotion::new();
        motion.connect_motion(move |_, _, _| {
            if let Some(m) = w.upgrade() {
                m.show_fullscreen_bar();
            }
        });
        overlay.add_controller(motion);
        let w = weak.clone();
        let over_bar = gtk4::EventControllerMotion::new();
        over_bar.connect_enter(move |_, _, _| {
            if let Some(m) = w.upgrade() {
                m.pointer_on_bar.set(true);
            }
        });
        let w = weak.clone();
        over_bar.connect_leave(move |_| {
            if let Some(m) = w.upgrade() {
                m.pointer_on_bar.set(false);
                m.show_fullscreen_bar();
            }
        });
        bars.add_controller(over_bar);
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
                if let Some(key) = m.playing_key() {
                    // Not from inside the dropdown's own handler: its popup
                    // is still closing.
                    glib::idle_add_local_once(move || m.play(&key));
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
                m.shutdown_all();
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
            snapshot,
            record,
            updating_record: Cell::new(false),
            notice,
            notice_generation: Cell::new(0),
            live: live.clone(),
            fullscreen_bar,
            bar_generation: Cell::new(0),
            pointer_on_bar: Cell::new(false),
            stopped: RefCell::new(None),
            previous,
            next,
            streaming: Cell::new(false),
            // Start on the lightest stream; heavier ones are opt-in.
            profile: Cell::new(StreamProfile::Sub),
            stream_options: RefCell::new(vec![StreamProfile::Main, StreamProfile::Sub]),
            refreshing_streams: Cell::new(false),
            devices: RefCell::new(Vec::new()),
            passwords: RefCell::new(HashMap::new()),
            remember: RefCell::new(HashSet::new()),
            links: RefCell::new(HashMap::new()),
            connected: RefCell::new(HashSet::new()),
            next_link_id: Cell::new(1),
            playing: RefCell::new(None),
            autoplay: RefCell::new(None),
            last_played: RefCell::new(None),
            settings: RefCell::new(settings),
            uid_transport,
            sink_request_tx,
        }
    });

    let (devices, last) = device_store::load();
    *main.last_played.borrow_mut() = last.clone();
    *main.autoplay.borrow_mut() = last;
    for device in devices {
        main.sidebar.add_device(&device);
        main.devices.borrow_mut().push(device);
    }
    if main.devices.borrow().is_empty() {
        main.show_message("Add a device to start");
    } else {
        main.show_message("Select a device");
    }
    let keys: Vec<String> = main.devices.borrow().iter().map(|d| d.key.clone()).collect();
    for key in keys {
        main.connect(&key, false);
    }

    // SIGINT/SIGTERM bypass the close-request signal; disconnect and quit.
    for signum in [2, 15] {
        let m = Rc::clone(&main);
        let app = app.clone();
        glib::source::unix_signal_add_local(signum, move || {
            m.shutdown_all();
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
    fn show_message(&self, text: &str) {
        self.message.set_text(text);
        self.message.set_visible(true);
    }

    fn device(&self, key: &str) -> Option<Device> {
        self.devices.borrow().iter().find(|d| d.key == key).cloned()
    }

    fn persist(&self) {
        device_store::save(&self.devices.borrow(), self.last_played.borrow().as_deref());
    }

    fn playing_key(&self) -> Option<String> {
        self.playing.borrow().clone()
    }

    fn set_fullscreen(self: &Rc<Self>, on: bool) {
        if on {
            self.window.fullscreen();
            // The control rows move onto the video, as a bar that slides in.
            self.live.remove(&self.bars);
            self.fullscreen_bar.set_child(Some(&self.bars));
            self.bars.add_css_class("osd");
            self.fullscreen_bar.set_visible(true);
            self.fullscreen_bar.set_reveal_child(true);
            self.show_fullscreen_bar();
        } else {
            self.window.unfullscreen();
            self.fullscreen_bar.set_reveal_child(false);
            self.fullscreen_bar.set_child(None::<&gtk4::Widget>);
            self.fullscreen_bar.set_visible(false);
            self.bars.remove_css_class("osd");
            self.live.append(&self.bars);
            self.bar_generation.set(self.bar_generation.get() + 1);
        }
        self.header.set_visible(!on);
        self.sidebar_toggle.set_visible(!on);
        self.sidebar.widget().set_visible(!on && self.sidebar_toggle.is_active());
        self.fullscreen_button.set_icon_name(if on {
            "view-restore-symbolic"
        } else {
            "view-fullscreen-symbolic"
        });
        self.fullscreen_button.set_tooltip_text(Some(if on { "Normal window" } else { "Fullscreen" }));
    }

    /// Brings the fullscreen controls up and hides them again after a pause
    /// in pointer movement (never while the pointer is on them).
    fn show_fullscreen_bar(self: &Rc<Self>) {
        if !self.window.is_fullscreen() {
            return;
        }
        self.fullscreen_bar.set_reveal_child(true);
        let generation = self.bar_generation.get() + 1;
        self.bar_generation.set(generation);
        let this = Rc::clone(self);
        glib::timeout_add_local_once(Duration::from_millis(2500), move || {
            if this.bar_generation.get() == generation
                && this.window.is_fullscreen()
                && !this.pointer_on_bar.get()
            {
                this.fullscreen_bar.set_reveal_child(false);
            }
        });
    }

    /// A short message over the video.
    fn notify(self: &Rc<Self>, text: &str) {
        self.notice.set_text(text);
        self.notice.set_visible(true);
        let generation = self.notice_generation.get() + 1;
        self.notice_generation.set(generation);
        let this = Rc::clone(self);
        glib::timeout_add_local_once(Duration::from_secs(4), move || {
            if this.notice_generation.get() == generation {
                this.notice.set_visible(false);
            }
        });
    }

    /// `~/Pictures/Reoling` or `~/Videos/Reoling`, created on demand.
    fn media_dir(kind: glib::UserDirectory) -> Option<std::path::PathBuf> {
        let dir = glib::user_special_dir(kind)?.join("Reoling");
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir)
    }

    /// `<device>-<channel>-<date>-<time>.<extension>` for what is playing.
    fn media_file_name(&self, extension: &str) -> String {
        let device = self.playing_key().and_then(|k| self.device(&k));
        let name = device
            .as_ref()
            .map(|d| format!("{}-ch{}", d.name, u16::from(d.channel) + 1))
            .unwrap_or_else(|| "Reoling".to_string());
        let name: String = name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
            .collect();
        let now = glib::DateTime::now_local()
            .ok()
            .and_then(|t| t.format("%Y%m%d-%H%M%S").ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        format!("{name}-{now}.{extension}")
    }

    fn take_snapshot(self: &Rc<Self>) {
        let Some(texture) = self.video.snapshot() else {
            self.notify("Nothing to save yet");
            return;
        };
        let Some(dir) = Self::media_dir(glib::UserDirectory::Pictures) else {
            self.notify("Could not create the Pictures/Reoling folder");
            return;
        };
        let path = dir.join(self.media_file_name("png"));
        match texture.save_to_png(&path) {
            Ok(()) => self.notify(&format!("Snapshot saved to {}", path.display())),
            Err(e) => self.notify(&format!("Could not save the snapshot: {e}")),
        }
    }

    fn toggle_recording(self: &Rc<Self>, on: bool) {
        let Some(sink) = self.video.current_sink() else {
            self.set_record_button(false);
            return;
        };
        if !on {
            if sink.stop_recording() {
                self.notify("Recording saved");
            }
            return;
        }
        let Some(dir) = Self::media_dir(glib::UserDirectory::Videos) else {
            self.notify("Could not create the Videos/Reoling folder");
            self.set_record_button(false);
            return;
        };
        let path = dir.join(self.media_file_name("mkv"));
        match sink.start_recording(&path) {
            Ok(()) => self.notify(&format!("Recording to {}", path.display())),
            Err(e) => {
                self.notify(&format!("Could not record: {e}"));
                self.set_record_button(false);
            }
        }
    }

    /// Sets the record button without triggering a recording change.
    fn set_record_button(&self, on: bool) {
        self.updating_record.set(true);
        self.record.set_active(on);
        self.updating_record.set(false);
    }

    /// Clears the picture and ends any recording (the pipeline is going).
    fn reset_video(&self) {
        self.video.reset();
        self.set_record_button(false);
    }

    /// Lists in the dropdown the streams the watched device/channel offers.
    /// Keeps the preferred stream if it is offered, else falls back to the
    /// lightest one. Returns whether the stream in use changed.
    fn refresh_streams(&self) -> bool {
        let offered = self
            .playing_key()
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
        // Touch the dropdown only where it differs: this runs from its own
        // "selected" handler too, and rebuilding a dropdown whose popup is
        // still open from inside that handler freezes GTK.
        let index = offered.iter().position(|p| *p == chosen).unwrap_or(0) as u32;
        if *self.stream_options.borrow() != offered {
            self.refreshing_streams.set(true);
            let names: Vec<&str> = offered.iter().map(label).collect();
            self.stream.set_model(Some(&gtk4::StringList::new(&names)));
            self.stream.set_selected(index);
            self.refreshing_streams.set(false);
        } else if self.stream.selected() != index {
            self.refreshing_streams.set(true);
            self.stream.set_selected(index);
            self.refreshing_streams.set(false);
        }
        *self.stream_options.borrow_mut() = offered;
        self.profile.set(chosen);
        chosen != before
    }

    /// Controls that only make sense while watching, or with a multi-camera
    /// device (NVR / Home Hub), follow the current state.
    fn update_controls(&self) {
        let playing = self.playing_key();
        let multi = playing
            .as_deref()
            .and_then(|k| self.device(k))
            .map(|d| d.multi_channel)
            .unwrap_or(false);
        let stopped = self.stopped.borrow().is_some();
        let (icon, tip) = if playing.is_none() && stopped {
            ("media-playback-start-symbolic", "Play")
        } else {
            ("media-playback-stop-symbolic", "Stop")
        };
        self.stop.set_icon_name(icon);
        self.stop.set_tooltip_text(Some(tip));
        self.stop.set_sensitive(playing.is_some() || stopped);
        self.snapshot.set_sensitive(self.streaming.get());
        self.record.set_sensitive(self.streaming.get());
        self.stream.set_sensitive(self.streaming.get());
        self.previous.set_sensitive(multi);
        self.next.set_sensitive(multi);
    }

    fn settings_dialog(self: &Rc<Self>) {
        let this = Rc::clone(self);
        let current = self.settings.borrow().clone();
        dialogs::settings(self.window.upcast_ref(), &current, move |new| {
            let decoding_changed = {
                let old = this.settings.borrow();
                old.decoding != new.decoding || old.hardware_decoder != new.hardware_decoder
            };
            new.apply();
            new.save();
            *this.settings.borrow_mut() = new;
            // A running stream keeps its decoder; restart it to use the new one.
            if decoding_changed {
                if let Some(key) = this.playing_key() {
                    this.play(&key);
                }
            }
        });
    }

    fn add_device_dialog(self: &Rc<Self>) {
        let this = Rc::clone(self);
        dialogs::add_device(self.window.upcast_ref(), move |target| {
            let device = Device::new(target);
            this.sidebar.add_device(&device);
            this.devices.borrow_mut().push(device.clone());
            this.persist();
            this.activate(&device.key);
        });
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
                this.persist();
                this.passwords.borrow_mut().insert(key.clone(), password);
                if remember {
                    this.remember.borrow_mut().insert(key.clone());
                } else {
                    glib::spawn_future_local(secrets::forget(key.clone()));
                }
                this.links.borrow_mut().remove(&key);
                this.connected.borrow_mut().remove(&key);
                *this.autoplay.borrow_mut() = Some(key.clone());
                this.connect(&key, true);
            },
        );
    }

    fn relogin(self: &Rc<Self>, key: &str) {
        if let Some(device) = self.device(key) {
            self.ask_login(device);
        }
    }

    /// Opens the device's connection (login, name, channels) without
    /// streaming anything. With `interactive`, asks for the login when no
    /// password is known.
    fn connect(self: &Rc<Self>, key: &str, interactive: bool) {
        if self.links.borrow().contains_key(key) {
            return;
        }
        let Some(device) = self.device(key) else { return };
        self.sidebar.set_status(key, &Status::Connecting);
        let this = Rc::clone(self);
        let key = key.to_string();
        glib::spawn_future_local(async move {
            let cached = this.passwords.borrow().get(&key).cloned();
            let password = match cached {
                Some(p) => Some(p),
                None => secrets::lookup(key.clone()).await,
            };
            let Some(password) = password else {
                this.sidebar.set_status(&key, &Status::LoginNeeded("No saved password".into()));
                if interactive {
                    this.ask_login(device);
                }
                return;
            };
            if this.links.borrow().contains_key(&key) || this.device(&key).is_none() {
                return; // connected or removed while the keyring answered
            }
            this.passwords.borrow_mut().insert(key.clone(), password.clone());
            let link = spawn_device(
                device.target.clone(),
                device.username.clone(),
                password,
                this.uid_transport,
                this.sink_request_tx.clone(),
            );
            let events = link.events.clone();
            let id = this.next_link_id.get();
            this.next_link_id.set(id + 1);
            this.links.borrow_mut().insert(key.clone(), Link { id, link });
            while let Ok(event) = events.recv().await {
                if this.links.borrow().get(&key).map(|l| l.id) != Some(id) {
                    break; // replaced or removed
                }
                this.on_event(&key, event);
            }
        });
    }

    fn on_event(self: &Rc<Self>, key: &str, event: DeviceEvent) {
        match event {
            DeviceEvent::Connected(identity) => {
                self.connected.borrow_mut().insert(key.to_string());
                self.sidebar.set_status(key, &Status::Connected);
                self.apply_identity(key, &identity);
                if self.remember.borrow_mut().remove(key) {
                    let password = self.passwords.borrow().get(key).cloned();
                    let name = self.device(key).map(|d| d.name).unwrap_or_default();
                    if let Some(password) = password {
                        glib::spawn_future_local(secrets::store(key.to_string(), name, password));
                    }
                }
                if self.autoplay.borrow().as_deref() == Some(key) {
                    self.autoplay.borrow_mut().take();
                    self.play(key);
                }
            }
            DeviceEvent::Channels(channels) => {
                self.apply_channels(key, channels);
                if self.playing_key().as_deref() == Some(key) && self.refresh_streams() {
                    self.play(key); // the stream we started is not on offer
                }
            }
            DeviceEvent::ChannelName { channel_id, name } => {
                if name.is_empty() {
                    return;
                }
                if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
                    if let Some(c) = d.channels.iter_mut().find(|c| c.channel_id == channel_id) {
                        c.name = name;
                    }
                }
                if let Some(d) = self.device(key) {
                    self.sidebar.set_channels(key, &d.channels, d.channel);
                }
            }
            DeviceEvent::Playing => {
                if self.playing_key().as_deref() == Some(key) {
                    self.message.set_visible(false);
                    self.streaming.set(true);
                    self.update_controls();
                }
            }
            DeviceEvent::PlayFailed(reason) => {
                if self.playing_key().as_deref() == Some(key) {
                    self.streaming.set(false);
                    self.show_message(&reason);
                    self.update_controls();
                }
            }
            DeviceEvent::LoginRejected => {
                self.links.borrow_mut().remove(key);
                self.connected.borrow_mut().remove(key);
                self.passwords.borrow_mut().remove(key);
                glib::spawn_future_local(secrets::forget(key.to_string()));
                self.sidebar.set_status(key, &Status::LoginNeeded("Incorrect password".into()));
                self.lost_playing(key, "Incorrect password");
            }
            DeviceEvent::Lost(reason) => {
                self.links.borrow_mut().remove(key);
                self.connected.borrow_mut().remove(key);
                self.sidebar.set_status(key, &Status::Failed(reason.clone()));
                self.lost_playing(key, &reason);
            }
        }
    }

    fn lost_playing(&self, key: &str, reason: &str) {
        if self.stopped.borrow().as_deref() == Some(key) {
            *self.stopped.borrow_mut() = None;
            self.update_controls();
        }
        if self.playing_key().as_deref() == Some(key) {
            *self.playing.borrow_mut() = None;
            self.streaming.set(false);
            self.reset_video();
            self.show_message(reason);
            self.update_controls();
        }
    }

    /// Takes the device's own name and kind once it has told us.
    fn apply_identity(&self, key: &str, identity: &reoling::DeviceIdentity) {
        let text = |s: &Option<String>| s.clone().unwrap_or_default().trim().to_string();
        let (name, model) = (text(&identity.name), text(&identity.model));
        let multi = looks_multi_channel(&name, &model);
        if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
            if !name.is_empty() {
                d.name = name.clone();
            }
            if !name.is_empty() || !model.is_empty() {
                d.multi_channel = multi;
            }
        }
        if !name.is_empty() {
            self.sidebar.set_name(key, &name);
        }
        self.persist();
        self.update_controls();
    }

    /// Takes the channel list the device pushed. Several channels means an
    /// NVR / Home Hub whatever its model string says.
    fn apply_channels(self: &Rc<Self>, key: &str, channels: Vec<reoling::ChannelInfo>) {
        if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
            if channels.len() > 1 {
                d.multi_channel = true;
            }
            // The device re-sends its list from time to time, without the names
            // we had to ask for separately; keep those.
            let mut channels = channels;
            for channel in channels.iter_mut().filter(|c| c.name.is_empty()) {
                if let Some(old) = d.channels.iter().find(|o| o.channel_id == channel.channel_id) {
                    channel.name = old.name.clone();
                }
            }
            d.channels = channels;
        }
        if let Some(d) = self.device(key) {
            self.sidebar.set_channels(key, &d.channels, d.channel);
        }
        self.persist();
        self.update_controls();
    }

    /// A click on a device's card: watch it (connecting first if need be).
    fn activate(self: &Rc<Self>, key: &str) {
        if self.device(key).is_none() {
            return;
        }
        if self.connected.borrow().contains(key) {
            self.play(key);
        } else {
            *self.autoplay.borrow_mut() = Some(key.to_string());
            self.connect(key, true);
        }
    }

    /// A click on one of a device's channels.
    fn pick_channel(self: &Rc<Self>, key: &str, channel: u8) {
        if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
            d.channel = channel;
        }
        self.persist();
        self.activate(key);
    }

    /// Streams the device's current channel, stopping whatever else was on.
    fn play(self: &Rc<Self>, key: &str) {
        let Some(device) = self.device(key) else { return };
        let other = self.playing_key().filter(|k| k != key);
        if let Some(other) = other {
            if let Some(l) = self.links.borrow().get(&other) {
                l.link.stop();
            }
        }
        self.reset_video();
        *self.stopped.borrow_mut() = None;
        *self.playing.borrow_mut() = Some(key.to_string());
        *self.last_played.borrow_mut() = Some(key.to_string());
        self.streaming.set(false);
        self.refresh_streams();
        self.persist();
        self.sidebar.select(key);
        self.sidebar.mark_channel(key, device.channel);
        self.show_message("Starting…");
        if let Some(l) = self.links.borrow().get(key) {
            l.link.play(device.channel, self.profile.get());
        }
        self.update_controls();
    }

    /// The stop button: stops the stream and forgets it as the one to
    /// restore on the next start.
    /// The stop button: stops the stream and leaves the last picture on
    /// screen; the button becomes Play. Also forgets the stream as the one to
    /// restore on the next start.
    fn stop(&self) {
        let Some(key) = self.playing_key() else { return };
        if let Some(l) = self.links.borrow().get(&key) {
            l.link.stop();
        }
        if let Some(sink) = self.video.current_sink() {
            sink.stop_recording();
        }
        self.set_record_button(false);
        *self.playing.borrow_mut() = None;
        *self.last_played.borrow_mut() = None;
        *self.stopped.borrow_mut() = Some(key);
        self.streaming.set(false);
        self.persist();
        self.update_controls();
    }

    /// The button beside the picture: Stop while streaming, Play after.
    fn stop_or_play(self: &Rc<Self>) {
        if self.playing_key().is_some() {
            self.stop();
        } else if let Some(key) = self.stopped.borrow().clone() {
            self.play(&key);
        }
    }

    fn step_channel(self: &Rc<Self>, delta: i32) {
        let Some(key) = self.playing_key() else { return };
        let Some(device) = self.device(&key) else { return };
        let ids: Vec<u8> = device.channels.iter().filter(|c| c.online).map(|c| c.channel_id).collect();
        let Some(index) = ids.iter().position(|c| *c == device.channel) else { return };
        let next = (index as i32 + delta).rem_euclid(ids.len() as i32) as usize;
        self.pick_channel(&key, ids[next]);
    }

    fn remove(self: &Rc<Self>, key: &str) {
        if self.playing_key().as_deref() == Some(key) {
            *self.playing.borrow_mut() = None;
            self.streaming.set(false);
            self.reset_video();
            self.show_message("Select a device");
            self.update_controls();
        }
        if self.last_played.borrow().as_deref() == Some(key) {
            *self.last_played.borrow_mut() = None;
        }
        if self.stopped.borrow().as_deref() == Some(key) {
            *self.stopped.borrow_mut() = None;
        }
        self.links.borrow_mut().remove(key); // dropping the link disconnects
        self.connected.borrow_mut().remove(key);
        self.sidebar.remove_device(key);
        self.devices.borrow_mut().retain(|d| d.key != key);
        self.persist();
        self.passwords.borrow_mut().remove(key);
        glib::spawn_future_local(secrets::forget(key.to_string()));
    }

    /// Tells every device we are leaving; the threads need a moment to get
    /// the packets out before the process exits.
    fn shutdown_all(&self) {
        let links = self.links.borrow();
        for l in links.values() {
            l.link.shutdown();
        }
        if !links.is_empty() {
            std::thread::sleep(DISCONNECT_GRACE);
        }
    }
}
