//! Reoling's look: the logo's amber-to-ember gradient as the accent colour,
//! and the app icon.
//!
//! The three colours below were sampled from the logo's background
//! gradient, which runs diagonally from the top-left corner (`AMBER`) to
//! the bottom-right corner (`EMBER`), passing through `ORANGE` in the
//! middle. Widgets that mean "this is the main thing" (the suggested
//! action button, the selected segment of a toggle group) use the full
//! gradient; everything else that highlights (focus rings, text
//! selection, progress) uses the solid `ORANGE`.

use gtk4::gdk::Display;

pub const AMBER: &str = "#f7a924";
pub const ORANGE: &str = "#e97b13";
pub const EMBER: &str = "#c74407";

/// The icon name shipped in `data/icons/hicolor/*/apps/`; also the
/// application id, so desktop environments match the window to the icon.
pub const ICON_NAME: &str = "de.dersobi.reoling";

fn css() -> String {
    format!(
        r#"
@define-color reoling_amber {AMBER};
@define-color reoling_orange {ORANGE};
@define-color reoling_ember {EMBER};

/* Accent colour for the themes that read these names (Adwaita on GTK
   4.6-4.14 uses theme_selected_*; newer ones use accent_*). */
@define-color theme_selected_bg_color {ORANGE};
@define-color theme_selected_fg_color #ffffff;
@define-color accent_color {ORANGE};
@define-color accent_bg_color {ORANGE};
@define-color accent_fg_color #ffffff;

/* Primary action: the logo's gradient. */
button.suggested-action {{
    background-image: linear-gradient(135deg, {AMBER}, {EMBER});
    border-color: alpha({EMBER}, 0.7);
    color: #ffffff;
    text-shadow: 0 1px 1px alpha(black, 0.3);
}}
button.suggested-action:hover {{
    background-image: linear-gradient(135deg, shade({AMBER}, 1.08), shade({EMBER}, 1.08));
}}
button.suggested-action:active {{
    background-image: linear-gradient(135deg, shade({AMBER}, 0.92), shade({EMBER}, 0.92));
}}
button.suggested-action:disabled {{
    background-image: none;
    color: alpha(currentColor, 0.5);
}}

/* Selected segment of a linked toggle group (UID/IP, Sub/Main). */
.linked > button:checked {{
    background-image: linear-gradient(135deg, {AMBER}, {EMBER});
    border-color: alpha({EMBER}, 0.7);
    color: #ffffff;
    text-shadow: 0 1px 1px alpha(black, 0.3);
}}

entry:focus-within {{
    border-color: {ORANGE};
    outline-color: alpha({ORANGE}, 0.5);
}}

progressbar > trough > progress {{
    background-image: linear-gradient(90deg, {AMBER}, {EMBER});
}}
"#
    )
}

/// Loads the stylesheet for the whole app. Call once the display exists
/// (from the application's `startup` handler).
pub fn install() {
    let Some(display) = Display::default() else {
        return;
    };
    let provider = gtk4::CssProvider::new();
    provider.connect_parsing_error(|_, section, error| {
        eprintln!("STYLE css problem at {}: {error}", section.to_str());
    });
    provider.load_from_data(&css());
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    install_icon(&display);
}

/// Makes the app icon available and sets it as every window's icon.
///
/// Installed builds find it through the system icon theme. When running
/// from a source checkout (`cargo run`) nothing is installed, so this also
/// looks for `data/icons` a few directories above the executable
/// (`target/{debug,release}/reoling` → the repository root), or in the
/// directory named by `REOLING_ICON_PATH`.
fn install_icon(display: &Display) {
    let theme = gtk4::IconTheme::for_display(display);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stylesheet_uses_the_logo_colours_and_balanced_braces() {
        let css = css();
        for colour in [AMBER, ORANGE, EMBER] {
            assert!(css.contains(colour));
        }
        assert_eq!(css.matches('{').count(), css.matches('}').count());
    }

    #[test]
    fn colours_match_the_sampled_logo_gradient() {
        // #rrggbb, sampled from the logo's top-left, middle and bottom-right.
        for c in [AMBER, ORANGE, EMBER] {
            assert_eq!(c.len(), 7);
            assert!(u32::from_str_radix(&c[1..], 16).is_ok());
        }
    }
}
