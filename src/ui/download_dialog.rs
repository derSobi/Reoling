//! The "Download a recording" dialog of the Playback page: the start
//! (device-local, as the page shows every time), how long, which stream
//! and which folder. It only collects the choice, shows the file's name
//! and hands the range it stands for to the page (which marks it on the
//! timeline); the page starts the download.

use crate::ui::bridge::FileSpan;
use crate::ui::dialogs::{button_row, caption, choose_folder, dialog, row};
use crate::ui::playback_page::{stream_index, stream_label, STREAMS, STREAM_LABELS};
use crate::ui::recording_file::{clock_text, download_file_name, range_of};
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Button, DropDown, Entry, Grid, Label, Orientation, Window};
use reoling::{CivilTime, RecordStream};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

/// The length choices, in the dropdown's order (`None`: the whole
/// recording that holds the start).
const LENGTHS: [(&str, Option<i64>); 5] =
    [("30 s", Some(30)), ("1 min", Some(60)), ("5 min", Some(300)), ("15 min", Some(900)), ("Whole recording", None)];
/// The choice offered: "1 min".
const DEFAULT_LENGTH: usize = 1;

/// Wider than the usual dialog: the file's name has to fit.
const WIDTH: i32 = 440;

/// What the dialog opens with.
pub struct Setup {
    /// Midnight of the page's day (device-local).
    pub day: CivilTime,
    /// The start offered: a time of `day`.
    pub start: CivilTime,
    pub stream: RecordStream,
    /// The day's recordings the page has listed, and of which stream (a
    /// range of the other stream is checked when the download starts).
    pub listed: Option<(RecordStream, Vec<FileSpan>)>,
    pub dir: PathBuf,
    /// For the file's name.
    pub device: String,
    pub camera: String,
}

/// What the user chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// Device-local.
    pub start: CivilTime,
    /// Seconds; `None`: the whole recording that holds the start.
    pub duration: Option<i64>,
    pub stream: RecordStream,
    pub dir: PathBuf,
}

/// What the dialog shows for the fields as they stand.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Preview {
    /// The range the device will be asked for, when it can be told here.
    pub range: Option<(CivilTime, CivilTime)>,
    /// The file's name, when the range is known.
    pub name: Option<String>,
    /// Why the download cannot start as the fields stand.
    pub error: Option<String>,
    /// Worth knowing; no error.
    pub note: Option<String>,
}

/// "hh:mm:ss" or "hh:mm" (the hours may have one digit) as hours,
/// minutes and seconds of a day.
fn parse_clock(text: &str) -> Option<(u8, u8, u8)> {
    let mut parts = text.trim().split(':');
    let h: u8 = parts.next()?.trim().parse().ok()?;
    let m: u8 = parts.next()?.trim().parse().ok()?;
    let s: u8 = match parts.next() {
        Some(s) => s.trim().parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() || h > 23 || m > 59 || s > 59 {
        return None;
    }
    Some((h, m, s))
}

/// The preview for a start typed as `clock`, a `duration` and a `stream`.
/// For the listed stream the range and the name are exact (and a range
/// without a recording is an error); for the other one a fixed length
/// is shown as asked for, and a whole recording is only found when the
/// download starts.
fn preview(setup: &Setup, clock: &str, duration: Option<i64>, stream: RecordStream) -> Preview {
    let Some((h, m, s)) = parse_clock(clock) else {
        return Preview { error: Some("Enter the start as hh:mm:ss".to_string()), ..Preview::default() };
    };
    let start = setup.day.add_seconds(i64::from(h) * 3600 + i64::from(m) * 60 + i64::from(s));
    let name = |(from, to): (CivilTime, CivilTime)| {
        download_file_name(&setup.device, &setup.camera, from, to.to_epoch_seconds() - from.to_epoch_seconds())
    };
    match &setup.listed {
        Some((listed, files)) if *listed == stream => {
            let spans: Vec<(CivilTime, CivilTime)> = files.iter().map(|f| (f.start, f.end)).collect();
            match range_of(&spans, start, duration) {
                Ok(range) => Preview { range: Some(range), name: Some(name(range)), ..Preview::default() },
                Err(error) => Preview { error: Some(error), ..Preview::default() },
            }
        }
        _ => {
            let other =
                format!("The {}'s recordings are listed when the download starts", stream_label(stream).to_lowercase());
            match duration {
                Some(seconds) => {
                    let range = (start, start.add_seconds(seconds.max(1)));
                    Preview { range: Some(range), name: Some(name(range)), note: Some(other), ..Preview::default() }
                }
                None => Preview { note: Some(format!("{other}; the file is named after the recording")), ..Preview::default() },
            }
        }
    }
}

/// Opens the dialog. `on_range` follows the range the fields stand for
/// (`None` when there is none to show); `on_dir`, a folder picked (to be
/// remembered); `on_download`, the Download button (the dialog closes);
/// `on_close`, the dialog going away, however it does.
pub fn open(
    parent: Option<&Window>,
    setup: Setup,
    on_range: impl Fn(Option<(CivilTime, CivilTime)>) + 'static,
    on_dir: impl Fn(String) + 'static,
    on_download: impl Fn(Choice) + 'static,
    on_close: impl Fn() + 'static,
) {
    let (window, content) = dialog(parent, "Download a recording", WIDTH);

    let start = Entry::builder()
        .text(clock_text(setup.start))
        .max_length(8)
        .width_chars(10)
        .tooltip_text("Device-local time, hh:mm:ss")
        .build();
    start.set_halign(gtk4::Align::Start);
    let length_labels: Vec<&str> = LENGTHS.iter().map(|(label, _)| *label).collect();
    let length = DropDown::from_strings(&length_labels);
    length.set_selected(DEFAULT_LENGTH as u32);
    length.set_halign(gtk4::Align::Start);
    let stream = DropDown::from_strings(&STREAM_LABELS);
    stream.set_selected(stream_index(setup.stream));
    stream.set_halign(gtk4::Align::Start);
    let dir = Rc::new(RefCell::new(setup.dir.clone()));
    let folder_label = Label::new(Some(&setup.dir.display().to_string()));
    folder_label.set_xalign(0.0);
    folder_label.set_hexpand(true);
    folder_label.set_ellipsize(gtk4::pango::EllipsizeMode::Start);
    let browse = Button::with_label("Browse");
    let folder = GtkBox::new(Orientation::Horizontal, 8);
    folder.append(&folder_label);
    folder.append(&browse);
    let grid = Grid::builder().row_spacing(8).column_spacing(12).build();
    let captions = gtk4::SizeGroup::new(gtk4::SizeGroupMode::Horizontal);
    captions.add_widget(&row(&grid, 0, "Start", &start));
    captions.add_widget(&row(&grid, 1, "Length", &length));
    captions.add_widget(&row(&grid, 2, "Stream", &stream));
    captions.add_widget(&row(&grid, 3, "Folder", &folder));
    // The name wraps, so its row is a box, not a row of the grid: asked
    // its height for no particular width, a grid lays its columns out at
    // their minimum, where the name takes more lines than in the dialog,
    // and GTK then holds the window too small on every layout pass. A
    // box measures a wrapped label at one line.
    let name = Label::new(None);
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_wrap(true);
    name.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    name.set_selectable(true);
    let caption = caption("File");
    captions.add_widget(&caption);
    let file = GtkBox::new(Orientation::Horizontal, 12);
    file.append(&caption);
    file.append(&name);
    let fields = GtkBox::new(Orientation::Vertical, 8);
    fields.append(&grid);
    fields.append(&file);
    content.append(&fields);

    let note = Label::new(None);
    note.set_xalign(0.0);
    note.set_wrap(true);
    note.add_css_class("dim-label");
    note.set_visible(false);
    content.append(&note);
    let error = Label::new(None);
    error.set_xalign(0.0);
    error.set_wrap(true);
    error.add_css_class("error");
    error.set_visible(false);
    content.append(&error);

    let cancel = Button::with_label("Cancel");
    let download = Button::with_label("Download");
    content.append(&button_row(&cancel, &download));
    window.set_default_widget(Some(&download));
    start.set_activates_default(true);

    // The name, the range and whether Download can be pressed follow the
    // fields.
    let setup = Rc::new(setup);
    let on_range = Rc::new(on_range);
    let chosen = {
        let (setup, start, length, stream) = (Rc::clone(&setup), start.clone(), length.clone(), stream.clone());
        Rc::new(move || {
            let duration = LENGTHS.get(length.selected() as usize).map_or(Some(60), |(_, d)| *d);
            let stream = STREAMS.get(stream.selected() as usize).copied().unwrap_or(setup.stream);
            (start.text().to_string(), duration, stream)
        })
    };
    let refresh = {
        let (setup, chosen, on_range) = (Rc::clone(&setup), Rc::clone(&chosen), Rc::clone(&on_range));
        let (name, note, error, download) = (name.clone(), note.clone(), error.clone(), download.clone());
        Rc::new(move || {
            let (clock, duration, stream) = chosen();
            let p = preview(&setup, &clock, duration, stream);
            name.set_text(p.name.as_deref().unwrap_or("—"));
            note.set_visible(p.note.is_some());
            note.set_text(p.note.as_deref().unwrap_or(""));
            error.set_visible(p.error.is_some());
            error.set_text(p.error.as_deref().unwrap_or(""));
            download.set_sensitive(p.error.is_none());
            on_range(p.range);
        })
    };
    let r = Rc::clone(&refresh);
    start.connect_changed(move |_| r());
    let r = Rc::clone(&refresh);
    length.connect_selected_notify(move |_| r());
    let r = Rc::clone(&refresh);
    stream.connect_selected_notify(move |_| r());
    refresh();

    let on_dir = Rc::new(on_dir);
    let w = window.clone();
    let (d, shown) = (Rc::clone(&dir), folder_label.clone());
    browse.connect_clicked(move |_| {
        let (d, shown, on_dir) = (Rc::clone(&d), shown.clone(), Rc::clone(&on_dir));
        let current = d.borrow().clone();
        choose_folder(&w, &current, move |folder| {
            shown.set_text(&folder.display().to_string());
            on_dir(folder.display().to_string());
            *d.borrow_mut() = folder;
        });
    });

    let w = window.clone();
    cancel.connect_clicked(move |_| w.close());
    let w = window.clone();
    let (setup, chosen, dir) = (Rc::clone(&setup), Rc::clone(&chosen), Rc::clone(&dir));
    download.connect_clicked(move |_| {
        let (clock, duration, stream) = chosen();
        let Some((h, m, s)) = parse_clock(&clock) else { return };
        let start = setup.day.add_seconds(i64::from(h) * 3600 + i64::from(m) * 60 + i64::from(s));
        on_download(Choice { start, duration, stream, dir: dir.borrow().clone() });
        w.close();
    });
    window.connect_close_request(move |_| {
        on_close();
        glib::Propagation::Proceed
    });

    window.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(h: u8, m: u8, s: u8) -> CivilTime {
        CivilTime::new(2026, 10, 19, h, m, s)
    }

    fn span(start: CivilTime, end: CivilTime) -> FileSpan {
        FileSpan { start, end, encrypted: false }
    }

    fn setup(listed: Option<(RecordStream, Vec<FileSpan>)>) -> Setup {
        Setup {
            day: CivilTime::start_of_day(2026, 10, 19),
            start: t(14, 5, 0),
            stream: RecordStream::Sub,
            listed,
            dir: PathBuf::from("/tmp"),
            device: "Hub".to_string(),
            camera: "Door".to_string(),
        }
    }

    #[test]
    fn clocks_parse_with_or_without_seconds() {
        assert_eq!(parse_clock("14:03:27"), Some((14, 3, 27)));
        assert_eq!(parse_clock("7:05"), Some((7, 5, 0)));
        assert_eq!(parse_clock(" 23:59:59 "), Some((23, 59, 59)));
        assert_eq!(parse_clock("00:00:00"), Some((0, 0, 0)));
        for bad in ["24:00:00", "14:60", "14:00:60", "14", "1:2:3:4", "ab:cd", "", "-1:00"] {
            assert_eq!(parse_clock(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_preview_names_the_file_and_the_range_for_the_listed_stream() {
        let files = vec![span(t(14, 0, 0), t(14, 10, 0)), span(t(14, 10, 0), t(14, 20, 0))];
        let s = setup(Some((RecordStream::Sub, files)));
        let p = preview(&s, "14:05:00", Some(60), RecordStream::Sub);
        assert_eq!(p.range, Some((t(14, 5, 0), t(14, 6, 0))));
        assert_eq!(p.name.as_deref(), Some("Hub-Door-20261019-140500-60s.mp4"));
        assert_eq!((p.error, p.note), (None, None));
        // The whole recording: the file's own span and length.
        let p = preview(&s, "14:15", None, RecordStream::Sub);
        assert_eq!(p.range, Some((t(14, 10, 0), t(14, 20, 0))));
        assert_eq!(p.name.as_deref(), Some("Hub-Door-20261019-141000-600s.mp4"));
        // Nothing recorded there: no download.
        let p = preview(&s, "15:00:00", Some(60), RecordStream::Sub);
        assert_eq!(p.error.as_deref(), Some("No recording between 15:00:00 and 15:01:00"));
        assert_eq!((p.range, p.name), (None, None));
        let p = preview(&s, "x", Some(60), RecordStream::Sub);
        assert_eq!(p.error.as_deref(), Some("Enter the start as hh:mm:ss"));
        assert_eq!((p.range, p.name), (None, None));
    }

    #[test]
    fn the_other_streams_recordings_are_only_known_when_the_download_starts() {
        let files = vec![span(t(14, 0, 0), t(14, 10, 0))];
        for s in [setup(Some((RecordStream::Sub, files))), setup(None)] {
            // A fixed length: the range as asked for, named, with a note.
            let p = preview(&s, "15:00:00", Some(30), RecordStream::Main);
            assert_eq!(p.range, Some((t(15, 0, 0), t(15, 0, 30))));
            assert_eq!(p.name.as_deref(), Some("Hub-Door-20261019-150000-30s.mp4"));
            assert_eq!(p.error, None);
            assert_eq!(p.note.as_deref(), Some("The main stream's recordings are listed when the download starts"));
            // The whole recording: not known here.
            let p = preview(&s, "15:00:00", None, RecordStream::Main);
            assert_eq!((p.range, p.name, p.error), (None, None, None));
            assert!(p.note.as_deref().is_some_and(|n| n.ends_with("the file is named after the recording")), "{:?}", p.note);
        }
    }

    #[test]
    fn the_length_choices_default_to_a_minute() {
        assert_eq!(LENGTHS[DEFAULT_LENGTH], ("1 min", Some(60)));
        let labels: Vec<&str> = LENGTHS.iter().map(|(label, _)| *label).collect();
        assert_eq!(labels, ["30 s", "1 min", "5 min", "15 min", "Whole recording"]);
        assert_eq!(LENGTHS.last().map(|(_, d)| *d), Some(None));
        assert_eq!(STREAMS[stream_index(RecordStream::Sub) as usize], RecordStream::Sub);
        assert_eq!(stream_label(RecordStream::Main), "Main stream");
    }

    /// Every widget under `widget`, depth first.
    fn descendants(widget: &gtk4::Widget, out: &mut Vec<gtk4::Widget>) {
        let mut child = widget.first_child();
        while let Some(c) = child {
            out.push(c.clone());
            descendants(&c, out);
            child = c.next_sibling();
        }
    }

    /// Runs the main loop for `ms`.
    fn pump(ms: u64) {
        let ctx = glib::MainContext::default();
        let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        while std::time::Instant::now() < until {
            while ctx.iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    static MEASURE_WARNINGS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// Needs a display: opens the dialog, makes its note and error appear
    /// and disappear as the fields change and counts GTK's "Trying to
    /// measure" warnings (a window smaller than its content) meanwhile.
    #[test]
    #[ignore = "opens a window on the display; run explicitly"]
    fn the_window_keeps_up_with_its_content() {
        use std::sync::atomic::Ordering::SeqCst;
        gtk4::init().expect("a display");
        glib::log_set_writer_func(|level, fields| {
            let message = fields.iter().find(|f| f.key() == "MESSAGE").and_then(|f| f.value_str()).unwrap_or("");
            if message.contains("Trying to measure") {
                MEASURE_WARNINGS.fetch_add(1, SeqCst);
            }
            glib::log_writer_default(level, fields)
        });
        let files = vec![span(t(14, 0, 0), t(14, 10, 0)), span(t(14, 10, 0), t(14, 20, 0))];
        open(None, setup(Some((RecordStream::Sub, files))), |_| {}, |_| {}, |_| {}, || {});
        let window = Window::list_toplevels()
            .into_iter()
            .filter_map(|w| w.downcast::<Window>().ok())
            .find(|w| w.title().is_some_and(|t| t == "Download a recording"))
            .expect("the dialog");
        let mut widgets = Vec::new();
        descendants(window.upcast_ref(), &mut widgets);
        let start = widgets.iter().find_map(|w| w.clone().downcast::<Entry>().ok()).expect("the start entry");
        let dropdowns: Vec<DropDown> = widgets.iter().filter_map(|w| w.clone().downcast::<DropDown>().ok()).collect();
        let (length, stream) = (&dropdowns[0], &dropdowns[1]);
        let report = |step: &str| {
            let (min_h, _, _, _) = window.measure(Orientation::Vertical, window.width());
            eprintln!(
                "{step}: window {}x{} px, content needs {min_h} px, warnings so far {}",
                window.width(),
                window.height(),
                MEASURE_WARNINGS.load(SeqCst)
            );
        };
        pump(600);
        report("presented");
        // The File row lines up with the grid: its caption as wide and as
        // placed as the grid's, the name where the grid's second column is.
        let labels: Vec<Label> = widgets.iter().filter_map(|w| w.clone().downcast::<Label>().ok()).collect();
        let caption_of = |text: &str| labels.iter().find(|l| l.text() == text).expect(text).allocation();
        let (folder, file) = (caption_of("Folder"), caption_of("File"));
        eprintln!("captions: Folder at x={} w={}, File at x={} w={}", folder.x(), folder.width(), file.x(), file.width());
        assert_eq!((folder.x(), folder.width()), (file.x(), file.width()));
        let name = labels.iter().find(|l| l.text().ends_with(".mp4")).expect("the name").allocation();
        eprintln!("start entry x={}, name x={}", start.allocation().x(), name.x());
        assert_eq!(start.allocation().x(), name.x());
        let steps: [(&str, &dyn Fn()); 9] = [
            ("start 'x' (error)", &|| start.set_text("x")),
            ("start 15:00:00 (no recording)", &|| start.set_text("15:00:00")),
            ("start 14:05:00 (named)", &|| start.set_text("14:05:00")),
            ("main stream (note)", &|| stream.set_selected(stream_index(RecordStream::Main))),
            ("whole recording (long note)", &|| length.set_selected(LENGTHS.len() as u32 - 1)),
            ("sub stream, whole recording", &|| stream.set_selected(stream_index(RecordStream::Sub))),
            ("1 min again (plain)", &|| length.set_selected(DEFAULT_LENGTH as u32)),
            ("focus leaves the start", &|| GtkWindowExt::set_focus(&window, None::<&gtk4::Widget>)),
            ("focus back on the start", &|| { start.grab_focus(); }),
        ];
        for (name, act) in steps {
            act();
            pump(600);
            report(name);
        }
        window.close();
        pump(300);
        let warnings = MEASURE_WARNINGS.load(SeqCst);
        eprintln!("measure warnings: {warnings}");
        assert_eq!(warnings, 0, "the window was smaller than its content");
    }
}
