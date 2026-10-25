mod ui;

use gtk4::prelude::*;
use gtk4::Application;
use ui::bridge::UidTransport;

fn main() {
    // `--version` answers before GTK or GStreamer come up, so it works in a
    // bare shell with no display: the packaging smoke tests run it.
    if std::env::args().skip(1).any(|a| a == "--version" || a == "-V") {
        println!("reoling {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    gstreamer::init().expect("failed to initialize GStreamer");
    scan_private_plugins();

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
        ui::icon::install_appimage_integration();
    });

    app.connect_activate(move |app| {
        // Reoling runs once: starting it again activates the running
        // instance, which shows the window it has instead of building
        // another one (that would log in to every device a second time).
        // Dialogs are plain windows; the main window is the application's.
        if let Some(window) = app.windows().into_iter().find(|w| w.is::<gtk4::ApplicationWindow>()) {
            window.present();
            return;
        }
        // The window lives on through its own signal handlers; nothing else
        // needs to hold it.
        std::mem::forget(ui::main_window::build(app, uid_transport));
    });

    // `run_with_args::<&str>(&[])` rather than `run()`: GTK's own argv
    // parser doesn't know `--prefer-udp` (parsed by hand above) and would
    // reject it as an invalid option.
    app.run_with_args::<&str>(&[]);
}

/// The Debian package carries the GTK 4 paintable sink itself when the
/// distribution has no `gstreamer1.0-gtk4` of its own (Ubuntu 24.04): it
/// lands in `<prefix>/lib/reoling/gstreamer-1.0`, beside
/// `<prefix>/bin/reoling`. That directory is added to the plugin registry
/// when it exists, so `reoling` finds the sink from a terminal as well as
/// from the desktop entry, with no wrapper script and no environment to
/// set. A source build or an AppImage has no such directory and skips this.
fn scan_private_plugins() {
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(prefix) = exe.parent().and_then(|bin| bin.parent()) else { return };
    let dir = prefix.join("lib/reoling/gstreamer-1.0");
    if dir.is_dir() {
        gstreamer::Registry::get().scan_path(&dir);
    }
}
