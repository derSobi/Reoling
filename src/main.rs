mod ui;

use ui::bridge::{spawn_connection, AppEvent, SinkRequest, UidTransport};
use ui::connect_dialog::build_connect_dialog;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, Button, HeaderBar, Label};
use std::rc::Rc;
use std::time::Duration;
use ui::video_view::VideoView;

const STATUS_UPDATE_INTERVAL: Duration = Duration::from_secs(1);

fn main() {
    gstreamer::init().expect("failed to initialize GStreamer");

    // Plain UDP/P2P is the default and the only path the real app
    // targets — the official Reolink client never uses TCP for the BC
    // protocol (confirmed by the user 2026-09-15: UDP:9000 direct when a
    // VPN/LAN reaches the device, UDP via P2P relay otherwise). An
    // earlier session made TCP:9000 the default instead, as a workaround
    // for a since-fixed UDP stutter bug — that workaround stuck around
    // far longer than the bug it was routing around, and every "it looks
    // fine" test run under it was quietly exercising a path the shipped
    // app should never take. `--prefer-tcp` is a diagnostic-only opt-in
    // now (see `bridge::spawn_connection`), never the default. Parsed by
    // hand and never handed to `app.run()` — GTK's own argv parser
    // rejects options it doesn't recognize.
    let uid_transport = if std::env::args().any(|a| a == "--prefer-tcp") {
        UidTransport::PreferTcp
    } else {
        UidTransport::Udp
    };

    let app = Application::builder()
        .application_id("de.dersobi.reoling")
        .build();

    app.connect_startup(|_| ui::style::install());

    app.connect_activate(move |app| {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Reoling")
            .default_width(800)
            .default_height(600)
            .build();

        let header = HeaderBar::new();
        let fullscreen_button = Button::from_icon_name("view-fullscreen-symbolic");
        fullscreen_button.set_tooltip_text(Some("Toggle fullscreen"));
        header.pack_end(&fullscreen_button);
        window.set_titlebar(Some(&header));

        let window_for_fullscreen = window.clone();
        fullscreen_button.connect_clicked(move |button| {
            if window_for_fullscreen.is_fullscreen() {
                window_for_fullscreen.unfullscreen();
                button.set_icon_name("view-fullscreen-symbolic");
            } else {
                window_for_fullscreen.fullscreen();
                button.set_icon_name("view-restore-symbolic");
            }
        });

        // gtk4::Label is already a reference-counted GObject wrapper
        // (cloning it clones the handle, not the widget), so it can be
        // cloned directly into closures without an extra Rc. VideoView is a
        // plain Rust struct wrapping GStreamer elements, so it does need Rc
        // to be shared the same way.
        let status_label = Label::new(Some("Not connected"));
        let video_view = Rc::new(VideoView::new());

        let dialog_container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

        // Holds the active connection's shutdown signal, if any, so the
        // window-close handler below can ask the background task to send
        // a proper disconnect (see `spawn_connection`'s doc comment)
        // instead of the connection just being abandoned. `Rc<RefCell<..>>`
        // rather than a plain variable because both the connect closure
        // (writes on each new connection) and the close-request closure
        // (reads on close) need to reach the same slot from the GTK main
        // thread — no other thread ever touches it.
        let active_shutdown: Rc<std::cell::RefCell<Option<std::sync::Arc<tokio::sync::Notify>>>> =
            Rc::new(std::cell::RefCell::new(None));

        // Every connection's background thread asks here for a VideoSink
        // (once, the first time it knows the video codec) instead of
        // pushing frames through the GTK main loop — see
        // `bridge::spawn_connection`'s doc comment. This responder is the
        // only thing that ever calls `VideoView::ensure_sink`, and it runs
        // on the GTK main thread (required: building the pipeline touches
        // the `Picture` widget), but replying to a request is cheap and
        // happens once per connection, not once per frame.
        let (sink_request_tx, mut sink_request_rx) = tokio::sync::mpsc::channel::<SinkRequest>(8);
        let video_view_for_sink = Rc::clone(&video_view);
        glib::spawn_future_local(async move {
            while let Some((video_type, reply)) = sink_request_rx.recv().await {
                let sink = video_view_for_sink.ensure_sink(video_type);
                let _ = reply.send(sink);
            }
        });

        let status_label_for_dialog = status_label.clone();
        let header_for_dialog = header.clone();
        let dialog_widget_for_hide = dialog_container.clone();
        let active_shutdown_for_dialog = Rc::clone(&active_shutdown);
        let sink_request_tx_for_dialog = sink_request_tx.clone();
        build_connect_dialog(&dialog_container, move |device_name, target, username, password, channel_id, quality| {
            status_label_for_dialog.set_text("Connecting...");
            let (receiver, shutdown) = spawn_connection(
                target,
                username,
                password,
                channel_id,
                quality,
                uid_transport,
                sink_request_tx_for_dialog.clone(),
            );
            *active_shutdown_for_dialog.borrow_mut() = Some(shutdown);
            let status_label = status_label_for_dialog.clone();
            let header = header_for_dialog.clone();
            let dialog_widget = dialog_widget_for_hide.clone();
            glib::spawn_future_local(async move {
                // Setting label text triggers GTK layout/redraw work on
                // this same main thread that also composites the video
                // texture — updating it on every single frame (dozens of
                // times a second) competed with that rendering for the
                // thread and made playback choppier the higher the frame
                // rate, confirmed against real hardware 2026-09-14 (worse
                // on the higher-bitrate main stream, but present on sub
                // stream too, just less noticeable). Throttled to once a
                // second; the byte count was diagnostic-only anyway.
                let mut last_status_update = std::time::Instant::now() - STATUS_UPDATE_INTERVAL;
                while let Ok(event) = receiver.recv().await {
                    match event {
                        AppEvent::LoggedIn(_info) => {
                            status_label.set_text("Logged in, starting video...");
                            // The form has done its job; give the live view
                            // the room, the same way the official app moves
                            // from an "add device" screen to a live-view one.
                            dialog_widget.set_visible(false);
                            let title = if device_name.is_empty() {
                                "Connected".to_string()
                            } else {
                                device_name.clone()
                            };
                            header.set_title_widget(Some(&Label::new(Some(&title))));
                        }
                        AppEvent::FrameDelivered { bytes } => {
                            // The frame itself already went straight into
                            // GStreamer from the background thread (see
                            // `bridge::spawn_connection`'s doc comment) —
                            // this event only carries the byte count for
                            // the status label, throttled the same way it
                            // always was.
                            if last_status_update.elapsed() >= STATUS_UPDATE_INTERVAL {
                                status_label
                                    .set_text(&format!("Streaming ({bytes} bytes/frame)"));
                                last_status_update = std::time::Instant::now();
                            }
                        }
                        AppEvent::Failed(reason) => {
                            status_label.set_text(&format!("Error: {reason}"))
                        }
                    }
                }
            });
        });

        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        root.append(&dialog_container);
        root.append(&status_label);
        root.append(video_view.widget());
        window.set_child(Some(&root));

        // Best-effort: ask the background task to send C2D_DISC before the
        // process actually exits. Confirmed real 2026-09-16 that closing
        // via this exact path (clicking the window's own close button,
        // not Ctrl+C) still left the camera streaming even with this
        // signal wired — returning `Proceed` immediately let the window
        // (and, once it's the last one, the whole process) tear down
        // before the background thread was ever scheduled to run
        // `disconnect()`'s `.await` chain. The brief blocking sleep below
        // — same pattern as the SIGINT/SIGTERM handlers further down —
        // gives it that window; blocking the GTK main thread for 200ms is
        // an acceptable one-time cost on the way out, not something that
        // needs to stay responsive.
        let active_shutdown_for_close = Rc::clone(&active_shutdown);
        window.connect_close_request(move |_| {
            if let Some(shutdown) = active_shutdown_for_close.borrow().as_ref() {
                shutdown.notify_one();
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            glib::Propagation::Proceed
        });

        // SIGINT/SIGTERM (Ctrl+C in the terminal that launched this, a
        // `systemctl stop`, etc.) bypass the window's close-request signal
        // entirely — confirmed 2026-09-16: closing this way left the
        // camera streaming, because the disconnect path above never ran.
        // `unix_signal_add_local` hooks both into the GLib main loop
        // directly (no extra dependency — `glib` is already used
        // throughout this file); once intercepted, the default
        // terminate-on-signal behavior no longer happens on its own, so
        // this must explicitly quit the app after giving the background
        // thread a moment to get the UDP packet out. A short blocking
        // sleep in a GLib callback is fine here — the process is about to
        // exit regardless, so nothing else needs this thread to stay
        // responsive.
        const SIGINT: i32 = 2;
        const SIGTERM: i32 = 15;
        for signum in [SIGINT, SIGTERM] {
            let active_shutdown = Rc::clone(&active_shutdown);
            let app = app.clone();
            glib::source::unix_signal_add_local(signum, move || {
                if let Some(shutdown) = active_shutdown.borrow().as_ref() {
                    shutdown.notify_one();
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
                app.quit();
                glib::ControlFlow::Break
            });
        }

        window.present();
    });

    // `run_with_args::<&str>(&[])` rather than `run()`: GTK's own argv
    // parser doesn't know `--prefer-udp` (parsed by hand above) and would
    // reject it as an invalid option.
    app.run_with_args::<&str>(&[]);
}
