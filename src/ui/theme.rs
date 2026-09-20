//! Makes the one button that means "do this" (Connect) use the active
//! theme's accent colour.
//!
//! Some themes (Yaru, for one) paint `suggested-action` buttons in a fixed
//! green that has nothing to do with the accent the user picked. The
//! stylesheet below therefore refers to the theme's *named* selection
//! colours (`@theme_selected_bg_color` / `@theme_selected_fg_color`, which
//! every GTK4 theme defines and which carry its accent) instead of any
//! colour written here. GTK re-resolves named colours whenever the theme or
//! its accent variant changes, so the button follows the theme live.
//!
//! No colour value appears in this file on purpose: everything in the app
//! must come from the active theme.

use gtk4::gdk::Display;

const CSS: &str = r#"
button.suggested-action {
    color: @theme_selected_fg_color;
    border-color: shade(@theme_selected_bg_color, 0.7);
    background-image: none;
    background-color: @theme_selected_bg_color;
}
button.suggested-action:hover {
    background-color: shade(@theme_selected_bg_color, 1.1);
}
button.suggested-action:active {
    background-color: shade(@theme_selected_bg_color, 0.85);
}
"#;

/// Loads the stylesheet for the whole app. Call once the display exists
/// (from the application's `startup` handler).
pub fn install() {
    let Some(display) = Display::default() else {
        return;
    };
    let provider = gtk4::CssProvider::new();
    provider.connect_parsing_error(|_, section, error| {
        eprintln!("THEME css problem at {}: {error}", section.to_str());
    });
    provider.load_from_data(CSS);
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

#[cfg(test)]
mod tests {
    use super::CSS;

    #[test]
    fn stylesheet_contains_no_literal_colours() {
        assert!(!CSS.contains('#'), "colours must come from the theme, not from here");
        assert!(!CSS.contains("rgb"));
        assert!(CSS.contains("@theme_selected_bg_color"));
    }
}
