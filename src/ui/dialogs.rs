//! Modal dialogs: add a device, log in to a device; and the pieces the
//! other dialogs of the app are built from (a modal window with its
//! content box, a labelled row and its caption, the Cancel/accept button
//! row, the folder chooser), and opening a folder or a link in the
//! desktop's application for it.

use crate::ui::bridge::ConnectTarget;
use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, CheckButton, Entry, Grid, Label, Notebook, Orientation,
    PasswordEntry, Window,
};
use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// The usual width of a dialog.
const DIALOG_WIDTH: i32 = 380;

/// A modal dialog window of `width` over `parent` and the vertical box
/// its content goes into.
pub(crate) fn dialog(parent: Option<&Window>, title: &str, width: i32) -> (Window, GtkBox) {
    let mut builder = Window::builder().modal(true).title(title).resizable(false).default_width(width);
    if let Some(parent) = parent {
        builder = builder.transient_for(parent);
    }
    let window = builder.build();
    let content = GtkBox::new(Orientation::Vertical, 12);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    window.set_child(Some(&content));
    (window, content)
}

/// The caption of a labelled row: a label set at the end of its column.
pub(crate) fn caption(text: &str) -> Label {
    let label = Label::new(Some(text));
    label.set_halign(gtk4::Align::End);
    label
}

/// A labelled row of a dialog's grid: the caption at the end of the first
/// column, `widget` filling the second. Hands back the caption (for a
/// row outside the grid to line up with).
pub(crate) fn row(grid: &Grid, row: i32, label: &str, widget: &impl IsA<gtk4::Widget>) -> Label {
    let label = caption(label);
    grid.attach(&label, 0, row, 1, 1);
    widget.set_hexpand(true);
    grid.attach(widget, 1, row, 1, 1);
    label
}

/// Opens the system's folder chooser over `parent`, at `current`;
/// `on_pick` gets the folder chosen (nothing on Cancel). The chooser is
/// kept alive while it is shown and let go with its answer.
pub(crate) fn choose_folder(parent: &Window, current: &Path, on_pick: impl Fn(PathBuf) + 'static) {
    let chooser = gtk4::FileChooserNative::new(
        Some("Choose a folder"),
        Some(parent),
        gtk4::FileChooserAction::SelectFolder,
        Some("Select"),
        Some("Cancel"),
    );
    let _ = chooser.set_current_folder(Some(&gtk4::gio::File::for_path(current)));
    // The chooser is no widget: nothing holds it while it is shown but
    // this slot, which its answer empties (a reference from its own
    // handler would keep it for good).
    let slot: Rc<RefCell<Option<gtk4::FileChooserNative>>> = Rc::new(RefCell::new(Some(chooser.clone())));
    let held = Rc::clone(&slot);
    chooser.connect_response(move |chooser, response| {
        if response == gtk4::ResponseType::Accept {
            if let Some(folder) = chooser.file().and_then(|f| f.path()) {
                on_pick(folder);
            }
        }
        held.borrow_mut().take();
    });
    chooser.show();
}

/// Opens `uri` (a folder, a web page) in the desktop's default application
/// for it. In the AppImage on its own GTK and GStreamer core (the "bundled"
/// stack, see scripts/appimage-hook.sh) LD_LIBRARY_PATH names their
/// directory, which an application started from here would load instead of
/// its own; that one directory is left out of its environment. Everything
/// else it inherits as before.
pub(crate) fn open_uri(uri: &str) {
    let context = match (std::env::var_os("APPDIR"), std::env::var_os("LD_LIBRARY_PATH")) {
        (Some(appdir), Some(path)) => match child_library_path(&path, Path::new(&appdir)) {
            ChildLibraryPath::Inherited => None,
            ChildLibraryPath::Unset => {
                let context = gtk4::gio::AppLaunchContext::new();
                context.unsetenv("LD_LIBRARY_PATH");
                Some(context)
            }
            ChildLibraryPath::Set(path) => {
                let context = gtk4::gio::AppLaunchContext::new();
                context.setenv("LD_LIBRARY_PATH", path);
                Some(context)
            }
        },
        _ => None,
    };
    let _ = gtk4::gio::AppInfo::launch_default_for_uri(uri, context.as_ref());
}

/// What an application started from the AppImage mounted at `appdir` gets
/// as LD_LIBRARY_PATH, from this process's `path`.
#[derive(Debug, PartialEq, Eq)]
enum ChildLibraryPath {
    /// `path` as it is: it names no GTK of the AppImage's.
    Inherited,
    /// Nothing: the AppImage's GTK was all it named.
    Unset,
    Set(OsString),
}

/// `path` without `appdir`'s `usr/lib/gtk-fallback`, the directory of the
/// AppImage's own GTK and GStreamer core.
fn child_library_path(path: &OsStr, appdir: &Path) -> ChildLibraryPath {
    let fallback = appdir.join("usr/lib/gtk-fallback");
    let all: Vec<PathBuf> = std::env::split_paths(path).collect();
    let kept: Vec<&PathBuf> = all.iter().filter(|dir| **dir != fallback).collect();
    if kept.len() == all.len() {
        ChildLibraryPath::Inherited
    } else if kept.is_empty() {
        ChildLibraryPath::Unset
    } else {
        // The entries came from splitting at ':', so none holds one.
        std::env::join_paths(kept).map_or(ChildLibraryPath::Inherited, ChildLibraryPath::Set)
    }
}

/// A dialog's bottom row: `cancel` and `accept` (the suggested action)
/// at the end.
pub(crate) fn button_row(cancel: &Button, accept: &Button) -> GtkBox {
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
    let (window, content) = dialog(Some(parent), "Add device", DIALOG_WIDTH);

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
    let (window, content) = dialog(Some(parent), device_name, DIALOG_WIDTH);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applications_started_from_the_bundled_stack_do_not_get_its_gtk() {
        let appdir = Path::new("/tmp/.mount_ReolinAbCdEf");
        let path = |s: &str| OsString::from(s);
        // As the hook leaves it in the bundled mode: the fallback directory
        // first, then the pixbuf loaders and what the system had.
        assert_eq!(
            child_library_path(
                &path("/tmp/.mount_ReolinAbCdEf/usr/lib/gtk-fallback:/tmp/.mount_ReolinAbCdEf/usr/lib/loaders:/opt/x"),
                appdir
            ),
            ChildLibraryPath::Set(path("/tmp/.mount_ReolinAbCdEf/usr/lib/loaders:/opt/x"))
        );
        assert_eq!(
            child_library_path(&path("/tmp/.mount_ReolinAbCdEf/usr/lib/gtk-fallback/"), appdir),
            ChildLibraryPath::Unset
        );
        // The system's stack, or a path that is no AppImage's: unchanged.
        for unchanged in ["", "/opt/x", "/tmp/.mount_ReolinAbCdEf/usr/lib/libva-fallback/libva-x11.so.2:/opt/x"] {
            assert_eq!(child_library_path(&path(unchanged), appdir), ChildLibraryPath::Inherited, "{unchanged}");
        }
        // Another AppImage's fallback directory is that one's business.
        assert_eq!(
            child_library_path(&path("/tmp/.mount_OtherXyZ/usr/lib/gtk-fallback"), appdir),
            ChildLibraryPath::Inherited
        );
    }
}
