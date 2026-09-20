use crate::ui::bridge::ConnectTarget;
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Button, Entry, Grid, Label, Orientation, ToggleButton};
use reoling::StreamQuality;

fn labeled_row(grid: &Grid, row: i32, label_text: &str, entry: &Entry) {
    let label = Label::new(Some(label_text));
    label.set_halign(gtk4::Align::End);
    grid.attach(&label, 0, row, 1, 1);
    entry.set_hexpand(true);
    grid.attach(entry, 1, row, 1, 1);
}

/// Builds the "add device" form directly into `container` (rather than
/// creating and returning its own), so the caller can hold a clone of
/// `container` *before* this call to hide it later — e.g. once connected,
/// mirroring the common NVR-app pattern of a dedicated add/connect screen
/// that gives way to a live-view screen instead of both staying stacked on
/// top of each other permanently.
///
/// `on_connect` receives the device name (for display only — a nickname
/// the user chose, not sent anywhere), then the connection target (UID or
/// IP+port, per the toggle), then username/password/channel.
pub fn build_connect_dialog(
    container: &GtkBox,
    on_connect: impl Fn(String, ConnectTarget, String, String, u8, StreamQuality) + 'static,
) {
    container.set_orientation(Orientation::Vertical);
    container.set_spacing(12);
    container.set_margin_top(16);
    container.set_margin_bottom(16);
    container.set_margin_start(16);
    container.set_margin_end(16);

    let heading = Label::new(Some("Connect to a Reolink device"));
    heading.add_css_class("title-2");
    heading.set_halign(gtk4::Align::Start);
    container.append(&heading);

    // Explicit UID/IP choice — never auto-detected or falls back silently
    // from one to the other (project decision, 2026-09-13: a user who
    // picks "IP" and mistypes it should see a connection failure, not have
    // the app quietly retry over P2P).
    let toggle_box = GtkBox::new(Orientation::Horizontal, 0);
    toggle_box.add_css_class("linked");
    let uid_toggle = ToggleButton::with_label("Connect by UID");
    let ip_toggle = ToggleButton::with_label("Connect by IP");
    ip_toggle.set_group(Some(&uid_toggle));
    uid_toggle.set_active(true);
    toggle_box.append(&uid_toggle);
    toggle_box.append(&ip_toggle);
    container.append(&toggle_box);

    let grid = Grid::new();
    grid.set_row_spacing(8);
    grid.set_column_spacing(12);

    let name_entry = Entry::builder().placeholder_text("e.g. Front Door NVR").build();
    let uid_entry = Entry::builder().placeholder_text("e.g. 9527000EXAMPLE01").build();
    let ip_entry = Entry::builder().placeholder_text("e.g. 192.168.1.50").build();
    let port_entry = Entry::builder().text("9000").build();
    let user_entry = Entry::builder().placeholder_text("admin").build();
    let pass_entry = Entry::builder().visibility(false).build();
    let channel_entry = Entry::builder().text("0").build();

    labeled_row(&grid, 0, "Device name", &name_entry);
    let uid_label = Label::new(Some("UID"));
    uid_label.set_halign(gtk4::Align::End);
    grid.attach(&uid_label, 0, 1, 1, 1);
    uid_entry.set_hexpand(true);
    grid.attach(&uid_entry, 1, 1, 1, 1);
    let ip_label = Label::new(Some("IP address"));
    ip_label.set_halign(gtk4::Align::End);
    grid.attach(&ip_label, 0, 2, 1, 1);
    ip_entry.set_hexpand(true);
    grid.attach(&ip_entry, 1, 2, 1, 1);
    let port_label = Label::new(Some("Port"));
    port_label.set_halign(gtk4::Align::End);
    grid.attach(&port_label, 0, 3, 1, 1);
    port_entry.set_hexpand(true);
    grid.attach(&port_entry, 1, 3, 1, 1);
    labeled_row(&grid, 4, "Username", &user_entry);
    labeled_row(&grid, 5, "Password", &pass_entry);
    labeled_row(&grid, 6, "Channel", &channel_entry);
    container.append(&grid);

    // Stream quality — mirrors the picker every official Reolink app (and
    // `leolink`) exposes. Sub stream defaults on: over a relay/internet
    // path (vs. direct LAN/VPN) the camera's full main-stream bitrate
    // routinely outruns what the link can keep up with in real time.
    let quality_label = Label::new(Some("Stream quality"));
    quality_label.set_halign(gtk4::Align::Start);
    container.append(&quality_label);
    let quality_box = GtkBox::new(Orientation::Horizontal, 0);
    quality_box.add_css_class("linked");
    let sub_toggle = ToggleButton::with_label("Sub stream (smoother)");
    let main_toggle = ToggleButton::with_label("Main stream (full res)");
    main_toggle.set_group(Some(&sub_toggle));
    sub_toggle.set_active(true);
    quality_box.append(&sub_toggle);
    quality_box.append(&main_toggle);
    container.append(&quality_box);

    // IP mode starts hidden (UID is the default-active toggle above);
    // hidden widgets in a Grid collapse to zero height, so this doesn't
    // leave a visible gap.
    ip_label.set_visible(false);
    ip_entry.set_visible(false);
    port_label.set_visible(false);
    port_entry.set_visible(false);

    let uid_label_for_toggle = uid_label.clone();
    let uid_entry_for_toggle = uid_entry.clone();
    let ip_label_for_toggle = ip_label.clone();
    let ip_entry_for_toggle = ip_entry.clone();
    let port_label_for_toggle = port_label.clone();
    let port_entry_for_toggle = port_entry.clone();
    uid_toggle.connect_toggled(move |btn| {
        let uid_mode = btn.is_active();
        uid_label_for_toggle.set_visible(uid_mode);
        uid_entry_for_toggle.set_visible(uid_mode);
        ip_label_for_toggle.set_visible(!uid_mode);
        ip_entry_for_toggle.set_visible(!uid_mode);
        port_label_for_toggle.set_visible(!uid_mode);
        port_entry_for_toggle.set_visible(!uid_mode);
    });

    let status_label = Label::new(None);
    status_label.set_halign(gtk4::Align::Start);
    container.append(&status_label);

    let connect_button = Button::with_label("Connect");
    connect_button.add_css_class("suggested-action");
    connect_button.set_halign(gtk4::Align::End);
    container.append(&connect_button);

    let name_entry_c = name_entry.clone();
    let uid_toggle_c = uid_toggle.clone();
    let uid_entry_c = uid_entry.clone();
    let ip_entry_c = ip_entry.clone();
    let port_entry_c = port_entry.clone();
    let user_entry_c = user_entry.clone();
    let pass_entry_c = pass_entry.clone();
    let channel_entry_c = channel_entry.clone();
    let sub_toggle_c = sub_toggle.clone();
    let status_label_c = status_label.clone();
    connect_button.connect_clicked(move |_| {
        status_label_c.set_text("");
        let channel_id: u8 = channel_entry_c.text().parse().unwrap_or(0);
        let quality =
            if sub_toggle_c.is_active() { StreamQuality::Sub } else { StreamQuality::Main };
        let target = if uid_toggle_c.is_active() {
            ConnectTarget::Uid(uid_entry_c.text().to_string())
        } else {
            let addr = match ip_entry_c.text().parse() {
                Ok(addr) => addr,
                Err(_) => {
                    status_label_c.set_text("Invalid IP address");
                    return;
                }
            };
            let port: u16 = match port_entry_c.text().parse() {
                Ok(0) | Err(_) => {
                    status_label_c.set_text("Invalid port");
                    return;
                }
                Ok(port) => port,
            };
            ConnectTarget::Ip { addr, port }
        };
        on_connect(
            name_entry_c.text().to_string(),
            target,
            user_entry_c.text().to_string(),
            pass_entry_c.text().to_string(),
            channel_id,
            quality,
        );
    });
}
