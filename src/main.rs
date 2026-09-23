mod ui;

use gtk4::prelude::*;
use gtk4::Application;
use ui::bridge::UidTransport;

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

    app.connect_startup(|_| {
        ui::icon::install();
        ui::icon::install_desktop_entry();
    });

    app.connect_activate(move |app| {
        // The window lives on through its own signal handlers; nothing else
        // needs to hold it.
        std::mem::forget(ui::main_window::build(app, uid_transport));
    });

    // `run_with_args::<&str>(&[])` rather than `run()`: GTK's own argv
    // parser doesn't know `--prefer-udp` (parsed by hand above) and would
    // reject it as an invalid option.
    app.run_with_args::<&str>(&[]);
}
