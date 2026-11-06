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

    // First: GStreamer would name the program after the executable, and GTK
    // reads the name when it opens the display (see `set_program_name`).
    ui::icon::set_program_name();
    init_gstreamer();

    // `--check-requirements` prints what this system lacks for each feature
    // (see `requirements`) and exits before GTK opens a display: the
    // packaging smoke tests run it. Exit status 0: nothing missing, 1: no
    // video for a codec (or none at all), 2: another feature misses
    // something.
    if std::env::args().skip(1).any(|a| a == "--check-requirements") {
        let report = ui::requirements::check();
        print!("{}", report.check_text());
        std::process::exit(report.exit_code());
    }

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
        ui::settings::refresh_autostart();
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

/// The Debian package carries its own GTK 4 paintable sink, the one Reoling
/// is built and tested with: it lands in `<prefix>/lib/reoling/gstreamer-1.0`,
/// beside `<prefix>/bin/reoling`, and `reoling` finds it from a terminal as
/// well as from the desktop entry, with no wrapper script. It is the one used
/// also where the distribution has a sink of its own (`gstreamer1.0-gtk4` on
/// Debian 13 and Ubuntu 26.04, another version) or the user's
/// `GST_PLUGIN_PATH` names one: while `gstreamer::init` scans the plugins,
/// that directory comes first on `GST_PLUGIN_PATH_1_0` (`plugin_path`), as
/// the system AppImage's hook puts its sink, and GStreamer takes a plugin file
/// name once, from the first directory that has it, whether its plugin
/// scanner or this process loads the plugins. The registry is Reoling's own
/// then (`private_registry`); with ours in it, the one other GStreamer
/// programs share would be written again at every start of Reoling and of
/// the next of them. Both variables are put back as they were right after
/// `init`, before any thread starts, so the programs Reoling starts get the
/// user's environment. Should our file fail to load, GStreamer says why on
/// stderr and takes the distribution's sink, where there is one. A source
/// build or an AppImage has no such directory and starts GStreamer as before.
fn init_gstreamer() {
    let Some(dir) = private_plugins() else {
        gstreamer::init().expect("failed to initialize GStreamer");
        return;
    };
    let registry = private_registry();
    if let Some(folder) = registry.parent() {
        let _ = std::fs::create_dir_all(folder);
    }
    let saved = [PLUGIN_PATH, REGISTRY].map(|name| (name, std::env::var_os(name)));
    let path = plugin_path(&dir, std::env::var_os(PLUGIN_PATH), std::env::var_os("GST_PLUGIN_PATH"));
    std::env::set_var(PLUGIN_PATH, path);
    std::env::set_var(REGISTRY, &registry);
    gstreamer::init().expect("failed to initialize GStreamer");
    for (name, value) in saved {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
    ui::requirements::set_private_registry(&registry);
}

/// The variables GStreamer reads for its plugin path (ahead of
/// `GST_PLUGIN_PATH`, which it reads only without this one) and for its
/// registry's file.
const PLUGIN_PATH: &str = "GST_PLUGIN_PATH_1_0";
const REGISTRY: &str = "GST_REGISTRY_1_0";

/// `<prefix>/lib/reoling/gstreamer-1.0` when it is a directory (the .deb).
fn private_plugins() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.parent()?.join("lib/reoling/gstreamer-1.0");
    dir.is_dir().then_some(dir)
}

/// The .deb's plugin registry, in Reoling's cache folder beside the
/// AppImages' and named as theirs are (`gstreamer-registry-<stack>-<GStreamer
/// version>.bin`, `deb` for the stack).
fn private_registry() -> std::path::PathBuf {
    let (major, minor, micro, _) = gstreamer::version();
    glib::user_cache_dir().join("reoling").join(format!("gstreamer-registry-deb-{major}.{minor}.{micro}.bin"))
}

/// `GST_PLUGIN_PATH_1_0` with `ours` first, then the user's own: their
/// `GST_PLUGIN_PATH_1_0` (`path_1_0`), even an empty one, else their
/// `GST_PLUGIN_PATH` (`path`), the one GStreamer would read, as the system
/// AppImage's hook does.
fn plugin_path(
    ours: &std::path::Path,
    path_1_0: Option<std::ffi::OsString>,
    path: Option<std::ffi::OsString>,
) -> std::ffi::OsString {
    let mut value = std::ffi::OsString::from(ours);
    if let Some(theirs) = path_1_0.or(path).filter(|theirs| !theirs.is_empty()) {
        value.push(":");
        value.push(theirs);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn our_plugins_come_before_the_users_own() {
        let ours = Path::new("/usr/lib/reoling/gstreamer-1.0");
        let os = |value: &str| Some(OsString::from(value));
        assert_eq!(plugin_path(ours, None, None), "/usr/lib/reoling/gstreamer-1.0");
        assert_eq!(plugin_path(ours, os("/opt/a:/opt/b"), None), "/usr/lib/reoling/gstreamer-1.0:/opt/a:/opt/b");
        assert_eq!(plugin_path(ours, None, os("/opt/c")), "/usr/lib/reoling/gstreamer-1.0:/opt/c");
        // GStreamer reads GST_PLUGIN_PATH only without GST_PLUGIN_PATH_1_0, even an empty one.
        assert_eq!(plugin_path(ours, os("/opt/a"), os("/opt/c")), "/usr/lib/reoling/gstreamer-1.0:/opt/a");
        assert_eq!(plugin_path(ours, os(""), os("/opt/c")), "/usr/lib/reoling/gstreamer-1.0");
        assert_eq!(plugin_path(ours, None, os("")), "/usr/lib/reoling/gstreamer-1.0");
    }
}
