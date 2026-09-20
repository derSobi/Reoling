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
