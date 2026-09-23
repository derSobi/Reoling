//! "Client Settings": a sidebar (System Settings, Download Settings, Local
//! Record, About) beside the page it selects, laid out after the official
//! app's own window (`.plans/official_app_screenshots/Settings`). Rows the
//! official app has but Reoling cannot act on yet are shown disabled, so the
//! window looks the same and it is plain what is missing.

use crate::ui::settings::{self, Decoding, Latency, Settings, Theme};
use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, DropDown, Image, Label, ListBox, ListBoxRow, Notebook, Orientation,
    ScrolledWindow, SelectionMode, Separator, Stack, StringList, Switch, Window,
};
use std::cell::RefCell;
use std::rc::Rc;

/// Applies one edit to the working copy of the settings and reports it.
type Commit = Rc<dyn Fn(&dyn Fn(&mut Settings))>;

const NOT_AVAILABLE: &str = "Not available in Reoling yet";

/// One settings row: a title (with an optional explanation under it) and a
/// control at the end. `available: false` greys the control out.
fn row(title: &str, subtitle: Option<&str>, control: &impl IsA<gtk4::Widget>, available: bool) -> ListBoxRow {
    let text = GtkBox::new(Orientation::Vertical, 2);
    text.set_hexpand(true);
    text.set_valign(gtk4::Align::Center);
    let heading = Label::new(Some(title));
    heading.set_halign(gtk4::Align::Start);
    text.append(&heading);
    if let Some(subtitle) = subtitle {
        let sub = Label::new(Some(subtitle));
        sub.add_css_class("dim-label");
        sub.add_css_class("caption");
        sub.set_halign(gtk4::Align::Start);
        sub.set_wrap(true);
        sub.set_xalign(0.0);
        sub.set_max_width_chars(60);
        text.append(&sub);
    }
    let line = GtkBox::new(Orientation::Horizontal, 12);
    line.set_margin_top(10);
    line.set_margin_bottom(10);
    line.set_margin_start(12);
    line.set_margin_end(12);
    control.set_valign(gtk4::Align::Center);
    line.append(&text);
    line.append(control);
    if !available {
        line.set_sensitive(false);
        line.set_tooltip_text(Some(NOT_AVAILABLE));
    }
    let row = ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);
    row.set_child(Some(&line));
    row
}

fn list(rows: &[ListBoxRow]) -> ListBox {
    let list = ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(SelectionMode::None);
    for r in rows {
        list.append(r);
    }
    list
}

fn page(title: &str) -> GtkBox {
    let page = GtkBox::new(Orientation::Vertical, 14);
    page.set_margin_top(18);
    page.set_margin_bottom(18);
    page.set_margin_start(20);
    page.set_margin_end(20);
    let heading = Label::new(Some(title));
    heading.add_css_class("title-2");
    heading.set_halign(gtk4::Align::Start);
    page.append(&heading);
    page
}

fn scrolled(child: &impl IsA<gtk4::Widget>) -> ScrolledWindow {
    let scroll = ScrolledWindow::new();
    scroll.set_child(Some(child));
    scroll.set_hexpand(true);
    scroll.set_vexpand(true);
    scroll
}

fn switch(on: bool) -> Switch {
    let s = Switch::new();
    s.set_active(on);
    s
}

fn drop_down(options: &[&str], selected: u32) -> DropDown {
    let d = DropDown::new(Some(StringList::new(options)), None::<gtk4::Expression>);
    d.set_selected(selected);
    d
}

/// A folder row: the path under the title, "Browse" to pick another, and a
/// button that opens the folder in the file manager.
fn folder_row(
    parent: &Window,
    title: &str,
    subtitle: Option<&str>,
    current: &str,
    available: bool,
    on_pick: impl Fn(String) + 'static,
) -> ListBoxRow {
    let controls = GtkBox::new(Orientation::Horizontal, 8);
    let browse = Button::with_label("Browse");
    browse.add_css_class("suggested-action");
    let open = Button::from_icon_name("folder-symbolic");
    open.set_tooltip_text(Some("Open the folder"));
    controls.append(&browse);
    controls.append(&open);

    let path = Rc::new(RefCell::new(current.to_string()));
    let shown = Label::new(Some(current));
    shown.add_css_class("dim-label");
    shown.add_css_class("caption");
    shown.set_halign(gtk4::Align::Start);
    shown.set_xalign(0.0);
    shown.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    shown.set_max_width_chars(48);

    let text = GtkBox::new(Orientation::Vertical, 2);
    text.set_hexpand(true);
    let heading = Label::new(Some(title));
    heading.set_halign(gtk4::Align::Start);
    text.append(&heading);
    if let Some(subtitle) = subtitle {
        let sub = Label::new(Some(subtitle));
        sub.add_css_class("dim-label");
        sub.add_css_class("caption");
        sub.set_halign(gtk4::Align::Start);
        text.append(&sub);
    }
    text.append(&shown);

    let line = GtkBox::new(Orientation::Horizontal, 12);
    line.set_margin_top(10);
    line.set_margin_bottom(10);
    line.set_margin_start(12);
    line.set_margin_end(12);
    controls.set_valign(gtk4::Align::Center);
    line.append(&text);
    line.append(&controls);
    if !available {
        line.set_sensitive(false);
        line.set_tooltip_text(Some(NOT_AVAILABLE));
    }

    let p = Rc::clone(&path);
    open.connect_clicked(move |_| {
        let uri = glib::filename_to_uri(&*p.borrow(), None).ok();
        if let Some(uri) = uri {
            let _ = gtk4::gio::AppInfo::launch_default_for_uri(&uri, None::<&gtk4::gio::AppLaunchContext>);
        }
    });
    let parent = parent.clone();
    let on_pick = Rc::new(on_pick);
    browse.connect_clicked(move |_| {
        let chooser = gtk4::FileChooserNative::new(
            Some("Choose a folder"),
            Some(&parent),
            gtk4::FileChooserAction::SelectFolder,
            Some("Select"),
            Some("Cancel"),
        );
        let (path, shown, on_pick) = (Rc::clone(&path), shown.clone(), Rc::clone(&on_pick));
        let keep = chooser.clone();
        chooser.connect_response(move |_, response| {
            if response == gtk4::ResponseType::Accept {
                if let Some(folder) = keep.file().and_then(|f| f.path()) {
                    let folder = folder.display().to_string();
                    shown.set_text(&folder);
                    *path.borrow_mut() = folder.clone();
                    on_pick(folder);
                }
            }
        });
        chooser.show();
    });

    let row = ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);
    row.set_child(Some(&line));
    row
}

fn default_dir(kind: glib::UserDirectory) -> String {
    glib::user_special_dir(kind)
        .map(|d| d.join("Reoling").display().to_string())
        .unwrap_or_else(|| "~/Reoling".to_string())
}

pub fn open(parent: &Window, current: &Settings, on_change: impl Fn(Settings) + 'static) {
    let window = Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Client Settings")
        .default_width(760)
        .default_height(700)
        .build();

    // Every control edits this one copy and reports the whole of it.
    let state = Rc::new(RefCell::new(current.clone()));
    let commit: Commit = Rc::new(move |edit| {
        let mut s = state.borrow_mut();
        edit(&mut s);
        on_change(s.clone());
    });

    let stack = Stack::new();

    // --- System Settings ---------------------------------------------------
    let system = page("System Settings");
    let tabs = Notebook::new();

    let startup = switch(settings::autostart_enabled());
    startup.connect_state_set(|_, on| {
        settings::set_autostart(on);
        glib::Propagation::Proceed
    });
    let theme = drop_down(
        &["Auto", "Light", "Dark"],
        match current.theme {
            Theme::Auto => 0,
            Theme::Light => 1,
            Theme::Dark => 2,
        },
    );
    let c = Rc::clone(&commit);
    theme.connect_selected_notify(move |d| {
        let theme = match d.selected() {
            1 => Theme::Light,
            2 => Theme::Dark,
            _ => Theme::Auto,
        };
        c(&|s| s.theme = theme);
    });
    let status_button = Button::with_label("View");
    status_button.add_css_class("flat");
    let w = window.clone();
    status_button.connect_clicked(move |_| system_status(&w));
    let general = GtkBox::new(Orientation::Vertical, 0);
    general.set_margin_top(14);
    general.append(&list(&[
        row("Run at Startup", None, &startup, true),
        row("Automatic Client Update", None, &switch(false), false),
        row("Add Devices Automatically", None, &switch(true), false),
        row("Lockscreen Password", None, &switch(false), false),
        row("Date Format", None, &drop_down(&["YYYY/MM/DD"], 0), false),
        row("Language", None, &drop_down(&["Auto"], 0), false),
        row("Theme", None, &theme, true),
        row("System Status", None, &status_button, true),
    ]));
    tabs.append_page(&scrolled(&general), Some(&Label::new(Some("General Settings"))));

    let auto_live = switch(current.auto_live_view);
    let c = Rc::clone(&commit);
    auto_live.connect_state_set(move |_, on| {
        c(&|s| s.auto_live_view = on);
        glib::Propagation::Proceed
    });
    let stretch = switch(current.stretch);
    let c = Rc::clone(&commit);
    stretch.connect_state_set(move |_, on| {
        c(&|s| s.stretch = on);
        glib::Propagation::Proceed
    });

    let hardware = settings::hardware_decoders();
    // Without any hardware decoder there is nothing to choose to force.
    let modes: Vec<(Decoding, &str)> =
        [(Decoding::Auto, "Auto"), (Decoding::Hardware, "Hardware"), (Decoding::Software, "Software")]
            .into_iter()
            .filter(|(m, _)| *m != Decoding::Hardware || !hardware.is_empty())
            .collect();
    let labels: Vec<&str> = modes.iter().map(|(_, l)| *l).collect();
    let decoding = drop_down(&labels, modes.iter().position(|(m, _)| *m == current.decoding).unwrap_or(0) as u32);
    let mut device_labels = vec!["Automatic".to_string()];
    device_labels.extend(hardware.iter().map(|d| d.label.clone()));
    let device_labels: Vec<&str> = device_labels.iter().map(String::as_str).collect();
    let device = drop_down(
        &device_labels,
        current.hardware_decoder.as_ref().and_then(|k| hardware.iter().position(|d| &d.key == k)).map_or(0, |i| i as u32 + 1),
    );
    let device_row = row("Decoder", Some("Which hardware decoder to use"), &device, true);
    let show_device = {
        let (decoding, device_row) = (decoding.clone(), device_row.clone());
        let modes = modes.clone();
        let count = hardware.len();
        move || {
            let hw = modes.get(decoding.selected() as usize).map(|(m, _)| *m) == Some(Decoding::Hardware);
            device_row.set_visible(hw && count > 1);
        }
    };
    show_device();
    let c = Rc::clone(&commit);
    let modes_for_change = modes.clone();
    decoding.connect_selected_notify(move |d| {
        show_device();
        let mode = modes_for_change.get(d.selected() as usize).map(|(m, _)| *m).unwrap_or_default();
        c(&|s| s.decoding = mode);
    });
    let c = Rc::clone(&commit);
    device.connect_selected_notify(move |d| {
        let key = (d.selected() > 0).then(|| hardware.get(d.selected() as usize - 1).map(|x| x.key.clone())).flatten();
        c(&|s| s.hardware_decoder = key.clone());
    });
    let latency = drop_down(
        &["Low (0.3 s)", "Balanced (1 s)", "Smooth (3 s)"],
        match current.latency {
            Latency::Low => 0,
            Latency::Balanced => 1,
            Latency::Smooth => 2,
        },
    );
    let c = Rc::clone(&commit);
    latency.connect_selected_notify(move |d| {
        let latency = match d.selected() {
            0 => Latency::Low,
            2 => Latency::Smooth,
            _ => Latency::Balanced,
        };
        c(&|s| s.latency = latency);
    });
    let live = GtkBox::new(Orientation::Vertical, 0);
    live.set_margin_top(14);
    live.append(&list(&[
        row("Auto Live View", Some("Start the last watched device as soon as it connects."), &auto_live, true),
        row(
            "Display Detection Overlays",
            Some("Always show your custom detection zones, lines and markers on the Live View screen."),
            &switch(false),
            false,
        ),
        row("Stretch Mode", Some("Fill the whole view with the picture instead of keeping its proportions."), &stretch, true),
        row("Scrollview Time", None, &drop_down(&["10secs"], 0), false),
        row("Decoding", Some("Applies to streams started afterwards."), &decoding, true),
        device_row,
        row(
            "Latency",
            Some("Shorter shows camera movement sooner; longer rides out a poor connection better."),
            &latency,
            true,
        ),
    ]));
    tabs.append_page(&scrolled(&live), Some(&Label::new(Some("Live View & Playback"))));

    let alarm = GtkBox::new(Orientation::Vertical, 0);
    alarm.set_margin_top(14);
    alarm.append(&list(&[row("Alarm Beep", None, &switch(false), false)]));
    tabs.append_page(&scrolled(&alarm), Some(&Label::new(Some("Alarm Settings"))));

    tabs.set_vexpand(true);
    system.append(&tabs);
    stack.add_named(&system, Some("system"));

    // --- Download Settings ---------------------------------------------------
    let download = page("Download Settings");
    let (pictures, videos) = (default_dir(glib::UserDirectory::Pictures), default_dir(glib::UserDirectory::Videos));
    let temp = glib::tmp_dir().join("reoling").display().to_string();
    let downloads = glib::user_special_dir(glib::UserDirectory::Downloads)
        .map(|d| d.join("Reoling").display().to_string())
        .unwrap_or_else(|| "~/Downloads/Reoling".to_string());
    let c1 = Rc::clone(&commit);
    let c2 = Rc::clone(&commit);
    download.append(&list(&[
        folder_row(&window, "Temporary Folder", None, &temp, false, |_| {}),
        folder_row(
            &window,
            "Download Path",
            Some("Set the storage path for playback and time-lapse files."),
            &downloads,
            false,
            |_| {},
        ),
        folder_row(
            &window,
            "Save Screenshot to",
            None,
            current.screenshot_dir.as_deref().unwrap_or(&pictures),
            true,
            move |dir| c1(&|s| s.screenshot_dir = Some(dir.clone())),
        ),
        folder_row(
            &window,
            "Save Screen Recording File to",
            None,
            current.recording_dir.as_deref().unwrap_or(&videos),
            true,
            move |dir| c2(&|s| s.recording_dir = Some(dir.clone())),
        ),
    ]));
    stack.add_named(&scrolled(&download), Some("download"));

    // --- Local Record ----------------------------------------------------------
    let local = page("Local Record");
    local.append(&list(&[
        row(
            "Local Record",
            Some("Auto-saving recorded videos to the computer requires adequate disk space. It is recommended to use this function within the LAN."),
            &switch(false),
            false,
        ),
        folder_row(&window, "Save Local Recording File to", None, &videos, false, |_| {}),
    ]));
    stack.add_named(&scrolled(&local), Some("local"));

    // --- Sidebar -------------------------------------------------------------------
    let nav = ListBox::new();
    nav.add_css_class("navigation-sidebar");
    nav.set_selection_mode(SelectionMode::Single);
    let pages = [("System Settings", "system"), ("Download Settings", "download"), ("Local Record", "local")];
    for (title, _) in pages {
        let label = Label::new(Some(title));
        label.set_halign(gtk4::Align::Start);
        label.set_margin_top(8);
        label.set_margin_bottom(8);
        label.set_margin_start(8);
        nav.append(&label);
    }
    let s = stack.clone();
    nav.connect_row_selected(move |_, row| {
        if let Some(row) = row {
            if let Some((_, name)) = pages.get(row.index() as usize) {
                s.set_visible_child_name(name);
            }
        }
    });
    if let Some(first) = nav.row_at_index(0) {
        nav.select_row(Some(&first));
    }
    let about = Button::with_label("About Reoling");
    about.add_css_class("flat");
    about.set_margin_bottom(10);
    let w = window.clone();
    about.connect_clicked(move |_| about_window(&w));

    let sidebar = GtkBox::new(Orientation::Vertical, 0);
    sidebar.set_width_request(200);
    sidebar.set_margin_top(12);
    sidebar.set_margin_start(8);
    sidebar.set_margin_end(8);
    nav.set_vexpand(true);
    sidebar.append(&nav);
    sidebar.append(&about);

    let body = GtkBox::new(Orientation::Horizontal, 0);
    body.append(&sidebar);
    body.append(&Separator::new(Orientation::Vertical));
    body.append(&stack);
    window.set_child(Some(&body));
    window.present();
}

/// What the official app's "System Status" shows: a plain text report of
/// what the client runs on.
fn system_status(parent: &Window) {
    let mut lines = vec![
        format!("Reoling {}", env!("CARGO_PKG_VERSION")),
        format!(
            "GTK {}.{}.{}",
            gtk4::major_version(),
            gtk4::minor_version(),
            gtk4::micro_version()
        ),
        gstreamer::version_string().to_string(),
    ];
    let hardware = settings::hardware_decoders();
    if hardware.is_empty() {
        lines.push("Hardware decoders: none found".to_string());
    } else {
        lines.push(format!(
            "Hardware decoders: {}",
            hardware.iter().map(|d| d.label.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    lines.push(format!("Settings: {}", glib::user_config_dir().join("reoling").join("settings.ini").display()));
    let text = Label::new(Some(&lines.join("\n")));
    text.set_selectable(true);
    // A selectable label takes the focus, and with it a select-all, when the
    // window opens; clicking the text still selects and copies.
    text.set_focusable(false);
    text.set_halign(gtk4::Align::Start);
    text.set_xalign(0.0);
    text.set_wrap(true);

    let close = Button::with_label("Confirm");
    close.add_css_class("suggested-action");
    close.set_halign(gtk4::Align::End);
    let content = GtkBox::new(Orientation::Vertical, 14);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    let heading = Label::new(Some("System Status"));
    heading.add_css_class("heading");
    heading.set_halign(gtk4::Align::Start);
    content.append(&heading);
    content.append(&text);
    content.append(&close);
    let window = Window::builder().transient_for(parent).modal(true).title("System Status").default_width(420).build();
    window.set_child(Some(&content));
    let w = window.clone();
    close.connect_clicked(move |_| w.close());
    window.set_focus(Some(&close));
    window.present();
}

/// The "About" window: name, version, where to follow and support the
/// project, and the legal lines.
fn about_window(parent: &Window) {
    let open_uri = |uri: &'static str| {
        move |_: &Button| {
            let _ = gtk4::gio::AppInfo::launch_default_for_uri(uri, None::<&gtk4::gio::AppLaunchContext>);
        }
    };
    let link = |title: &str, note: &str, uri: &'static str| {
        let button = Button::new();
        button.add_css_class("flat");
        let line = GtkBox::new(Orientation::Horizontal, 8);
        let title = Label::new(Some(title));
        title.set_halign(gtk4::Align::Start);
        title.set_hexpand(true);
        let note = Label::new(Some(note));
        note.add_css_class("dim-label");
        line.append(&title);
        line.append(&note);
        line.append(&Image::from_icon_name("go-next-symbolic"));
        button.set_child(Some(&line));
        button.connect_clicked(open_uri(uri));
        button
    };

    let content = GtkBox::new(Orientation::Vertical, 10);
    content.set_margin_top(20);
    content.set_margin_bottom(20);
    content.set_margin_start(20);
    content.set_margin_end(20);

    let logo = Image::from_icon_name("de.dersobi.reoling");
    logo.set_pixel_size(96);
    content.append(&logo);
    let name = Label::new(Some("Reoling"));
    name.add_css_class("title-1");
    content.append(&name);
    let version = Label::new(Some(&format!("Version {}", env!("CARGO_PKG_VERSION"))));
    version.add_css_class("dim-label");
    content.append(&version);
    let blurb = Label::new(Some("An unofficial Linux client for Reolink cameras, NVRs and Home Hubs."));
    blurb.set_wrap(true);
    blurb.set_justify(gtk4::Justification::Center);
    content.append(&blurb);
    content.append(&link("Source code and issues", "GitHub", "https://github.com/derSobi/Reoling"));
    content.append(&link("Support the project", "GitHub Sponsors", "https://github.com/sponsors/derSobi"));
    let legal = Label::new(Some(
        "Licensed under the AGPL-3.0-or-later. Reoling is not affiliated with, endorsed by, or sponsored by Reolink; \"Reolink\" is a trademark of its respective owner.",
    ));
    legal.add_css_class("dim-label");
    legal.add_css_class("caption");
    legal.set_wrap(true);
    legal.set_max_width_chars(44);
    legal.set_justify(gtk4::Justification::Center);
    content.append(&legal);

    let window = Window::builder().transient_for(parent).modal(true).title("About Reoling").resizable(false).build();
    window.set_child(Some(&content));
    window.present();
}
