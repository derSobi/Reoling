//! Modal dialogs: add a device, log in to a device.

use crate::ui::bridge::ConnectTarget;
use crate::ui::settings::{self, Decoding, Latency, Settings, Theme};
use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, CheckButton, DropDown, Entry, Grid, Label, Notebook, Orientation,
    PasswordEntry, StringList, Window,
};

fn dialog(parent: &Window, title: &str) -> (Window, GtkBox) {
    let window = Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(title)
        .resizable(false)
        .default_width(380)
        .build();
    let content = GtkBox::new(Orientation::Vertical, 12);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    window.set_child(Some(&content));
    (window, content)
}

fn row(grid: &Grid, row: i32, label: &str, widget: &impl IsA<gtk4::Widget>) {
    let label = Label::new(Some(label));
    label.set_halign(gtk4::Align::End);
    grid.attach(&label, 0, row, 1, 1);
    widget.set_hexpand(true);
    grid.attach(widget, 1, row, 1, 1);
}

fn button_row(cancel: &Button, accept: &Button) -> GtkBox {
    let buttons = GtkBox::new(Orientation::Horizontal, 8);
    buttons.set_halign(gtk4::Align::End);
    accept.add_css_class("suggested-action");
    buttons.append(cancel);
    buttons.append(accept);
    buttons
}

/// "Add device": a UID tab and an IP tab. The device's name is read from the
/// device itself once connected.
pub fn add_device(parent: &Window, on_add: impl Fn(ConnectTarget) + 'static) {
    let (window, content) = dialog(parent, "Add device");

    let notebook = Notebook::new();
    let uid = Entry::builder().placeholder_text("Device UID").build();
    let uid_grid = Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    row(&uid_grid, 0, "UID", &uid);
    notebook.append_page(&uid_grid, Some(&Label::new(Some("UID"))));

    let ip = Entry::builder().placeholder_text("192.168.1.50").build();
    let port = Entry::builder().text("9000").build();
    let ip_grid = Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    row(&ip_grid, 0, "IP address", &ip);
    row(&ip_grid, 1, "Port", &port);
    notebook.append_page(&ip_grid, Some(&Label::new(Some("IP address"))));
    content.append(&notebook);

    let error = Label::new(None);
    error.set_halign(gtk4::Align::Start);
    error.add_css_class("error");
    content.append(&error);

    let cancel = Button::with_label("Cancel");
    let add = Button::with_label("Add");
    content.append(&button_row(&cancel, &add));
    window.set_default_widget(Some(&add));
    uid.set_activates_default(true);
    ip.set_activates_default(true);
    port.set_activates_default(true);

    let w = window.clone();
    cancel.connect_clicked(move |_| w.close());

    let w = window.clone();
    add.connect_clicked(move |_| {
        let target = if notebook.current_page() == Some(0) {
            let uid = uid.text().trim().to_string();
            if uid.is_empty() {
                error.set_text("Enter the device UID");
                return;
            }
            ConnectTarget::Uid(uid)
        } else {
            let Ok(addr) = ip.text().trim().parse() else {
                error.set_text("Invalid IP address");
                return;
            };
            let port = match port.text().trim().parse() {
                Ok(p) if p != 0 => p,
                _ => {
                    error.set_text("Invalid port");
                    return;
                }
            };
            ConnectTarget::Ip { addr, port }
        };
        on_add(target);
        w.close();
    });

    window.present();
}

/// The device's login: username, password (with a reveal toggle) and whether
/// to keep the password in the keyring. `on_login(username, password, remember)`.
pub fn login(
    parent: &Window,
    device_name: &str,
    username: &str,
    on_login: impl Fn(String, String, bool) + 'static,
) {
    let (window, content) = dialog(parent, device_name);

    let heading = Label::new(Some(device_name));
    heading.add_css_class("title-3");
    heading.set_halign(gtk4::Align::Start);
    content.append(&heading);

    let user = Entry::builder().text(username).build();
    let password = PasswordEntry::builder().show_peek_icon(true).build();
    let grid = Grid::builder().row_spacing(8).column_spacing(12).build();
    row(&grid, 0, "Username", &user);
    row(&grid, 1, "Password", &password);
    content.append(&grid);

    let remember = CheckButton::with_label("Remember password");
    remember.set_active(true);
    content.append(&remember);

    let cancel = Button::with_label("Cancel");
    let login = Button::with_label("Log in");
    content.append(&button_row(&cancel, &login));
    window.set_default_widget(Some(&login));
    user.set_activates_default(true);
    password.set_activates_default(true);

    let w = window.clone();
    cancel.connect_clicked(move |_| w.close());
    let w = window.clone();
    let password_for_click = password.clone();
    login.connect_clicked(move |_| {
        let password = &password_for_click;
        on_login(user.text().to_string(), password.text().to_string(), remember.is_active());
        w.close();
    });

    window.present();
    password.grab_focus();
}

/// The application settings. Every change is applied at once through
/// `on_change`, which gets the full settings.
pub fn settings(parent: &Window, current: &Settings, on_change: impl Fn(Settings) + 'static) {
    let (window, content) = dialog(parent, "Settings");
    window.set_default_width(420);

    let heading = |text: &str| {
        let l = Label::new(Some(text));
        l.add_css_class("heading");
        l.set_halign(gtk4::Align::Start);
        l
    };

    let grid = Grid::builder().row_spacing(10).column_spacing(12).build();
    content.append(&heading("Appearance"));
    let theme = DropDown::new(Some(StringList::new(&["Auto", "Light", "Dark"])), None::<gtk4::Expression>);
    theme.set_selected(match current.theme {
        Theme::Auto => 0,
        Theme::Light => 1,
        Theme::Dark => 2,
    });
    row(&grid, 0, "Theme", &theme);
    content.append(&grid);

    content.append(&heading("Video"));
    let hardware = settings::hardware_decoders();
    // Without any hardware decoder there is nothing to choose to force.
    let modes: Vec<(Decoding, &str)> = [(Decoding::Auto, "Auto"), (Decoding::Hardware, "Hardware"), (Decoding::Software, "Software")]
        .into_iter()
        .filter(|(m, _)| *m != Decoding::Hardware || !hardware.is_empty())
        .collect();
    let labels: Vec<&str> = modes.iter().map(|(_, l)| *l).collect();
    let decoding = DropDown::new(Some(StringList::new(&labels)), None::<gtk4::Expression>);
    decoding.set_selected(modes.iter().position(|(m, _)| *m == current.decoding).unwrap_or(0) as u32);

    let mut device_labels = vec!["Automatic".to_string()];
    device_labels.extend(hardware.iter().map(|d| d.label.clone()));
    let device_labels: Vec<&str> = device_labels.iter().map(String::as_str).collect();
    let device = DropDown::new(Some(StringList::new(&device_labels)), None::<gtk4::Expression>);
    device.set_selected(
        current
            .hardware_decoder
            .as_ref()
            .and_then(|k| hardware.iter().position(|d| &d.key == k))
            .map_or(0, |i| i as u32 + 1),
    );
    let video_grid = Grid::builder().row_spacing(10).column_spacing(12).build();
    row(&video_grid, 0, "Decoding", &decoding);
    let latency = DropDown::new(
        Some(StringList::new(&["Low (0.3 s)", "Balanced (1 s)", "Smooth (3 s)"])),
        None::<gtk4::Expression>,
    );
    latency.set_selected(match current.latency {
        Latency::Low => 0,
        Latency::Balanced => 1,
        Latency::Smooth => 2,
    });
    let latency_label = Label::new(Some("Latency"));
    latency_label.set_halign(gtk4::Align::End);
    video_grid.attach(&latency_label, 0, 2, 1, 1);
    latency.set_hexpand(true);
    video_grid.attach(&latency, 1, 2, 1, 1);
    let device_label = Label::new(Some("Decoder"));
    device_label.set_halign(gtk4::Align::End);
    video_grid.attach(&device_label, 0, 1, 1, 1);
    device.set_hexpand(true);
    video_grid.attach(&device, 1, 1, 1, 1);
    content.append(&video_grid);

    let note = Label::new(Some("Decoding and latency apply to streams started afterwards. A shorter latency shows camera movement sooner but rides out a poor connection less well."));
    note.add_css_class("dim-label");
    note.set_halign(gtk4::Align::Start);
    content.append(&note);

    // The decoder choice only matters with hardware decoding and, being a
    // choice, only when there is more than one.
    let show_device = {
        let (decoding, device, device_label) = (decoding.clone(), device.clone(), device_label.clone());
        let modes = modes.clone();
        let count = hardware.len();
        move || {
            let hw = modes.get(decoding.selected() as usize).map(|(m, _)| *m) == Some(Decoding::Hardware);
            device.set_visible(hw && count > 1);
            device_label.set_visible(hw && count > 1);
        }
    };
    show_device();

    let emit = {
        let (theme, decoding, device, latency) = (theme.clone(), decoding.clone(), device.clone(), latency.clone());
        let base = current.clone();
        move || {
            let mode = modes.get(decoding.selected() as usize).map(|(m, _)| *m).unwrap_or_default();
            on_change(Settings {
                theme: match theme.selected() {
                    1 => Theme::Light,
                    2 => Theme::Dark,
                    _ => Theme::Auto,
                },
                decoding: mode,
                latency: match latency.selected() {
                    0 => Latency::Low,
                    2 => Latency::Smooth,
                    _ => Latency::Balanced,
                },
                hardware_decoder: (device.selected() > 0)
                    .then(|| hardware.get(device.selected() as usize - 1).map(|d| d.key.clone()))
                    .flatten(),
                ..base.clone()
            });
        }
    };
    let emit = std::rc::Rc::new(emit);
    for dd in [&theme, &decoding, &device, &latency] {
        let emit = std::rc::Rc::clone(&emit);
        let show_device = show_device.clone();
        dd.connect_selected_notify(move |_| {
            show_device();
            emit();
        });
    }

    let close = Button::with_label("Close");
    close.set_halign(gtk4::Align::End);
    let w = window.clone();
    close.connect_clicked(move |_| w.close());
    content.append(&close);

    window.present();
}
