//! The application window: a device sidebar next to a live-view page, with a
//! playback page reserved behind the header's view switcher.

use crate::ui::bridge::{spawn_device, DeviceEvent, DeviceLink, SinkRequest, UidTransport};
use crate::ui::device_store::{self, Device};
use crate::ui::sidebar::{Handlers, Sidebar, Status};
use crate::ui::video_view::VideoView;
use crate::ui::audio::{AudioOutput, Microphone};
use crate::ui::remote::{self, RemoteControl};
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
use std::sync::Arc;
use std::time::Duration;

/// The speed the official app moves a camera at.
const PTZ_SPEED: u8 = 32;

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
    siren: Button,
    spotlight: ToggleButton,
    ptz: Button,
    remote: Rc<RemoteControl>,
    talk: ToggleButton,
    updating_talk: Cell<bool>,
    /// A talk was asked for and the camera has not answered yet.
    talk_pending: Cell<bool>,
    talk_retried: Cell<bool>,
    microphone: RefCell<Option<Microphone>>,
    updating_spotlight: Cell<bool>,
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
    preset_queue: RefCell<std::collections::VecDeque<u8>>,
    preset_in_flight: Cell<bool>,
    preset_queue_generation: Cell<u64>,
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
    audio: Arc<AudioOutput>,
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

    let audio = Arc::new(AudioOutput::new(settings.volume));
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
        controls.append(&gtk4::Separator::new(Orientation::Vertical));
        // Camera controls, per channel. Talk is not built yet.
        let talk = ToggleButton::new();
        talk.set_icon_name("audio-input-microphone-symbolic");
        talk.set_tooltip_text(Some("Talk"));
        talk.set_sensitive(false);
        let w = weak.clone();
        talk.connect_toggled(move |button| {
            if let Some(m) = w.upgrade() {
                if !m.updating_talk.get() {
                    m.toggle_talk(button.is_active());
                }
            }
        });
        let siren = Button::from_icon_name(icon_of(&["alarm-symbolic", "dialog-warning-symbolic"]));
        siren.set_tooltip_text(Some("Sound the siren"));
        siren.set_sensitive(false);
        let spotlight = ToggleButton::new();
        spotlight.set_icon_name(icon_of(&["weather-clear-symbolic", "keyboard-brightness-symbolic"]));
        spotlight.set_tooltip_text(Some("Spotlight"));
        spotlight.set_sensitive(false);
        // The camera's remote control opens in a window of its own.
        let ptz = Button::from_icon_name(icon_of(&["input-dpad-symbolic", "object-move-symbolic", "find-location-symbolic"]));
        ptz.set_tooltip_text(Some("Camera control"));
        ptz.set_sensitive(false);
        let w = weak.clone();
        ptz.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.open_remote();
            }
        });
        let (w_move, w_stop, w_zoom, w_focus) = (weak.clone(), weak.clone(), weak.clone(), weak.clone());
        let (w_add, w_goto, w_delete) = (weak.clone(), weak.clone(), weak.clone());
        let (w_calibrate, w_mp_config, w_mp_reset, w_mp_goto, w_mp_image, w_preset_image, w_preset_all, w_preset_rename) =
            (weak.clone(), weak.clone(), weak.clone(), weak.clone(), weak.clone(), weak.clone(), weak.clone(), weak.clone());
        let remote = RemoteControl::new(remote::Handlers {
            on_move: Box::new(move |command| {
                if let Some(m) = w_move.upgrade() {
                    m.control(move |link, channel| link.ptz(channel, command, PTZ_SPEED));
                }
            }),
            on_stop: Box::new(move || {
                if let Some(m) = w_stop.upgrade() {
                    m.control(|link, channel| link.ptz(channel, "stop", 0));
                }
            }),
            on_zoom: Box::new(move |position| {
                if let Some(m) = w_zoom.upgrade() {
                    m.control(move |link, channel| link.set_zoom(channel, position));
                    m.refresh_zoom_focus_soon();
                }
            }),
            on_focus: Box::new(move |position| {
                if let Some(m) = w_focus.upgrade() {
                    m.control(move |link, channel| link.set_focus(channel, position));
                    m.refresh_zoom_focus_soon();
                }
            }),
            on_add_preset: Box::new(move |name| {
                if let Some(m) = w_add.upgrade() {
                    m.add_preset(name);
                }
            }),
            on_goto_preset: Box::new(move |id| {
                if let Some(m) = w_goto.upgrade() {
                    m.control(move |link, channel| link.goto_preset(channel, id));
                }
            }),
            on_delete_preset: Box::new(move |id| {
                if let Some(m) = w_delete.upgrade() {
                    m.control(move |link, channel| link.delete_preset(channel, id));
                    // The list only changes on the device; ask again shortly.
                    m.query_presets_soon();
                }
            }),
            on_calibrate: Box::new(move || {
                if let Some(m) = w_calibrate.upgrade() {
                    m.control(|link, channel| link.calibrate(channel));
                    m.notify("Calibrating… this can take a few seconds");
                }
            }),
            on_monitor_point_config: Box::new(move |enabled, timeout| {
                if let Some(m) = w_mp_config.upgrade() {
                    m.control(move |link, channel| link.set_monitor_point_config(channel, enabled, timeout));
                }
            }),
            on_reset_monitor_point: Box::new(move |enabled, timeout| {
                if let Some(m) = w_mp_reset.upgrade() {
                    m.control(move |link, channel| link.set_monitor_point_here(channel, enabled, timeout));
                    m.query_monitor_point_soon();
                }
            }),
            on_go_to_monitor_point: Box::new(move |timeout| {
                if let Some(m) = w_mp_goto.upgrade() {
                    m.control(move |link, channel| link.go_to_monitor_point(channel, timeout));
                }
            }),
            on_refresh_monitor_point_image: Box::new(move || {
                if let Some(m) = w_mp_image.upgrade() {
                    m.control(|link, channel| link.query_monitor_point_image(channel));
                }
            }),
            on_refresh_preset_image: Box::new(move |id| {
                if let Some(m) = w_preset_image.upgrade() {
                    m.enqueue_preset_images(&[id]);
                }
            }),
            on_refresh_all_presets: Box::new(move || {
                if let Some(m) = w_preset_all.upgrade() {
                    let ids: Vec<u8> = m
                        .playing_key()
                        .and_then(|k| m.device(&k))
                        .map(|d| d.presets.iter().map(|p| p.id).collect())
                        .unwrap_or_default();
                    if std::env::var("REOLING_DEBUG_PRESETS").is_ok() {
                        eprintln!("PRESETS refresh all: {ids:?}");
                    }
                    m.enqueue_preset_images(&ids);
                }
            }),
            on_rename_preset: Box::new(move |_, _| {
                if let Some(m) = w_preset_rename.upgrade() {
                    m.notify("Renaming a preset is not supported yet");
                }
            }),
        });
        controls.append(&talk);
        controls.append(&siren);
        controls.append(&spotlight);
        controls.append(&ptz);
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
        volume.set_value(settings.volume);
        let w = weak.clone();
        volume.connect_value_changed(move |_, value| {
            if let Some(m) = w.upgrade() {
                m.audio.set_volume(value);
                m.settings.borrow_mut().volume = value;
                m.settings.borrow().save();
            }
        });
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
        siren.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.control(|link, channel| link.siren(channel));
            }
        });
        let w = weak.clone();
        spotlight.connect_toggled(move |button| {
            if let Some(m) = w.upgrade() {
                if !m.updating_spotlight.get() {
                    let on = button.is_active();
                    m.control(move |link, channel| link.spotlight(channel, on));
                }
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
            siren,
            spotlight,
            ptz,
            remote,
            talk,
            updating_talk: Cell::new(false),
            talk_pending: Cell::new(false),
            talk_retried: Cell::new(false),
            microphone: RefCell::new(None),
            updating_spotlight: Cell::new(false),
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
            preset_queue: RefCell::new(std::collections::VecDeque::new()),
            preset_in_flight: Cell::new(false),
            preset_queue_generation: Cell::new(0),
            connected: RefCell::new(HashSet::new()),
            next_link_id: Cell::new(1),
            playing: RefCell::new(None),
            autoplay: RefCell::new(None),
            last_played: RefCell::new(None),
            settings: RefCell::new(settings),
            audio: Arc::clone(&audio),
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

/// The first of these icons the theme has (the last if none).
fn icon_of(names: &[&'static str]) -> &'static str {
    let has = |name: &str| {
        gtk4::gdk::Display::default()
            .map(|d| IconTheme::for_display(&d).has_icon(name))
            .unwrap_or(false)
    };
    names.iter().copied().find(|n| has(n)).unwrap_or(names[names.len() - 1])
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

    /// Preset thumbnails are read one at a time — the camera breaks on
    /// overlapping image-file requests — each next one starting when the
    /// previous answered (or a 6 s watchdog says it never will).
    fn enqueue_preset_images(self: &Rc<Self>, ids: &[u8]) {
        {
            let mut queue = self.preset_queue.borrow_mut();
            for id in ids {
                if !queue.contains(id) {
                    queue.push_back(*id);
                }
            }
        }
        self.pump_preset_images();
    }

    fn pump_preset_images(self: &Rc<Self>) {
        if self.preset_in_flight.get() {
            return;
        }
        let Some(id) = self.preset_queue.borrow_mut().pop_front() else { return };
        if std::env::var("REOLING_DEBUG_PRESETS").is_ok() {
            eprintln!("PRESETS asking for the picture of preset {id}");
        }
        self.preset_in_flight.set(true);
        self.control(move |link, channel| link.query_preset_image(channel, id));
        let generation = self.preset_queue_generation.get() + 1;
        self.preset_queue_generation.set(generation);
        let this = Rc::clone(self);
        glib::timeout_add_local_once(Duration::from_secs(6), move || {
            if this.preset_queue_generation.get() == generation {
                this.preset_in_flight.set(false);
                this.pump_preset_images();
            }
        });
    }

    fn preset_image_done(self: &Rc<Self>) {
        if std::env::var("REOLING_DEBUG_PRESETS").is_ok() {
            eprintln!("PRESETS a picture request finished");
        }
        self.preset_in_flight.set(false);
        self.preset_queue_generation.set(self.preset_queue_generation.get() + 1);
        self.pump_preset_images();
    }

    /// Sends a control command to the channel being watched.
    fn control(&self, send: impl FnOnce(&crate::ui::bridge::DeviceLink, u8)) {
        let Some(key) = self.playing_key() else { return };
        let Some(device) = self.device(&key) else { return };
        if let Some(l) = self.links.borrow().get(&key) {
            send(&l.link, device.channel);
        }
    }

    fn open_remote(&self) {
        if self.remote.is_visible() {
            self.remote.hide();
        } else {
            self.remote.present();
            self.query_zoom();
            self.query_presets();
            self.query_monitor_point();
        }
    }

    /// Zooming makes the camera focus by itself, and a focus change can be
    /// clamped: read both back a moment after a command, and again once the
    /// autofocus has had time to settle.
    fn refresh_zoom_focus_soon(self: &Rc<Self>) {
        for delay in [Duration::from_millis(800), Duration::from_millis(2500)] {
            let this = Rc::clone(self);
            glib::timeout_add_local_once(delay, move || this.query_zoom());
        }
    }

    /// Saves the current position as a new preset with an id the camera does
    /// not already use (1..=63, the range neolink documents).
    fn add_preset(self: &Rc<Self>, name: String) {
        let Some(key) = self.playing_key() else { return };
        let used: std::collections::HashSet<u8> = self
            .device(&key)
            .map(|d| d.presets.iter().map(|p| p.id).collect())
            .unwrap_or_default();
        let Some(id) = (1..=63u8).find(|id| !used.contains(id)) else {
            self.notify("No free preset slot (63 max)");
            return;
        };
        self.control(move |link, channel| link.set_preset(channel, id, name.clone()));
        self.query_presets_soon();
    }

    fn query_presets(&self) {
        self.control(|link, channel| link.query_presets(channel));
    }

    fn query_presets_soon(self: &Rc<Self>) {
        for delay in [Duration::from_millis(500), Duration::from_millis(1500)] {
            let this = Rc::clone(self);
            glib::timeout_add_local_once(delay, move || this.query_presets());
        }
    }

    /// Asks the watched camera for Monitor Point's state, if it has one.
    fn query_monitor_point(&self) {
        let has_monitor_point = self
            .playing_key()
            .and_then(|k| self.device(&k))
            .and_then(|d| d.abilities.get(&d.channel).copied())
            .is_some_and(|a| a.monitor_point);
        if has_monitor_point {
            self.control(|link, channel| link.query_monitor_point(channel));
        }
    }

    fn query_monitor_point_soon(self: &Rc<Self>) {
        for delay in [Duration::from_millis(500), Duration::from_millis(1500)] {
            let this = Rc::clone(self);
            glib::timeout_add_local_once(delay, move || this.query_monitor_point());
        }
    }

    /// Asks the watched camera where its zoom and focus stand, if it has them.
    fn query_zoom(&self) {
        let has_zoom = self
            .playing_key()
            .and_then(|k| self.device(&k))
            .and_then(|d| d.abilities.get(&d.channel).copied())
            .is_some_and(|a| a.zoom);
        if has_zoom {
            self.control(|link, channel| link.query_zoom_focus(channel));
        }
    }

    fn set_talk_button(&self, on: bool) {
        self.updating_talk.set(true);
        self.talk.set_active(on);
        self.updating_talk.set(false);
    }

    /// The Talk button: opens a session with the camera and, once it accepts,
    /// starts sending the microphone.
    fn toggle_talk(self: &Rc<Self>, on: bool) {
        if on {
            self.talk_pending.set(true);
            self.talk_retried.set(false);
            self.control(|link, channel| link.talk_start(channel));
        } else {
            self.end_talk();
        }
    }

    /// Ends any talk: the microphone stops, the camera is told, the button
    /// pops out.
    fn end_talk(&self) {
        let was = self.talk_pending.replace(false) || self.microphone.borrow_mut().take().is_some();
        self.set_talk_button(false);
        if was {
            self.control(|link, channel| link.talk_stop(channel));
        }
        self.update_controls();
    }

    /// The camera answered the talk configuration.
    fn talk_answered(self: &Rc<Self>, code: u16) {
        if !self.talk_pending.get() {
            return;
        }
        if code == 422 && !self.talk_retried.get() {
            // Another talk is still open on the camera (or one of ours died):
            // close it and ask again, as the official app does.
            self.talk_retried.set(true);
            self.control(|link, channel| link.talk_stop(channel));
            self.control(|link, channel| link.talk_start(channel));
            return;
        }
        if code != 200 {
            self.talk_pending.set(false);
            self.set_talk_button(false);
            self.notify(&format!("Talk: the camera refused (code {code})"));
            return;
        }
        self.talk_pending.set(false);
        let Some(key) = self.playing_key() else { return };
        let Some(device) = self.device(&key) else { return };
        let sink = self.links.borrow().get(&key).map(|l| l.link.talk_sink(device.channel));
        let Some(sink) = sink else { return };
        match Microphone::start(sink) {
            Ok(mic) => {
                *self.microphone.borrow_mut() = Some(mic);
                self.notify("Talking… press the microphone again to stop");
            }
            Err(e) => {
                self.set_talk_button(false);
                self.control(|link, channel| link.talk_stop(channel));
                self.notify(&format!("No microphone: {e}"));
            }
        }
    }

    fn set_spotlight_button(&self, on: bool) {
        self.updating_spotlight.set(true);
        self.spotlight.set_active(on);
        self.updating_spotlight.set(false);
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
        self.audio.reset();
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
        // What this channel's camera can do; until the device has said, assume
        // it can (a wrong guess is answered with a refusal).
        let abilities = playing
            .as_deref()
            .and_then(|k| self.device(k))
            .and_then(|d| d.abilities.get(&d.channel).copied());
        let (siren, spotlight) = abilities.map_or((true, true), |a| (a.siren, a.spotlight));
        let has_ptz = abilities.is_some_and(|a| a.ptz());
        let can_talk = abilities.is_some_and(|a| a.talk);
        if !self.talk.is_active() {
            self.talk.set_sensitive(self.streaming.get() && can_talk);
        }
        self.talk.set_tooltip_text(Some(if can_talk { "Talk" } else { "This camera has no two-way audio" }));
        self.ptz.set_sensitive(self.streaming.get() && has_ptz);
        let (move_ok, zoom_ok) = abilities.map_or((false, false), |a| (a.pan || a.tilt, a.zoom));
        let (calibration_ok, monitor_point_ok) = abilities.map_or((false, false), |a| (a.calibration, a.monitor_point));
        let streaming = self.streaming.get();
        self.remote.set_enabled(
            streaming && move_ok,
            streaming && zoom_ok,
            streaming && zoom_ok,
            streaming && calibration_ok,
            streaming && monitor_point_ok,
        );
        if let Some(d) = playing.as_deref().and_then(|k| self.device(k)) {
            let camera = d.channels.iter().find(|c| c.channel_id == d.channel).map(|c| c.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| format!("Channel {}", u16::from(d.channel) + 1));
            self.remote.set_target(&format!("{} — {camera}", d.name));
        }
        self.ptz.set_tooltip_text(Some(if has_ptz { "Camera control" } else { "This camera cannot move" }));
        self.siren.set_sensitive(self.streaming.get() && siren);
        self.spotlight.set_sensitive(self.streaming.get() && spotlight);
        self.siren.set_tooltip_text(Some(if siren { "Sound the siren" } else { "This camera has no siren" }));
        self.spotlight
            .set_tooltip_text(Some(if spotlight { "Spotlight" } else { "This camera has no spotlight" }));
        self.record.set_sensitive(self.streaming.get());
        self.stream.set_sensitive(self.streaming.get());
        self.previous.set_sensitive(multi);
        self.next.set_sensitive(multi);
    }

    fn settings_dialog(self: &Rc<Self>) {
        let this = Rc::clone(self);
        let current = self.settings.borrow().clone();
        dialogs::settings(self.window.upcast_ref(), &current, move |mut new| {
            // The volume slider lives in the main window; keep its value.
            new.volume = this.settings.borrow().volume;
            let decoding_changed = {
                let old = this.settings.borrow();
                old.decoding != new.decoding
                    || old.hardware_decoder != new.hardware_decoder
                    || old.latency != new.latency
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
                Arc::clone(&this.audio),
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
            DeviceEvent::ControlReply { msg_id, code } => {
                let ok = code == 200;
                match msg_id {
                    201 => {
                        self.talk_answered(code);
                    }
                    18 | 295 => {
                        if !ok {
                            self.notify(&format!("Move: the camera refused (code {code})"));
                        }
                    }
                    263 => {
                        if ok {
                            self.notify("Siren sounding");
                        } else {
                            self.notify(&format!("Siren: the camera refused (code {code})"));
                        }
                    }
                    288 => {
                        if ok {
                            self.notify("Spotlight command sent");
                        } else {
                            self.notify(&format!("Spotlight: the camera refused (code {code})"));
                            // It did not switch; show the truth.
                            let on = self.spotlight.is_active();
                            self.set_spotlight_button(!on);
                        }
                    }
                    341 => {
                        // This reply is the only signal the wire protocol
                        // gives at all — whether it means "request queued"
                        // or "physically finished" is not confirmed either
                        // way, so the busy state clears on it regardless.
                        self.remote.set_calibrating(false);
                        if ok {
                            self.notify("Calibration finished");
                        } else {
                            self.notify(&format!("Calibration: the camera refused (code {code})"));
                        }
                    }
                    // Sent for three different actions (a plain config
                    // change, "Reset", "Return") — only a failure is worth a
                    // message; a config change alone would spam one on every
                    // settled slider drag otherwise.
                    331 if !ok => {
                        self.notify(&format!("Monitor Point: the camera refused (code {code})"));
                    }
                    _ => {}
                }
            }
            DeviceEvent::Abilities(abilities) => {
                if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
                    d.abilities.extend(abilities);
                }
                self.update_controls();
            }
            DeviceEvent::ZoomFocus { channel_id, zoom, focus } => {
                let watching = self.playing_key().and_then(|k| self.device(&k)).is_some_and(|d| d.channel == channel_id);
                if watching {
                    if let Some((min, max, current)) = zoom {
                        self.remote.set_zoom_range(min, max, current);
                    }
                    if let Some((min, max, current)) = focus {
                        self.remote.set_focus_range(min, max, current);
                    }
                }
            }
            DeviceEvent::Presets { channel_id, presets } => {
                if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
                    if d.channel == channel_id {
                        d.presets = presets.clone();
                    }
                }
                let watching = self.playing_key().and_then(|k| self.device(&k)).is_some_and(|d| d.channel == channel_id);
                if watching {
                    self.remote.set_presets(&presets);
                }
            }
            DeviceEvent::MonitorPoint { channel_id, state } => {
                let watching = self.playing_key().and_then(|k| self.device(&k)).is_some_and(|d| d.channel == channel_id);
                if watching {
                    self.remote.set_monitor_point(state);
                    if state.valid {
                        self.control(|link, channel| link.query_monitor_point_image(channel));
                    } else {
                        self.remote.clear_monitor_point_image();
                    }
                }
            }
            DeviceEvent::MonitorPointImage { channel_id, jpeg } => {
                let watching = self.playing_key().and_then(|k| self.device(&k)).is_some_and(|d| d.channel == channel_id);
                if watching {
                    self.remote.set_monitor_point_image(&jpeg);
                }
            }
            DeviceEvent::PresetImage { channel_id, preset_id, jpeg } => {
                let watching = self.playing_key().and_then(|k| self.device(&k)).is_some_and(|d| d.channel == channel_id);
                if watching {
                    self.remote.set_preset_image(preset_id, &jpeg);
                }
                self.preset_image_done();
            }
            DeviceEvent::ImageNotFound { preset_id } => {
                if preset_id.is_some() {
                    self.preset_image_done();
                }
            }
            DeviceEvent::ControlFailed(reason) => {
                self.notify(&format!("Command not sent: {reason}"));
            }
            DeviceEvent::Playing => {
                if self.playing_key().as_deref() == Some(key) {
                    self.message.set_visible(false);
                    self.streaming.set(true);
                    self.update_controls();
                    self.query_zoom();
                    self.query_monitor_point();
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
        self.set_spotlight_button(false);
        self.end_talk();
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
        self.end_talk();
        self.video.freeze();
        if let Some(sink) = self.video.current_sink() {
            sink.stop_recording();
        }
        self.set_record_button(false);
        self.audio.reset();
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
            return;
        }
        // Copied out first: `play` changes `stopped`, so a borrow must not
        // still be held when it runs.
        let stopped = self.stopped.borrow().clone();
        if let Some(key) = stopped {
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
