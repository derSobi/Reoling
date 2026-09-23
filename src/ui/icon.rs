//! The app icon. Colours and widget styling are left entirely to the
//! active GTK theme (including its accent colour); Reoling ships no
//! stylesheet of its own.

use gtk4::gdk::Display;

/// The icon name shipped in `data/icons/hicolor/*/apps/`; also the
/// application id, so desktop environments match the window to the icon.
pub const ICON_NAME: &str = "de.dersobi.reoling";

/// Makes the app icon available and sets it as every window's icon. Call
/// once the display exists (from the application's `startup` handler).
///
/// Installed builds find the icon through the system icon theme. When
/// running from a source checkout (`cargo run`) nothing is installed, so
/// this also looks for `data/icons` a few directories above the executable
/// (`target/{debug,release}/reoling` → the repository root), or in the
/// directory named by `REOLING_ICON_PATH`.
pub fn install() {
    let Some(display) = Display::default() else {
        return;
    };
    let theme = gtk4::IconTheme::for_display(&display);
    if let Some(dir) = std::env::var_os("REOLING_ICON_PATH") {
        theme.add_search_path(dir);
    }
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1).take(4) {
            let candidate = ancestor.join("data/icons");
            if candidate.join("hicolor").is_dir() {
                theme.add_search_path(candidate);
                break;
            }
        }
    }
    gtk4::Window::set_default_icon_name(ICON_NAME);
}

/// A source checkout (`cargo run`) has no installed desktop entry, and the
/// dock then shows the bare application id and no icon. Writes a per-user
/// entry (named "Reoling", pointing at this executable and the checkout's
/// icon) unless the system already has one; an installed build ships its own.
pub fn install_desktop_entry() {
    let file = "de.dersobi.reoling.desktop";
    let system_has_it = glib::system_data_dirs().iter().any(|d| d.join("applications").join(file).is_file());
    let Ok(exe) = std::env::current_exe() else { return };
    if system_has_it || !exe.ancestors().any(|a| a.join("data/icons/hicolor").is_dir()) {
        return;
    }
    let icon = exe
        .ancestors()
        .map(|a| a.join("data/icons/hicolor/256x256/apps/de.dersobi.reoling.png"))
        .find(|p| p.is_file())
        .map_or_else(|| ICON_NAME.to_string(), |p| p.display().to_string());
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=Reoling\nGenericName=Reolink client\nComment=Unofficial client for Reolink cameras, NVRs and Home Hubs\nExec=\"{}\"\nIcon={icon}\nTerminal=false\nCategories=AudioVideo;Video;\nStartupWMClass={ICON_NAME}\n",
        exe.display()
    );
    let path = glib::user_data_dir().join("applications").join(file);
    if std::fs::read_to_string(&path).is_ok_and(|current| current == entry) {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, entry) {
        eprintln!("could not write the desktop entry: {e}");
    }
}
