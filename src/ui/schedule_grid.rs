//! A week of hours as a grid: 24 columns by 7 rows, one filled cell per
//! hour a schedule is on. The settings pages show one grid per schedule
//! type; a schedule with no hour set is listed as text instead.

use gtk4::prelude::*;
use gtk4::{cairo, Align, Box as GtkBox, DrawingArea, Label, Orientation, Widget};
use reoling::ScheduleTable;

/// The row labels, first day first. The device's tables start with Sunday
/// as far as is known; the day order has not been confirmed on a device
/// with a schedule that differs between days.
pub const DAY_LABELS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Cell geometry, in pixels.
pub const CELL: f64 = 12.0;
pub const GAP: f64 = 1.0;
/// Room for the day labels on the left and the hour marks above.
pub const LABEL_WIDTH: f64 = 30.0;
pub const MARK_HEIGHT: f64 = 14.0;

/// The hours marked above the columns.
pub const HOUR_MARKS: [usize; 4] = [0, 6, 12, 18];

/// A cell's index in a table: day-major, hour-minor.
pub fn cell(day: usize, hour: usize) -> usize {
    day * ScheduleTable::HOURS + hour
}

/// The top-left corner of a cell in the drawing area.
pub fn cell_origin(day: usize, hour: usize) -> (f64, f64) {
    (LABEL_WIDTH + hour as f64 * (CELL + GAP), MARK_HEIGHT + day as f64 * (CELL + GAP))
}

/// The drawing area's size for one table.
pub fn grid_size() -> (i32, i32) {
    let width = LABEL_WIDTH + ScheduleTable::HOURS as f64 * (CELL + GAP);
    let height = MARK_HEIGHT + ScheduleTable::DAYS as f64 * (CELL + GAP);
    (width.ceil() as i32, height.ceil() as i32)
}

/// The grids of one or more schedules, stacked, each under its type label.
pub struct ScheduleGrid {
    root: GtkBox,
}

impl Default for ScheduleGrid {
    fn default() -> Self {
        Self::new()
    }
}

impl ScheduleGrid {
    pub fn new() -> Self {
        let root = GtkBox::new(Orientation::Vertical, 8);
        root.set_halign(Align::Start);
        ScheduleGrid { root }
    }

    pub fn widget(&self) -> &Widget {
        self.root.upcast_ref()
    }

    /// One grid per (label, table); an all-off table is listed as
    /// "<label>: Off" text.
    pub fn set(&self, tables: &[(String, ScheduleTable)]) {
        self.clear();
        for (label, table) in tables {
            if !table.any() {
                let off = Label::builder().label(format!("{label}: Off")).xalign(0.0).css_classes(["caption", "dim-label"]).build();
                self.root.append(&off);
                continue;
            }
            let caption = Label::builder().label(label).xalign(0.0).css_classes(["caption"]).build();
            self.root.append(&caption);
            self.root.append(&draw_table(label, *table));
        }
    }

    pub fn clear(&self) {
        while let Some(child) = self.root.first_child() {
            self.root.remove(&child);
        }
    }
}

/// The hours a table is on, in words: "Every hour", "Off", or the days with
/// the same hours together, e.g. "Mon–Fri: 00:00–06:00, 18:00–24:00; Sun,
/// Sat: all day". The grid's tooltip and what assistive technology reads.
pub fn describe(table: &ScheduleTable) -> String {
    if table.all() {
        return "Every hour".to_string();
    }
    if !table.any() {
        return "Off".to_string();
    }
    let cells = table.cells();
    let hours_of = |day: usize| &cells[cell(day, 0)..cell(day, 0) + ScheduleTable::HOURS];
    let mut described = [false; ScheduleTable::DAYS];
    let mut parts = Vec::new();
    for day in 0..ScheduleTable::DAYS {
        if described[day] || !hours_of(day).contains(&true) {
            continue;
        }
        let same: Vec<usize> = (day..ScheduleTable::DAYS).filter(|d| hours_of(*d) == hours_of(day)).collect();
        for d in &same {
            described[*d] = true;
        }
        parts.push(format!("{}: {}", day_list(&same), hour_list(hours_of(day))));
    }
    parts.join("; ")
}

/// Days in order, consecutive ones as a span: "Mon–Fri", "Sun, Sat".
fn day_list(days: &[usize]) -> String {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for &day in days {
        match runs.last_mut() {
            Some((_, end)) if *end + 1 == day => *end = day,
            _ => runs.push((day, day)),
        }
    }
    runs.iter()
        .map(|(first, last)| match last - first {
            0 => DAY_LABELS[*first].to_string(),
            1 => format!("{}, {}", DAY_LABELS[*first], DAY_LABELS[*last]),
            _ => format!("{}–{}", DAY_LABELS[*first], DAY_LABELS[*last]),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The on-hours of one day as spans: "all day", "18:00–24:00".
fn hour_list(hours: &[bool]) -> String {
    if hours.iter().all(|on| *on) {
        return "all day".to_string();
    }
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (hour, _) in hours.iter().enumerate().filter(|(_, on)| **on) {
        match spans.last_mut() {
            Some((_, end)) if *end == hour => *end = hour + 1,
            _ => spans.push((hour, hour + 1)),
        }
    }
    spans.iter().map(|(from, to)| format!("{from:02}:00–{to:02}:00")).collect::<Vec<_>>().join(", ")
}

/// A drawing area showing one table in the widget's own foreground colour:
/// set hours at full strength, the rest faint, so it reads in any theme.
/// The same hours in words are its tooltip and its accessible description.
fn draw_table(label: &str, table: ScheduleTable) -> DrawingArea {
    let (width, height) = grid_size();
    let area = DrawingArea::new();
    area.set_content_width(width);
    area.set_content_height(height);
    area.set_halign(Align::Start);
    let words = describe(&table);
    area.set_tooltip_text(Some(&words));
    area.update_property(&[
        gtk4::accessible::Property::Label(&format!("{label} schedule")),
        gtk4::accessible::Property::Description(&format!("Hours of the week the schedule is on, first day {}: {words}", DAY_LABELS[0])),
    ]);
    area.set_draw_func(move |area, cr, _, _| {
        let fg = area.style_context().color();
        let (r, g, b) = (f64::from(fg.red()), f64::from(fg.green()), f64::from(fg.blue()));
        // The marks and day names in a small sans, like the timeline's.
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
        cr.set_font_size(10.0);
        cr.set_source_rgba(r, g, b, 0.7);
        for hour in HOUR_MARKS {
            let (x, _) = cell_origin(0, hour);
            cr.move_to(x, MARK_HEIGHT - 4.0);
            let _ = cr.show_text(&hour.to_string());
        }
        for (day, label) in DAY_LABELS.iter().enumerate() {
            let (_, y) = cell_origin(day, 0);
            cr.move_to(0.0, y + CELL - 3.0);
            let _ = cr.show_text(label);
        }
        let cells = table.cells();
        for day in 0..ScheduleTable::DAYS {
            for hour in 0..ScheduleTable::HOURS {
                let alpha = if cells[cell(day, hour)] { 1.0 } else { 0.12 };
                cr.set_source_rgba(r, g, b, alpha);
                let (x, y) = cell_origin(day, hour);
                cr.rectangle(x, y, CELL, CELL);
                let _ = cr.fill();
            }
        }
    });
    area
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_are_day_major() {
        assert_eq!(cell(0, 0), 0);
        assert_eq!(cell(0, 23), 23);
        assert_eq!(cell(1, 0), 24);
        assert_eq!(cell(6, 23), 167);
        // The widget's index agrees with the table's own lookup.
        let mut text = "0".repeat(168);
        text.replace_range(cell(3, 7)..cell(3, 7) + 1, "1");
        let table = ScheduleTable::parse(&text).unwrap();
        assert!(table.on(3, 7));
        assert!(!table.on(3, 8));
        assert!(!table.on(2, 7));
    }

    #[test]
    fn geometry_leaves_room_for_the_labels() {
        assert_eq!(cell_origin(0, 0), (LABEL_WIDTH, MARK_HEIGHT));
        let (x, y) = cell_origin(1, 2);
        assert_eq!(x, LABEL_WIDTH + 2.0 * (CELL + GAP));
        assert_eq!(y, MARK_HEIGHT + CELL + GAP);
        let (w, h) = grid_size();
        let (last_x, last_y) = cell_origin(6, 23);
        assert!(f64::from(w) >= last_x + CELL);
        assert!(f64::from(h) >= last_y + CELL);
        assert_eq!(DAY_LABELS.len(), ScheduleTable::DAYS);
        assert_eq!(DAY_LABELS[0], "Sun");
        assert!(HOUR_MARKS.iter().all(|h| *h < ScheduleTable::HOURS));
    }

    #[test]
    fn tables_in_words() {
        assert_eq!(describe(&ScheduleTable::parse(&"1".repeat(168)).unwrap()), "Every hour");
        assert_eq!(describe(&ScheduleTable::parse(&"0".repeat(168)).unwrap()), "Off");
        // Weekdays (Mon–Fri) from 18:00 to 06:00, the weekend all day.
        let weekday = format!("{}{}{}", "1".repeat(6), "0".repeat(12), "1".repeat(6));
        let weekend = "1".repeat(24);
        let week = format!("{weekend}{}{weekend}", weekday.repeat(5));
        assert_eq!(describe(&ScheduleTable::parse(&week).unwrap()), "Sun, Sat: all day; Mon–Fri: 00:00–06:00, 18:00–24:00");
        // One hour on a single day; two days alike but apart; a day off.
        let mut text = "0".repeat(168);
        text.replace_range(cell(1, 9)..cell(1, 10), "1");
        text.replace_range(cell(3, 9)..cell(3, 10), "1");
        text.replace_range(cell(6, 0)..cell(6, 24), &"1".repeat(24));
        assert_eq!(describe(&ScheduleTable::parse(&text).unwrap()), "Mon, Wed: 09:00–10:00; Sat: all day");
        assert_eq!(day_list(&[0, 1, 2, 4, 5, 6]), "Sun–Tue, Thu–Sat");
        assert_eq!(day_list(&[1, 3]), "Mon, Wed");
        assert_eq!(hour_list(&[true, true, false, true]), "00:00–02:00, 03:00–04:00");
    }
}
