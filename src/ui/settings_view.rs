//! The widgets of a read-only settings tab: a status line and the groups
//! of rows that `settings_pages` describes, rendered in the style of the
//! app's other settings rows (title left, value right).

use crate::ui::schedule_grid::ScheduleGrid;
use crate::ui::settings_pages::{Group, Row, Tab, Value};
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Expander, Label, LevelBar, ListBox, ListBoxRow, Orientation, ScrolledWindow, SelectionMode};

/// What a tab says while its reads are out.
pub const READING: &str = "Reading…";
/// What a tab says while a download holds the connection.
pub const WAITING_FOR_DOWNLOAD: &str = "Waiting for the download to finish…";
/// What a tab says when there is no connection to read it over.
pub const NOT_CONNECTED: &str = "Not connected";
/// What a tab says once the channel's live video was stopped: the device
/// is still connected, and the window reads again when the channel is
/// watched again.
pub const LIVE_STOPPED: &str = "Live video is stopped; press Play to read the camera's settings";

/// One tab: a scrolling column of groups under a status line.
pub struct ReadOnlyPage {
    scroller: ScrolledWindow,
    status: Label,
    groups: GtkBox,
}

impl Default for ReadOnlyPage {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadOnlyPage {
    pub fn new() -> Self {
        let column = GtkBox::new(Orientation::Vertical, 14);
        column.set_margin_top(12);
        column.set_margin_bottom(12);
        column.set_margin_start(4);
        column.set_margin_end(4);
        let status = Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label"]).visible(false).build();
        let groups = GtkBox::new(Orientation::Vertical, 14);
        column.append(&status);
        column.append(&groups);
        let scroller = ScrolledWindow::builder().child(&column).hexpand(true).vexpand(true).build();
        scroller.set_hscrollbar_policy(gtk4::PolicyType::Never);
        ReadOnlyPage { scroller, status, groups }
    }

    pub fn widget(&self) -> &ScrolledWindow {
        &self.scroller
    }

    /// The line above the groups ("Reading…"); empty hides it.
    pub fn set_status(&self, text: &str) {
        self.status.set_text(text);
        self.status.set_visible(!text.is_empty());
    }

    /// Replaces everything with the tab's content.
    pub fn show(&self, tab: &Tab) {
        self.clear();
        self.set_status(&tab.status);
        for group in &tab.groups {
            self.groups.append(&group_widget(group));
        }
    }

    pub fn clear(&self) {
        self.set_status("");
        while let Some(child) = self.groups.first_child() {
            self.groups.remove(&child);
        }
    }
}

fn group_widget(group: &Group) -> GtkBox {
    let column = GtkBox::new(Orientation::Vertical, 6);
    if !group.title.is_empty() {
        let heading = Label::builder().label(&group.title).xalign(0.0).css_classes(["heading"]).build();
        column.append(&heading);
    }
    let list = ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(SelectionMode::None);
    for row in &group.rows {
        list.append(&row_widget(row));
    }
    column.append(&list);
    column
}

/// A value label: selectable so it can be copied, never focused (a focused
/// selectable label shows a caret and selects all).
fn value_label(text: &str) -> Label {
    Label::builder()
        .label(text)
        .xalign(1.0)
        .justify(gtk4::Justification::Right)
        .wrap(true)
        .max_width_chars(40)
        .css_classes(["dim-label"])
        .selectable(true)
        .focusable(false)
        .build()
}

fn title_label(text: &str) -> Label {
    let label = Label::builder().label(text).xalign(0.0).wrap(true).hexpand(true).build();
    label.set_valign(Align::Center);
    label
}

fn line() -> GtkBox {
    let line = GtkBox::new(Orientation::Horizontal, 12);
    line.set_margin_top(10);
    line.set_margin_bottom(10);
    line.set_margin_start(12);
    line.set_margin_end(12);
    line
}

fn row_widget(row: &Row) -> ListBoxRow {
    let content: gtk4::Widget = match &row.value {
        Value::Text(text) => {
            let line = line();
            if row.title.is_empty() {
                // A line on its own (why a group has nothing to show).
                let note = Label::builder().label(text).xalign(0.0).wrap(true).hexpand(true).css_classes(["dim-label"]).build();
                line.append(&note);
            } else {
                line.append(&title_label(&row.title));
                line.append(&value_label(text));
            }
            line.upcast()
        }
        Value::Level { text, fraction } => {
            let line = line();
            let title = title_label(&row.title);
            line.append(&title);
            let level = GtkBox::new(Orientation::Horizontal, 10);
            let bar = LevelBar::new();
            bar.set_value(*fraction);
            bar.set_width_request(140);
            bar.set_valign(Align::Center);
            // Read out as "<title>: <text>", like the rows around it.
            bar.update_relation(&[gtk4::accessible::Relation::LabelledBy(&[title.upcast_ref()])]);
            bar.update_property(&[gtk4::accessible::Property::ValueText(text)]);
            level.append(&bar);
            level.append(&value_label(text));
            line.append(&level);
            line.upcast()
        }
        Value::Schedules(tables) => {
            let column = GtkBox::new(Orientation::Vertical, 8);
            column.set_margin_top(10);
            column.set_margin_bottom(10);
            column.set_margin_start(12);
            column.set_margin_end(12);
            if !row.title.is_empty() {
                column.append(&Label::builder().label(&row.title).xalign(0.0).build());
            }
            let grid = ScheduleGrid::new();
            grid.set(tables);
            column.append(grid.widget());
            column.upcast()
        }
        Value::Lines { text, lines } => {
            let column = GtkBox::new(Orientation::Vertical, 4);
            let line = line();
            line.set_margin_bottom(if lines.is_empty() { 10 } else { 4 });
            line.append(&title_label(&row.title));
            line.append(&value_label(text));
            column.append(&line);
            if !lines.is_empty() {
                let expander = Expander::new(Some("Per channel"));
                expander.set_margin_start(12);
                expander.set_margin_end(12);
                expander.set_margin_bottom(10);
                let list = GtkBox::new(Orientation::Vertical, 2);
                list.set_margin_top(4);
                list.set_margin_start(12);
                for text in lines {
                    list.append(&Label::builder().label(text).xalign(0.0).css_classes(["caption", "dim-label"]).build());
                }
                expander.set_child(Some(&list));
                column.append(&expander);
            }
            column.upcast()
        }
    };
    let row = ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);
    row.set_child(Some(&content));
    row
}
