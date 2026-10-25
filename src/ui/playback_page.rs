//! The Playback page: recorded footage of one device channel, a day at a
//! time. The device and channel come from the sidebar; a calendar picks the
//! day (recorded days are marked), the day timeline shows its recordings
//! and event windows and seeks, and the controls play, pause, jump between
//! events (under a detection filter applied here, not on the device) and
//! choose the speed and the stream.
//!
//! The page only says what it wants. The device's network thread does the
//! work and reports back as `PlaybackEvent`s, which are dropped here unless
//! they belong to the device on the page and to its current session (every
//! day, device or channel change starts a new one). Every time shown is
//! device-local, exactly as the device reports it: the host's zone is used
//! for nothing but the calendar's first month.
//!
//! A Download button saves a range of the day to an MP4 file
//! (`download_dialog` asks what, `download_job` does it on the device's
//! thread). The page shows the download's progress under the controls
//! and marks its range on the timeline; as everything else here, that
//! belongs to the session it started in, and the download runs on
//! whether or not the page still shows it.

use crate::ui::bridge::{
    DeviceLink, DownloadEvent, DownloadOutcome, DownloadRequest, FileSpan, PlaybackEvent, PlayerPhase, PlayerState,
};
use crate::ui::download_dialog::{self, Choice, Setup};
use crate::ui::playback_driver::{civil_at, civil_of, seconds_of_day, timeline_of};
use crate::ui::playback_timeline::{PlaybackTimeline, TimelineEvent};
use crate::ui::recording_file::size_text;
use crate::ui::video_view::VideoView;
use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, Calendar, DropDown, Label, MenuButton, Orientation, Overlay, Popover, ProgressBar, Spinner,
    ToggleButton,
};
use reoling::{CivilTime, RecordStream, RecordedEvent, ReplaySpeed};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// How often the cursor and the time label follow the picture.
const CURSOR_TICK: Duration = Duration::from_millis(250);

/// A month's recorded days are asked for again when it is shown more than
/// this long after they came (the device's current month gains days).
const MONTH_FRESH_FOR: Duration = Duration::from_secs(60);

/// How long a notice stays over the picture.
const NOTICE_FOR: Duration = Duration::from_secs(4);

/// After a seek the cursor stays where it was put until the device's thread
/// has taken the seek up, at most this long (an open over a relay can take
/// 20 s, and the close before it a few more).
const SEEK_HOLD: Duration = Duration::from_secs(30);

/// Less footage than this waiting in the picture: the day's last recording
/// has been played out.
const PLAYED_OUT_BELOW: Duration = Duration::from_millis(300);

/// The stream dropdown, in the live page's order and words (the download
/// dialog's too).
pub(crate) const STREAMS: [RecordStream; 2] = [RecordStream::Main, RecordStream::Sub];
pub(crate) const STREAM_LABELS: [&str; 2] = ["Main stream", "Sub stream"];

/// An event jump passes over a start this close to the position: the one
/// just played from (the device starts a little before the seek time), or
/// a second window at the same moment.
const EVENT_JUMP_SLACK: f64 = 2.0;

/// The detection filter: which event windows the timeline marks and the
/// event buttons jump between. Applied here; the device lists every kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventFilter {
    All,
    Motion,
    Person,
    Vehicle,
    Animal,
    Package,
    Zone,
}

impl EventFilter {
    /// In the dropdown's order.
    pub const ALL: [EventFilter; 7] = [
        EventFilter::All,
        EventFilter::Motion,
        EventFilter::Person,
        EventFilter::Vehicle,
        EventFilter::Animal,
        EventFilter::Package,
        EventFilter::Zone,
    ];

    pub fn label(self) -> &'static str {
        match self {
            EventFilter::All => "All events",
            EventFilter::Motion => "Any motion",
            EventFilter::Person => "Person",
            EventFilter::Vehicle => "Vehicle",
            EventFilter::Animal => "Animal",
            EventFilter::Package => "Package",
            EventFilter::Zone => "Line crossing / zone",
        }
    }

    /// The kinds (as the device names them) the filter admits; `None`
    /// admits every kind. Each is one the event search asks the device for.
    pub fn kinds(self) -> Option<&'static [&'static str]> {
        match self {
            EventFilter::All => None,
            EventFilter::Motion => Some(&["md", "pir", "other"]),
            EventFilter::Person => Some(&["people", "face", "visitor"]),
            EventFilter::Vehicle => Some(&["vehicle", "nonmotorveh"]),
            EventFilter::Animal => Some(&["dog_cat"]),
            EventFilter::Package => Some(&["package"]),
            EventFilter::Zone => Some(&["crossline", "intrude", "loitering"]),
        }
    }

    /// Whether the filter shows an event of these kinds.
    pub fn admits(self, event: &RecordedEvent) -> bool {
        match self.kinds() {
            None => true,
            Some(kinds) => event.kinds.iter().any(|kind| kinds.contains(&kind.as_str())),
        }
    }
}

/// What the user reads for a kind as the device names it: every kind the
/// event search asks for has a name here (an unknown one is shown as the
/// device names it).
fn kind_label(kind: &str) -> &str {
    match kind {
        "md" => "Motion",
        "pir" => "PIR motion",
        "other" => "Other",
        "io" => "Alarm input",
        "people" => "Person",
        "face" => "Face",
        "vehicle" => "Vehicle",
        "nonmotorveh" => "Non-motor vehicle",
        "dog_cat" => "Animal",
        "visitor" => "Visitor",
        "cry" => "Crying",
        "package" => "Package",
        "crossline" => "Line crossing",
        "intrude" => "Intrusion",
        "loitering" => "Loitering",
        "legacy" => "Legacy alarm",
        "loss" => "Video loss",
        "answer" => "Doorbell answered",
        "tamper" => "Tampering",
        "lmsg" => "Message",
        other => other,
    }
}

/// Whether the page's device can be asked for recordings yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// Not logged in (yet).
    Connecting,
    /// An NVR or Home Hub that has not sent its channel list on this
    /// connection: the requests need the cameras it names.
    Channels,
    Ready,
}

pub struct Handlers {
    /// Runs `f` with device `key`'s link, if it has one.
    pub with_link: Box<dyn Fn(&str, &dyn Fn(&DeviceLink))>,
    /// Device `key`'s name and the name of its camera on `channel`, for a
    /// download's file name.
    pub names: Box<dyn Fn(&str, u8) -> (String, String)>,
    /// Where downloads go (the recordings folder of the settings).
    pub download_dir: Box<dyn Fn() -> Option<PathBuf>>,
    /// The user chose another folder for downloads: kept in the settings.
    pub set_download_dir: Box<dyn Fn(String)>,
}

/// What the page needs to pick up where it was once a lost device is back.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Remembered {
    day: CivilTime,
    position: Option<CivilTime>,
    was_playing: bool,
}

/// A seek the device's thread has not taken up yet: the cursor stays on it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PendingSeek {
    seconds: f64,
    at: CivilTime,
    since: Instant,
}

/// Looking for the latest recorded day: the month asked for, and whether it
/// is already the month before the first one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Choosing {
    year: u16,
    month: u8,
    second: bool,
}

/// The download started in the current session, as the page shows it.
#[derive(Debug, Clone, PartialEq)]
struct Download {
    name: String,
    /// The file, once saved.
    path: Option<PathBuf>,
    bytes: u64,
    estimate: Option<u64>,
    /// The range asked for (device-local), once the device accepted it.
    range: Option<(CivilTime, CivilTime)>,
    phase: DownloadPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DownloadPhase {
    /// Asked for; the device has not answered yet.
    Starting,
    Running,
    /// Cancel was pressed; the device has not stopped yet.
    Cancelling,
    Saved,
}

impl Download {
    fn new() -> Self {
        Download { name: String::new(), path: None, bytes: 0, estimate: None, range: None, phase: DownloadPhase::Starting }
    }

    /// Still reading from the device.
    fn running(&self) -> bool {
        self.phase != DownloadPhase::Saved
    }

    /// What the row under the controls says.
    fn text(&self) -> String {
        match self.phase {
            DownloadPhase::Starting => "Starting the download…".to_string(),
            DownloadPhase::Running => format!("Downloading {}", self.name),
            DownloadPhase::Cancelling => "Cancelling the download…".to_string(),
            DownloadPhase::Saved => format!("Saved {}", self.name),
        }
    }
}

/// What the bar shows at most while the download runs: the device's
/// estimate is only that (the devices deliver up to a third more than
/// it says), so the bar never fills before the file is saved.
const RUNNING_FRACTION_MAX: f64 = 0.99;

/// "1.2 MB of 3.4 MB · 35 %" and the fraction (at most
/// `RUNNING_FRACTION_MAX`: a download past the device's estimate reads
/// "99 %" until it is saved), or "1.2 MB" and none without an estimate
/// (the bar then pulses).
fn progress_of(bytes: u64, estimate: Option<u64>) -> (String, Option<f64>) {
    match estimate.filter(|&e| e > 0) {
        Some(estimate) => {
            let fraction = (bytes as f64 / estimate as f64).min(RUNNING_FRACTION_MAX);
            (format!("{} of {} · {:.0} %", size_text(bytes), size_text(estimate), fraction * 100.0), Some(fraction))
        }
        None => (size_text(bytes), None),
    }
}

pub struct PlaybackPage {
    root: GtkBox,
    video: Rc<VideoView>,
    timeline: Rc<PlaybackTimeline>,
    message: Label,
    spinner: Spinner,
    notice: Label,
    notice_generation: Rc<Cell<u64>>,
    sidebar_toggle: ToggleButton,
    prev_day: Button,
    next_day: Button,
    date: MenuButton,
    popover: Popover,
    calendar: Calendar,
    play: Button,
    time: Label,
    whole_day: Button,
    speed: DropDown,
    stream: DropDown,
    prev_event: Button,
    next_event: Button,
    filter: DropDown,
    download_button: Button,
    download_row: GtkBox,
    download_label: Label,
    download_bar: ProgressBar,
    download_cancel: Button,
    download_open: Button,
    download_dismiss: Button,
    handlers: Handlers,

    /// The page is the visible one (set and cleared by `enter`/`leave` only).
    shown: Cell<bool>,
    /// The device key and channel on the page.
    device: RefCell<Option<(String, u8)>>,
    readiness: Cell<Readiness>,
    /// Why the device's connection went away, while it is away.
    lost: RefCell<Option<String>>,
    /// The current session; events of any other are stale.
    session: Cell<u64>,
    /// Midnight of the selected day (device-local).
    day: Cell<Option<CivilTime>>,
    /// The selected day's recordings: `None` while they are being listed.
    files: RefCell<Option<Result<Vec<FileSpan>, String>>>,
    /// The stream `files` were listed for.
    files_stream: Cell<RecordStream>,
    /// The selected day's event windows (every kind; the filter is applied
    /// when they are shown): `None` while they are being listed.
    events: RefCell<Option<Result<Vec<RecordedEvent>, String>>>,
    /// A failed event listing was reported in this session: the next ones
    /// (every day and stream lists anew) are not, the day plays all the same.
    events_failed: Cell<bool>,
    /// Recorded days per (device, channel, year, month), and when they came.
    months: RefCell<HashMap<(String, u8, u16, u8), (Vec<u8>, Instant)>>,
    /// Months asked for and not answered yet (device and channel on the page).
    asked: RefCell<HashSet<(u16, u8)>>,
    /// The month the calendar showed before its latest change.
    shown_month: Cell<Option<(u16, u8)>>,
    month_sync_pending: Cell<bool>,
    choosing: Cell<Option<Choosing>>,
    /// Why no day is selected, when that needs saying.
    day_note: RefCell<Option<String>>,
    /// The device thread's last word on the player (current session only).
    state: RefCell<Option<PlayerState>>,
    /// What the play button stands for: playing (it shows Pause) or not.
    playing: Cell<bool>,
    /// The playback generation of the page's last Play, Pause or seek (0:
    /// none in this session): a state from before it leaves `playing` as the
    /// user set it.
    command_gen: Cell<u64>,
    pending_seek: Cell<Option<PendingSeek>>,
    last_error: RefCell<Option<String>>,
    want_speed: Cell<ReplaySpeed>,
    want_stream: Cell<RecordStream>,
    /// The detection filter; it outlives sessions (it asks nothing of the
    /// device).
    want_filter: Cell<EventFilter>,
    /// Kept while the device is away.
    remembered: Cell<Option<Remembered>>,
    /// To be applied once the session's recordings are listed.
    restore: Cell<Option<Remembered>>,
    timer: RefCell<Option<glib::SourceId>>,
    /// Set while code, not the user, changes a control.
    updating: Cell<bool>,
    /// The download started in this session (see `on_download`).
    download: RefCell<Option<Download>>,
    /// The download dialog is open: the range it stands for shows on the
    /// timeline.
    dialog_open: Cell<bool>,
}

impl PlaybackPage {
    pub fn new(video: Rc<VideoView>, handlers: Handlers) -> Rc<Self> {
        // The picture, with the status message, the buffering spinner and
        // short notices over it (as on the live page).
        let message = Label::new(Some("Select a device"));
        message.add_css_class("title-2");
        message.add_css_class("dim-label");
        message.set_can_target(false);
        message.set_halign(gtk4::Align::Center);
        message.set_valign(gtk4::Align::Center);
        message.set_wrap(true);
        message.set_justify(gtk4::Justification::Center);
        let overlay = Overlay::new();
        overlay.set_hexpand(true);
        overlay.set_vexpand(true);
        let picture = video.widget();
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        overlay.set_child(Some(picture));
        overlay.add_overlay(&message);
        let spinner = Spinner::new();
        spinner.set_size_request(48, 48);
        spinner.set_halign(gtk4::Align::Center);
        spinner.set_valign(gtk4::Align::Center);
        spinner.set_can_target(false);
        spinner.set_visible(false);
        overlay.add_overlay(&spinner);
        let notice = Label::new(None);
        notice.add_css_class("osd");
        notice.set_halign(gtk4::Align::Center);
        notice.set_valign(gtk4::Align::Start);
        notice.set_margin_top(12);
        notice.set_visible(false);
        notice.set_can_target(false);
        overlay.add_overlay(&notice);

        // The day's timeline.
        let timeline = PlaybackTimeline::new();
        let whole_day = Button::from_icon_name("zoom-fit-best-symbolic");
        whole_day.set_tooltip_text(Some("Whole day"));
        whole_day.set_valign(gtk4::Align::Center);
        let timeline_row = GtkBox::new(Orientation::Horizontal, 8);
        timeline_row.set_margin_top(6);
        timeline_row.set_margin_bottom(6);
        timeline_row.set_margin_start(8);
        timeline_row.set_margin_end(8);
        timeline_row.append(timeline.widget());
        timeline_row.append(&whole_day);

        // Day, play/pause and time, speed and stream.
        let sidebar_toggle = ToggleButton::new();
        sidebar_toggle.set_tooltip_text(Some("Show or hide the device list"));
        let prev_day = Button::from_icon_name("go-previous-symbolic");
        prev_day.set_tooltip_text(Some("Previous day"));
        let calendar = Calendar::new();
        let popover = Popover::new();
        popover.set_child(Some(&calendar));
        let date = MenuButton::new();
        date.set_label("Choose a day");
        date.set_always_show_arrow(true);
        date.set_tooltip_text(Some("Choose a day"));
        date.set_popover(Some(&popover));
        let next_day = Button::from_icon_name("go-next-symbolic");
        next_day.set_tooltip_text(Some("Next day"));
        let play = Button::from_icon_name("media-playback-start-symbolic");
        play.set_tooltip_text(Some("Play"));
        let time = Label::new(None);
        time.add_css_class("numeric");
        time.set_width_chars(19);
        time.set_xalign(0.0);
        let speed_labels: Vec<String> = ReplaySpeed::ALL.iter().map(|s| speed_label(*s)).collect();
        let speed_names: Vec<&str> = speed_labels.iter().map(String::as_str).collect();
        let speed = DropDown::from_strings(&speed_names);
        speed.set_selected(speed_index(ReplaySpeed::X1));
        speed.set_tooltip_text(Some("Speed"));
        let stream = DropDown::from_strings(&STREAM_LABELS);
        stream.set_selected(stream_index(RecordStream::Sub));
        stream.set_tooltip_text(Some("Stream"));
        let prev_event = Button::from_icon_name("media-skip-backward-symbolic");
        prev_event.set_tooltip_text(Some("Previous event"));
        let next_event = Button::from_icon_name("media-skip-forward-symbolic");
        next_event.set_tooltip_text(Some("Next event"));
        let filter_labels: Vec<&str> = EventFilter::ALL.iter().map(|f| f.label()).collect();
        let filter = DropDown::from_strings(&filter_labels);
        filter.set_selected(filter_index(EventFilter::All));
        filter.set_tooltip_text(Some("Detection filter"));
        let download_button = Button::from_icon_name("document-save-symbolic");
        download_button.set_tooltip_text(Some("Download a recording"));
        let controls = GtkBox::new(Orientation::Horizontal, 8);
        controls.set_margin_top(6);
        controls.set_margin_bottom(6);
        controls.set_margin_start(8);
        controls.set_margin_end(8);
        controls.append(&sidebar_toggle);
        controls.append(&prev_day);
        controls.append(&date);
        controls.append(&next_day);
        controls.append(&gtk4::Separator::new(Orientation::Vertical));
        controls.append(&play);
        controls.append(&time);
        controls.append(&gtk4::Separator::new(Orientation::Vertical));
        controls.append(&prev_event);
        controls.append(&filter);
        controls.append(&next_event);
        let spacer = GtkBox::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        controls.append(&spacer);
        controls.append(&speed);
        controls.append(&stream);
        controls.append(&download_button);

        // The download, under the controls while it runs and once it is
        // saved: what, how far, and Cancel; then Open folder.
        let download_label = Label::new(None);
        download_label.set_xalign(0.0);
        download_label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
        let download_bar = ProgressBar::new();
        download_bar.set_hexpand(true);
        download_bar.set_valign(gtk4::Align::Center);
        download_bar.set_show_text(true);
        let download_cancel = Button::with_label("Cancel");
        let download_open = Button::with_label("Open folder");
        let download_dismiss = Button::from_icon_name("window-close-symbolic");
        download_dismiss.set_tooltip_text(Some("Dismiss"));
        let download_row = GtkBox::new(Orientation::Horizontal, 8);
        download_row.set_margin_bottom(6);
        download_row.set_margin_start(8);
        download_row.set_margin_end(8);
        download_row.append(&download_label);
        download_row.append(&download_bar);
        download_row.append(&download_cancel);
        download_row.append(&download_open);
        download_row.append(&download_dismiss);
        download_row.set_visible(false);

        let root = GtkBox::new(Orientation::Vertical, 0);
        root.append(&overlay);
        root.append(&timeline_row);
        root.append(&controls);
        root.append(&download_row);

        let this = Rc::new(Self {
            root,
            video,
            timeline,
            message,
            spinner,
            notice,
            notice_generation: Rc::new(Cell::new(0)),
            sidebar_toggle,
            prev_day,
            next_day,
            date,
            popover,
            calendar,
            play,
            time,
            whole_day,
            speed,
            stream,
            prev_event,
            next_event,
            filter,
            download_button,
            download_row,
            download_label,
            download_bar,
            download_cancel,
            download_open,
            download_dismiss,
            handlers,
            shown: Cell::new(false),
            device: RefCell::new(None),
            readiness: Cell::new(Readiness::Connecting),
            lost: RefCell::new(None),
            session: Cell::new(0),
            day: Cell::new(None),
            files: RefCell::new(None),
            files_stream: Cell::new(RecordStream::Sub),
            events: RefCell::new(None),
            events_failed: Cell::new(false),
            months: RefCell::new(HashMap::new()),
            asked: RefCell::new(HashSet::new()),
            shown_month: Cell::new(None),
            month_sync_pending: Cell::new(false),
            choosing: Cell::new(None),
            day_note: RefCell::new(None),
            state: RefCell::new(None),
            playing: Cell::new(false),
            command_gen: Cell::new(0),
            pending_seek: Cell::new(None),
            last_error: RefCell::new(None),
            want_speed: Cell::new(ReplaySpeed::X1),
            want_stream: Cell::new(RecordStream::Sub),
            want_filter: Cell::new(EventFilter::All),
            remembered: Cell::new(None),
            restore: Cell::new(None),
            timer: RefCell::new(None),
            updating: Cell::new(false),
            download: RefCell::new(None),
            dialog_open: Cell::new(false),
        });
        this.wire();
        this.refresh();
        this
    }

    fn wire(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.timeline.connect_seek_requested(move |seconds| {
            if let Some(p) = weak.upgrade() {
                p.seek_to(seconds);
            }
        });
        let weak = Rc::downgrade(self);
        self.whole_day.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.timeline.reset_view();
            }
        });
        let weak = Rc::downgrade(self);
        self.play.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.toggle_play();
            }
        });
        let weak = Rc::downgrade(self);
        self.prev_day.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.step_day(-1);
            }
        });
        let weak = Rc::downgrade(self);
        self.next_day.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.step_day(1);
            }
        });
        let weak = Rc::downgrade(self);
        self.speed.connect_selected_notify(move |dropdown| {
            if let Some(p) = weak.upgrade() {
                if !p.updating.get() {
                    let chosen = ReplaySpeed::ALL.get(dropdown.selected() as usize).copied();
                    if let Some(speed) = chosen {
                        p.speed_chosen(speed);
                    }
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.stream.connect_selected_notify(move |dropdown| {
            if let Some(p) = weak.upgrade() {
                if !p.updating.get() {
                    if let Some(stream) = STREAMS.get(dropdown.selected() as usize).copied() {
                        p.stream_chosen(stream);
                    }
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.filter.connect_selected_notify(move |dropdown| {
            if let Some(p) = weak.upgrade() {
                if !p.updating.get() {
                    if let Some(filter) = EventFilter::ALL.get(dropdown.selected() as usize).copied() {
                        p.filter_chosen(filter);
                    }
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.prev_event.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.jump_event(false);
            }
        });
        let weak = Rc::downgrade(self);
        self.next_event.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.jump_event(true);
            }
        });
        let weak = Rc::downgrade(self);
        self.download_button.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.open_download_dialog();
            }
        });
        let weak = Rc::downgrade(self);
        self.download_cancel.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.cancel_download();
            }
        });
        let weak = Rc::downgrade(self);
        self.download_open.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.open_download_folder();
            }
        });
        let weak = Rc::downgrade(self);
        self.download_dismiss.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                *p.download.borrow_mut() = None;
                p.refresh_selection();
                p.refresh();
            }
        });

        // A pick in the calendar. Whether a `day-selected` is a pick or part
        // of moving to another month is told by the month shown before:
        // `shown_month` only follows the calendar from an idle, after the
        // whole burst of signals, so the order GTK emits them in does not
        // matter.
        let weak = Rc::downgrade(self);
        self.calendar.connect_day_selected(move |calendar| {
            let Some(p) = weak.upgrade() else { return };
            if p.updating.get() {
                return;
            }
            let date = calendar.date();
            let Some(day) = day_of(date.year(), date.month(), date.day_of_month()) else { return };
            if p.shown_month.get() != Some((day.year, day.month)) {
                return; // moving to another month, not a pick
            }
            p.popover.popdown();
            p.open_day(day);
        });
        for property in ["month", "year"] {
            let weak = Rc::downgrade(self);
            self.calendar.connect_notify_local(Some(property), move |_, _| {
                if let Some(p) = weak.upgrade() {
                    p.calendar_moved_soon();
                }
            });
        }
        // Each time it opens, the calendar shows the selected day's month.
        let weak = Rc::downgrade(self);
        self.popover.connect_show(move |_| {
            if let Some(p) = weak.upgrade() {
                if let Some(day) = p.day.get() {
                    p.show_in_calendar(day);
                }
            }
        });
    }

    pub fn widget(&self) -> &GtkBox {
        &self.root
    }

    /// The page's own sidebar button; the window binds it to the live
    /// page's, so both show and hide the same sidebar.
    pub fn sidebar_toggle(&self) -> &ToggleButton {
        &self.sidebar_toggle
    }

    pub fn is_shown(&self) -> bool {
        self.shown.get()
    }

    pub fn device_key(&self) -> Option<String> {
        self.device.borrow().as_ref().map(|(key, _)| key.clone())
    }

    /// Whether footage of playback session `session` may be shown: the page
    /// is visible and that is its current session (sessions are numbered
    /// across devices, so this also tells the page's device from others).
    pub fn shows_session(&self, session: u64) -> bool {
        self.shown.get() && self.session.get() == session
    }

    /// The page became visible, with this device and channel (`None`: no
    /// device). It opens on the latest recorded day, at 1x, on the sub
    /// stream, paused.
    pub fn enter(self: &Rc<Self>, device: Option<(&str, u8)>, readiness: Readiness) {
        self.shown.set(true);
        self.want_speed.set(ReplaySpeed::X1);
        self.want_stream.set(RecordStream::Sub);
        self.updating.set(true);
        self.speed.set_selected(speed_index(ReplaySpeed::X1));
        self.stream.set_selected(stream_index(RecordStream::Sub));
        self.updating.set(false);
        *self.device.borrow_mut() = device.map(|(key, channel)| (key.to_string(), channel));
        self.readiness.set(readiness);
        *self.lost.borrow_mut() = None;
        self.remembered.set(None);
        self.forget_marks();
        self.clear_session();
        self.ensure_timer();
        if self.is_ready() {
            self.start_default();
        }
        self.refresh();
    }

    /// The page was hidden: ends the playback on the device, clears the
    /// picture and stops the cursor. `shown` goes first, so that the live
    /// view the window starts next is not held back by the page.
    pub fn leave(&self) {
        self.shown.set(false);
        self.send(|link| link.playback_close());
        self.stop_timer();
        self.remembered.set(None);
        self.clear_session();
        self.refresh();
    }

    /// A device or channel picked in the sidebar while the page is shown.
    /// The same one again changes nothing (or, while it is away, waits for
    /// it to come back). Another channel of the same device keeps the day.
    pub fn select(self: &Rc<Self>, key: &str, channel: u8, readiness: Readiness) {
        if !self.shown.get() {
            return;
        }
        let current = self.device.borrow().clone();
        let same_device = current.as_ref().is_some_and(|(k, _)| k == key);
        if same_device && current.as_ref().is_some_and(|(_, c)| *c == channel) {
            if !self.is_ready() {
                *self.lost.borrow_mut() = None;
                self.set_readiness(key, readiness);
            }
            return;
        }
        if !same_device {
            // One playback per device: the old one ends first.
            self.send(|link| link.playback_close());
        }
        let keep = if same_device { self.day.get() } else { None };
        *self.device.borrow_mut() = Some((key.to_string(), channel));
        self.readiness.set(readiness);
        *self.lost.borrow_mut() = None;
        self.remembered.set(None);
        self.forget_marks();
        self.clear_session();
        if let Some(day) = keep {
            // Shown at once, listed once the device is ready.
            self.day.set(Some(day));
            self.date.set_label(&date_text(day));
            self.timeline.set_day(Some((day.year, day.month, day.day)));
        }
        self.ensure_timer();
        if self.is_ready() {
            self.start();
        }
        self.refresh();
    }

    /// How far the page's device is (connected, channel list in). Starts
    /// the session once it becomes ready; nothing changes for a ready
    /// device telling it again (a device re-sends its channel list).
    pub fn set_readiness(self: &Rc<Self>, key: &str, readiness: Readiness) {
        if !self.shown.get() || self.device_key().as_deref() != Some(key) {
            return;
        }
        let was = self.readiness.replace(readiness);
        if readiness == Readiness::Ready && was != Readiness::Ready {
            *self.lost.borrow_mut() = None;
            self.ensure_timer();
            self.start();
        }
        self.refresh();
    }

    /// The page's device lost its connection: the picture stays frozen, the
    /// day and the position are kept for when it is back.
    pub fn device_lost(&self, key: &str, reason: &str) {
        if self.suspend(key) {
            *self.lost.borrow_mut() = Some(reason.to_string());
            self.refresh();
        }
    }

    /// The page's device is connecting again (a new login): as a loss, but
    /// the page only says it is connecting.
    pub fn device_reconnecting(&self, key: &str) {
        if self.suspend(key) {
            *self.lost.borrow_mut() = None;
            self.refresh();
        }
    }

    /// The page's device was removed.
    pub fn device_removed(&self, key: &str) {
        if self.device_key().as_deref() != Some(key) {
            return;
        }
        self.stop_timer();
        *self.device.borrow_mut() = None;
        *self.lost.borrow_mut() = None;
        self.remembered.set(None);
        self.forget_marks();
        self.months.borrow_mut().retain(|(k, ..), _| k != key);
        self.clear_session();
        self.refresh();
    }

    pub fn on_event(self: &Rc<Self>, key: &str, event: PlaybackEvent) {
        if !self.shown.get() || self.device_key().as_deref() != Some(key) {
            return;
        }
        match event {
            PlaybackEvent::Month { channel, year, month, days } => self.month_answer(channel, year, month, days),
            PlaybackEvent::Day { session, stream, files, .. } => {
                if session == self.session.get() && stream == self.want_stream.get() {
                    self.day_answer(files);
                }
            }
            PlaybackEvent::Events { session, stream, events, .. } => {
                if session == self.session.get() && stream == self.want_stream.get() {
                    self.events_answer(events);
                }
            }
            PlaybackEvent::State(state) => {
                if state.session == self.session.get() {
                    self.state_changed(state);
                }
            }
            PlaybackEvent::Error { session, message } => {
                if session == self.session.get() {
                    self.pending_seek.set(None);
                    // What really plays after a refusal or a seek past the
                    // end (kept as it was once the stream broke off). A state
                    // from before the last Play, Pause or seek does not say;
                    // the state that follows the error does.
                    let phase = self.state.borrow().as_ref().filter(|s| self.is_current(s)).map(|s| s.phase);
                    if let Some(phase) = phase.filter(|&phase| phase != PlayerPhase::Failed) {
                        self.playing.set(plays(phase));
                    }
                    // On a day listed without recordings the message over the
                    // picture says so already.
                    if !matches!(&*self.files.borrow(), Some(Ok(files)) if files.is_empty()) {
                        self.notify(&message);
                    }
                    *self.last_error.borrow_mut() = Some(message);
                    self.refresh();
                }
            }
            PlaybackEvent::Notice { session, message } => {
                if session == self.session.get() {
                    self.notify(&message);
                }
            }
        }
    }

    // ---- sessions -----------------------------------------------------------

    fn is_ready(&self) -> bool {
        self.device.borrow().is_some() && self.readiness.get() == Readiness::Ready && self.lost.borrow().is_none()
    }

    /// Sends a command to the page's device.
    fn send(&self, f: impl Fn(&DeviceLink)) {
        let key = self.device_key();
        if let Some(key) = key {
            (self.handlers.with_link)(&key, &f);
        }
    }

    /// Sends a Play, Pause or seek (`f` returns the generation of the last
    /// one it sent) and notes it in `command_gen`.
    fn send_player(&self, f: impl Fn(&DeviceLink) -> u64) {
        let gen = Cell::new(None);
        self.send(|link| gen.set(Some(f(link))));
        if let Some(gen) = gen.get() {
            self.command_gen.set(gen);
        }
    }

    /// Whether `state` was sent after the device thread took up the page's
    /// last Play, Pause or seek.
    fn is_current(&self, state: &PlayerState) -> bool {
        state.gen >= self.command_gen.get()
    }

    /// Forgets the session: its events are stale from now on, the picture
    /// and the timeline are cleared.
    fn clear_session(&self) {
        self.session.set(self.session.get() + 1);
        self.day.set(None);
        *self.files.borrow_mut() = None;
        *self.events.borrow_mut() = None;
        self.events_failed.set(false);
        *self.state.borrow_mut() = None;
        *self.last_error.borrow_mut() = None;
        *self.day_note.borrow_mut() = None;
        self.choosing.set(None);
        self.restore.set(None);
        self.pending_seek.set(None);
        self.playing.set(false);
        self.command_gen.set(0);
        // A download under way runs on; its progress belonged to the
        // session it started in.
        *self.download.borrow_mut() = None;
        self.video.reset();
        self.timeline.set_day(None);
        self.date.set_label("Choose a day");
        self.time.set_text("");
    }

    /// A ready device: back where it was if it was lost, else the selected
    /// day, else the latest recorded day.
    fn start(self: &Rc<Self>) {
        if let Some(remembered) = self.remembered.take() {
            self.open_day(remembered.day);
            self.restore.set(Some(remembered));
        } else if let Some(day) = self.day.get() {
            self.open_day(day);
        } else {
            self.start_default();
        }
    }

    /// Looks for the latest recorded day, starting with the host's month
    /// (only a starting point: the days are the device's own).
    fn start_default(self: &Rc<Self>) {
        let Ok(now) = glib::DateTime::now_local() else { return };
        let Some(today) = day_of(now.year(), now.month(), now.day_of_month()) else { return };
        *self.day_note.borrow_mut() = None;
        self.choosing.set(Some(Choosing { year: today.year, month: today.month, second: false }));
        self.show_in_calendar(today);
        self.ensure_month(today.year, today.month);
        self.refresh();
    }

    /// Starts a session on `day`: lists its recordings, plays nothing.
    fn open_day(self: &Rc<Self>, day: CivilTime) {
        let day = CivilTime::start_of_day(day.year, day.month, day.day);
        let Some((_, channel)) = self.device.borrow().clone() else { return };
        self.clear_session();
        self.day.set(Some(day));
        self.date.set_label(&date_text(day));
        self.timeline.set_day(Some((day.year, day.month, day.day)));
        self.show_in_calendar(day);
        self.ensure_timer();
        self.refresh();
        if !self.is_ready() {
            return;
        }
        let (session, stream, speed) = (self.session.get(), self.want_stream.get(), self.want_speed.get());
        self.send(|link| {
            link.playback_day(session, channel, day, stream);
            // A device's player starts at 1x.
            if speed != ReplaySpeed::X1 {
                link.playback_speed(speed);
            }
        });
    }

    fn step_day(self: &Rc<Self>, delta: i64) {
        if let Some(day) = self.day.get() {
            self.open_day(day_after(day, delta));
        }
    }

    /// Marks the device as away; `false` if it is not the page's.
    fn suspend(&self, key: &str) -> bool {
        if !self.shown.get() || self.device_key().as_deref() != Some(key) {
            return false;
        }
        if self.remembered.get().is_none() {
            if let Some(day) = self.day.get() {
                let position = self.screen_position(day).map(|(_, at)| at);
                self.remembered.set(Some(Remembered { day, position, was_playing: self.playing.get() }));
            }
        }
        self.video.freeze();
        self.stop_timer();
        self.readiness.set(Readiness::Connecting);
        // Nothing more comes from that connection; a new one starts anew.
        self.session.set(self.session.get() + 1);
        *self.state.borrow_mut() = None;
        self.pending_seek.set(None);
        self.choosing.set(None);
        self.restore.set(None);
        self.playing.set(false);
        self.command_gen.set(0);
        self.forget_marks();
        self.months.borrow_mut().retain(|(k, ..), _| k != key);
        // A download went with the connection (its partial file is
        // removed there); its row would wait for an end that never comes.
        if self.download_running() {
            self.notify("Download abandoned: connection lost");
        }
        *self.download.borrow_mut() = None;
        self.refresh_selection();
        true
    }

    // ---- the player -----------------------------------------------------------

    /// A click on the timeline (or an arrow key): plays from there. The
    /// cursor goes there at once.
    fn seek_to(&self, seconds: f64) {
        let Some(day) = self.day.get() else { return };
        // A day listed without recordings has nothing to play (the page says
        // so already); while it is still being listed, the seek goes out.
        if !self.is_ready() || matches!(&*self.files.borrow(), Some(Ok(files)) if files.is_empty()) {
            return;
        }
        let at = civil_at(day, seconds);
        self.pending_seek.set(Some(PendingSeek { seconds, at, since: Instant::now() }));
        self.timeline.set_position(Some(seconds));
        self.time.set_text(&time_text(at));
        self.playing.set(true);
        self.send_player(|link| link.playback_seek(at));
        self.refresh();
    }

    fn toggle_play(&self) {
        if !self.is_ready() || !self.has_files() {
            return;
        }
        if self.playing.get() {
            // At once, even if the device's thread is busy.
            self.video.freeze();
            self.playing.set(false);
            self.send_player(DeviceLink::playback_pause);
        } else {
            self.playing.set(true);
            self.send_player(DeviceLink::playback_play);
        }
        self.refresh();
    }

    fn speed_chosen(&self, speed: ReplaySpeed) {
        self.want_speed.set(speed);
        if self.is_ready() && self.day.get().is_some() {
            self.send(|link| link.playback_speed(speed));
        }
    }

    /// The other stream has other files: the day is listed again, and the
    /// timeline keeps the old ones until they come.
    fn stream_chosen(&self, stream: RecordStream) {
        self.want_stream.set(stream);
        if self.is_ready() && self.day.get().is_some() {
            self.send(|link| link.playback_stream(stream));
        }
    }

    fn day_answer(self: &Rc<Self>, files: Result<Vec<FileSpan>, String>) {
        let Some(day) = self.day.get() else { return };
        if let Ok(files) = &files {
            self.timeline.set_files(&spans_of(files, day));
        }
        let listed = files.as_ref().is_ok_and(|files| !files.is_empty());
        *self.files.borrow_mut() = Some(files);
        self.files_stream.set(self.want_stream.get());
        if let Some(back) = self.restore.take() {
            // Back after a lost connection: where it was, as it was.
            if let (Some(at), true) = (back.position, listed) {
                let seconds = seconds_of_day(timeline_of(at), day);
                self.pending_seek.set(Some(PendingSeek { seconds, at, since: Instant::now() }));
                self.playing.set(back.was_playing);
                self.send_player(|link| {
                    let gen = link.playback_seek(at);
                    if back.was_playing {
                        gen
                    } else {
                        link.playback_pause()
                    }
                });
            }
        }
        self.refresh();
    }

    fn state_changed(&self, state: PlayerState) {
        // A state sent before the device thread saw the page's last Play,
        // Pause or seek says nothing about them: the button and the cursor
        // stay as the user left them.
        let current = self.is_current(&state);
        // The seek has been taken up once the player is anything but still
        // playing the footage from before it.
        if current && state.phase != PlayerPhase::Playing {
            self.pending_seek.set(None);
        }
        // A broken-off stream is followed by the lost connection: whether it
        // was playing is what the page remembers for when the device is back.
        if current && state.phase != PlayerPhase::Failed {
            self.playing.set(plays(state.phase));
        }
        *self.state.borrow_mut() = Some(state);
        self.refresh();
    }

    fn has_files(&self) -> bool {
        matches!(&*self.files.borrow(), Some(Ok(files)) if !files.is_empty())
    }

    // ---- events ---------------------------------------------------------------

    /// The day's event windows came (after its recordings): the bars show
    /// under the filter. The ones of the stream before stay until then. A
    /// failure is said once per session (the day plays without the bars).
    fn events_answer(&self, events: Result<Vec<RecordedEvent>, String>) {
        if let Err(e) = &events {
            if !self.events_failed.replace(true) {
                self.notify(&format!("Could not list the events: {e}"));
            }
        }
        *self.events.borrow_mut() = Some(events);
        self.show_events();
        self.refresh();
    }

    /// The filter changed: the bars follow at once; the device is not asked.
    fn filter_chosen(&self, filter: EventFilter) {
        self.want_filter.set(filter);
        self.show_events();
        self.refresh();
    }

    /// The timeline marks the events the filter admits.
    fn show_events(&self) {
        let Some(day) = self.day.get() else { return };
        let shown = match &*self.events.borrow() {
            Some(Ok(events)) => timeline_events(events, self.want_filter.get(), day),
            _ => Vec::new(),
        };
        self.timeline.set_events(&shown);
    }

    /// Whether the filter admits any of the day's events.
    fn has_matching_events(&self) -> bool {
        let filter = self.want_filter.get();
        matches!(&*self.events.borrow(), Some(Ok(events)) if events.iter().any(|e| filter.admits(e)))
    }

    /// The previous or next event button: plays from the start of the
    /// nearest event the filter admits before or after the position on
    /// screen (with none, the day's last or first).
    fn jump_event(&self, forward: bool) {
        let Some(day) = self.day.get() else { return };
        let starts = match &*self.events.borrow() {
            Some(Ok(events)) => event_starts(events, self.want_filter.get(), day),
            _ => Vec::new(),
        };
        let position = self.screen_position(day).map(|(seconds, _)| seconds);
        let target =
            if forward { next_event_start(&starts, position) } else { previous_event_start(&starts, position) };
        match target {
            Some(at) => self.seek_to(at),
            None => self.notify(if forward { "No later event" } else { "No earlier event" }),
        }
    }

    // ---- calendar -------------------------------------------------------------

    /// Selects `day` in the calendar (no pick) and shows its month.
    fn show_in_calendar(self: &Rc<Self>, day: CivilTime) {
        if let Ok(date) = glib::DateTime::from_local(i32::from(day.year), i32::from(day.month), i32::from(day.day), 12, 0, 0.0) {
            self.updating.set(true);
            self.calendar.select_day(&date);
            self.updating.set(false);
        }
        if self.shown_month.get() != Some((day.year, day.month)) {
            self.month_shown(day.year, day.month);
        }
    }

    /// The calendar moved to another month or year: follow it once the
    /// signals of that move are all out.
    fn calendar_moved_soon(self: &Rc<Self>) {
        if self.month_sync_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(p) = weak.upgrade() {
                p.month_sync_pending.set(false);
                let year = p.calendar.property::<i32>("year");
                let month = p.calendar.property::<i32>("month") + 1;
                if let (Ok(year), Ok(month)) = (u16::try_from(year), u8::try_from(month)) {
                    if p.shown_month.get() != Some((year, month)) {
                        p.month_shown(year, month);
                    }
                }
            }
        });
    }

    /// The calendar shows `year`/`month`: its marks are the known recorded
    /// days, asked for when not known or no longer fresh.
    fn month_shown(self: &Rc<Self>, year: u16, month: u8) {
        self.shown_month.set(Some((year, month)));
        let known = self.cached_month(year, month).map(|(days, _)| days);
        self.mark_days(known.as_deref().unwrap_or(&[]));
        self.ensure_month(year, month);
    }

    fn mark_days(&self, days: &[u8]) {
        self.calendar.clear_marks();
        for day in days {
            self.calendar.mark_day(u32::from(*day));
        }
    }

    /// Another device, channel or connection: the marks are marked again
    /// (and asked for) the next time a month is shown, whichever it is.
    fn forget_marks(&self) {
        self.asked.borrow_mut().clear();
        self.shown_month.set(None);
        self.calendar.clear_marks();
    }

    fn cached_month(&self, year: u16, month: u8) -> Option<(Vec<u8>, Instant)> {
        let (key, channel) = self.device.borrow().clone()?;
        self.months.borrow().get(&(key, channel, year, month)).cloned()
    }

    /// Uses the month's days if they are fresh, else asks for them.
    fn ensure_month(self: &Rc<Self>, year: u16, month: u8) {
        if !self.is_ready() {
            return;
        }
        let Some((_, channel)) = self.device.borrow().clone() else { return };
        match self.cached_month(year, month) {
            Some((days, fetched)) if is_fresh(fetched, Instant::now()) => self.month_known(year, month, &days),
            _ => {
                if self.asked.borrow_mut().insert((year, month)) {
                    self.send(|link| link.playback_month(channel, year, month));
                }
            }
        }
    }

    fn month_answer(self: &Rc<Self>, channel: u8, year: u16, month: u8, days: Result<Vec<u8>, String>) {
        let Some((key, current)) = self.device.borrow().clone() else { return };
        if channel != current {
            return;
        }
        self.asked.borrow_mut().remove(&(year, month));
        match days {
            Ok(days) => {
                self.months.borrow_mut().insert((key, channel, year, month), (days.clone(), Instant::now()));
                self.month_known(year, month, &days);
            }
            Err(e) => {
                let text = format!("Could not list the recordings: {e}");
                if self.choosing.get().is_some_and(|c| (c.year, c.month) == (year, month)) {
                    self.choosing.set(None);
                    *self.day_note.borrow_mut() = Some(text);
                    self.refresh();
                } else {
                    self.notify(&text);
                }
            }
        }
    }

    /// A month's recorded days are known: marked if shown, and the latest
    /// one picked while looking for the default day.
    fn month_known(self: &Rc<Self>, year: u16, month: u8, days: &[u8]) {
        if self.shown_month.get() == Some((year, month)) {
            self.mark_days(days);
        }
        let Some(choosing) = self.choosing.get().filter(|c| (c.year, c.month) == (year, month)) else { return };
        if let Some(day) = latest_day(year, month, days) {
            self.open_day(day);
        } else if !choosing.second {
            let (year, month) = previous_month(year, month);
            self.choosing.set(Some(Choosing { year, month, second: true }));
            self.ensure_month(year, month);
        } else {
            self.choosing.set(None);
            *self.day_note.borrow_mut() = Some("No recordings in the last two months".to_string());
            self.refresh();
        }
    }

    // ---- what is on screen ----------------------------------------------------

    fn ensure_timer(self: &Rc<Self>) {
        if self.timer.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        // Never stops by itself: `stop_timer` is the only way it ends.
        let id = glib::timeout_add_local(CURSOR_TICK, move || {
            if let Some(p) = weak.upgrade() {
                p.tick();
            }
            glib::ControlFlow::Continue
        });
        *self.timer.borrow_mut() = Some(id);
    }

    fn stop_timer(&self) {
        if let Some(id) = self.timer.borrow_mut().take() {
            id.remove();
        }
    }

    /// The cursor and the time label follow the picture.
    fn tick(&self) {
        self.pulse_download();
        let Some(day) = self.day.get() else { return };
        let position = self.screen_position(day);
        self.timeline.set_position(position.map(|(seconds, _)| seconds));
        let text = position.map(|(_, at)| time_text(at)).unwrap_or_default();
        if self.time.text() != text {
            self.time.set_text(&text);
        }
        self.refresh();
    }

    /// Where the picture is (seconds of `day`, and the device-local time):
    /// a seek not taken up yet, else the footage on screen, else the
    /// player's last target.
    fn screen_position(&self, day: CivilTime) -> Option<(f64, CivilTime)> {
        if let Some(seek) = self.pending_seek.get() {
            if seek.since.elapsed() < SEEK_HOLD {
                return Some((seek.seconds, seek.at));
            }
            self.pending_seek.set(None);
        }
        let shown = self
            .video
            .current_sink()
            .and_then(|sink| sink.replay_status())
            .map(|status| status.position)
            .filter(|position| !position.is_zero());
        if let Some(position) = shown {
            return Some((seconds_of_day(position, day), civil_of(position)));
        }
        let at = self.state.borrow().as_ref().and_then(|state| state.at)?;
        Some((seconds_of_day(timeline_of(at), day), at))
    }

    /// Whether the picture has played out what it held.
    fn played_out(&self) -> bool {
        self.video
            .current_sink()
            .and_then(|sink| sink.replay_status())
            .map_or(true, |status| status.buffered < PLAYED_OUT_BELOW)
    }

    /// Brings the controls, the spinner and the message in line with the
    /// page's state.
    fn refresh(&self) {
        let ready = self.is_ready();
        let has_day = self.day.get().is_some();
        let has_files = self.has_files();
        self.prev_day.set_sensitive(ready && has_day);
        self.next_day.set_sensitive(ready && has_day);
        self.date.set_sensitive(ready);
        self.timeline.widget().set_sensitive(ready && has_day);
        self.whole_day.set_sensitive(has_day);
        self.play.set_sensitive(ready && has_files);
        self.speed.set_sensitive(ready);
        self.stream.set_sensitive(ready);
        self.filter.set_sensitive(ready);
        let can_jump = ready && has_files && self.has_matching_events();
        self.prev_event.set_sensitive(can_jump);
        self.next_event.set_sensitive(can_jump);
        let (icon, tip) = if self.playing.get() {
            ("media-playback-pause-symbolic", "Pause")
        } else {
            ("media-playback-start-symbolic", "Play")
        };
        if self.play.icon_name().as_deref() != Some(icon) {
            self.play.set_icon_name(icon);
            self.play.set_tooltip_text(Some(tip));
        }
        self.refresh_download_row(ready && has_files);
        let state = self.state.borrow().clone();
        // Not once the user has paused: a state from before the pause may
        // still say buffering.
        let buffering = ready && self.playing.get() && state.as_ref().is_some_and(|s| s.buffering);
        if self.spinner.is_visible() != buffering {
            self.spinner.set_visible(buffering);
            self.spinner.set_spinning(buffering);
        }
        let files = self.files.borrow();
        let day_note = self.day_note.borrow();
        let lost = self.lost.borrow();
        let last_error = self.last_error.borrow();
        let phase = state.as_ref().map(|s| s.phase);
        let situation = Situation {
            device: self.device.borrow().is_some(),
            lost: lost.as_deref(),
            readiness: self.readiness.get(),
            choosing: self.choosing.get().is_some(),
            day_note: day_note.as_deref(),
            day: has_day,
            files: files.as_ref().map(|f| f.as_ref().map(Vec::len).map_err(String::as_str)),
            phase,
            picture: self.video.current_sink().is_some(),
            played_out: phase == Some(PlayerPhase::Ended) && self.played_out(),
            error: last_error.as_deref(),
        };
        match message_of(&situation) {
            Some(text) => {
                if self.message.text() != text {
                    self.message.set_text(&text);
                }
                self.message.set_visible(true);
            }
            None => self.message.set_visible(false),
        }
    }

    // ---- downloads ------------------------------------------------------------

    fn download_running(&self) -> bool {
        self.download.borrow().as_ref().is_some_and(Download::running)
    }

    /// The Download button: the dialog, offered the position on screen
    /// (else the day's first recording), the current stream and the
    /// settings' folder.
    fn open_download_dialog(self: &Rc<Self>) {
        let Some(day) = self.day.get() else { return };
        if !self.is_ready() || !self.has_files() || self.download_running() || self.dialog_open.get() {
            return;
        }
        let Some((key, channel)) = self.device.borrow().clone() else { return };
        let Some(dir) = (self.handlers.download_dir)() else {
            self.notify("Could not create the download folder");
            return;
        };
        let (device, camera) = (self.handlers.names)(&key, channel);
        let listed = match &*self.files.borrow() {
            Some(Ok(files)) => Some((self.files_stream.get(), files.clone())),
            _ => None,
        };
        let first = listed.as_ref().and_then(|(_, files)| files.first()).map(|f| f.start);
        // A time of the day: a position in a file from the evening before
        // (or running into the next day) is brought back into it.
        let start = self.screen_position(day).map(|(_, at)| at).or(first).unwrap_or(day);
        let start = start.clamp(day, day.add_seconds(86_399));
        let setup = Setup { day, start, stream: self.want_stream.get(), listed, dir, device, camera };
        let parent = self.root.root().and_then(|root| root.downcast::<gtk4::Window>().ok());
        self.dialog_open.set(true);
        let (w1, w2, w3, w4) = (Rc::downgrade(self), Rc::downgrade(self), Rc::downgrade(self), Rc::downgrade(self));
        download_dialog::open(
            parent.as_ref(),
            setup,
            move |range| {
                if let Some(p) = w1.upgrade() {
                    p.show_selection(range);
                }
            },
            move |dir| {
                if let Some(p) = w2.upgrade() {
                    (p.handlers.set_download_dir)(dir);
                }
            },
            move |choice| {
                if let Some(p) = w3.upgrade() {
                    p.start_download(choice);
                }
            },
            move || {
                if let Some(p) = w4.upgrade() {
                    p.dialog_open.set(false);
                    p.refresh_selection();
                }
            },
        );
    }

    /// The dialog's choice: the playback is paused (a download needs the
    /// device's connection to itself; Play carries on afterwards) and the
    /// device's thread asked.
    fn start_download(&self, choice: Choice) {
        let Some(day) = self.day.get() else { return };
        let Some((key, channel)) = self.device.borrow().clone() else { return };
        if !self.is_ready() || self.download_running() {
            return;
        }
        if self.playing.get() {
            self.video.freeze();
            self.playing.set(false);
            self.send_player(DeviceLink::playback_pause);
            self.notify("Playback paused for the download");
        }
        let (device, camera) = (self.handlers.names)(&key, channel);
        let request = DownloadRequest {
            session: self.session.get(),
            channel,
            day,
            start: choice.start,
            duration: choice.duration,
            stream: choice.stream,
            dir: choice.dir,
            device,
            camera,
        };
        *self.download.borrow_mut() = Some(Download::new());
        self.send(|link| link.download(request.clone()));
        self.refresh();
    }

    /// What the device's thread says about a download: dropped unless it
    /// is the page's device's and of the current session (a day or
    /// device left meanwhile keeps its download to itself).
    pub fn on_download(&self, key: &str, event: DownloadEvent) {
        if !self.shown.get() || self.device_key().as_deref() != Some(key) || event.session() != self.session.get() {
            return;
        }
        match event {
            DownloadEvent::Started { name, estimate, start, end, .. } => {
                if let Some(d) = self.download.borrow_mut().as_mut() {
                    d.name = name;
                    d.estimate = estimate;
                    d.range = Some((start, end));
                    if d.phase == DownloadPhase::Starting {
                        d.phase = DownloadPhase::Running;
                    }
                }
            }
            DownloadEvent::Progress { bytes, estimate, .. } => {
                if let Some(d) = self.download.borrow_mut().as_mut() {
                    d.bytes = bytes;
                    d.estimate = estimate;
                }
            }
            DownloadEvent::Finished { outcome, .. } => match outcome {
                DownloadOutcome::Saved { name, path, bytes } => {
                    {
                        let mut download = self.download.borrow_mut();
                        let d = download.get_or_insert_with(Download::new);
                        d.name = name.clone();
                        d.path = Some(path);
                        d.bytes = bytes;
                        d.estimate = Some(bytes);
                        d.phase = DownloadPhase::Saved;
                    }
                    self.notify(&format!("Saved {name}"));
                }
                DownloadOutcome::Cancelled => {
                    *self.download.borrow_mut() = None;
                    self.notify("Download cancelled");
                }
                DownloadOutcome::Failed(message) => {
                    *self.download.borrow_mut() = None;
                    self.notify(&message);
                }
            },
        }
        self.refresh_selection();
        self.refresh();
    }

    /// The Cancel button: the device is asked to stop; the partial file
    /// goes.
    fn cancel_download(&self) {
        {
            let mut download = self.download.borrow_mut();
            match download.as_mut() {
                Some(d) if matches!(d.phase, DownloadPhase::Starting | DownloadPhase::Running) => {
                    d.phase = DownloadPhase::Cancelling;
                }
                _ => return,
            }
        }
        self.send(|link| link.download_cancel());
        self.refresh();
    }

    /// The saved file's folder, in the file manager.
    fn open_download_folder(&self) {
        let dir = self
            .download
            .borrow()
            .as_ref()
            .and_then(|d| d.path.as_ref())
            .and_then(|path| path.parent().map(Path::to_path_buf));
        if let Some(uri) = dir.and_then(|dir| glib::filename_to_uri(&dir, None).ok()) {
            crate::ui::dialogs::open_uri(&uri);
        }
    }

    /// The timeline marks `range` (device-local) as the download's.
    fn show_selection(&self, range: Option<(CivilTime, CivilTime)>) {
        let Some(day) = self.day.get() else { return };
        self.timeline.set_selection(
            range.map(|(start, end)| (seconds_of_day(timeline_of(start), day), seconds_of_day(timeline_of(end), day))),
        );
    }

    /// The running download's range, or none (while the dialog is open,
    /// its own range shows instead).
    fn refresh_selection(&self) {
        if self.dialog_open.get() {
            return;
        }
        let range = self.download.borrow().as_ref().filter(|d| d.running()).and_then(|d| d.range);
        self.show_selection(range);
    }

    /// The row under the controls, and the Download button (`usable`: the
    /// day's recordings are listed on a ready device).
    fn refresh_download_row(&self, usable: bool) {
        let download = self.download.borrow();
        let running = download.as_ref().is_some_and(Download::running);
        self.download_button.set_sensitive(usable && !running);
        let Some(d) = download.as_ref() else {
            self.download_row.set_visible(false);
            return;
        };
        let text = d.text();
        if self.download_label.text() != text {
            self.download_label.set_text(&text);
        }
        if running {
            let (bar_text, fraction) = progress_of(d.bytes, d.estimate);
            if self.download_bar.text().as_deref() != Some(bar_text.as_str()) {
                self.download_bar.set_text(Some(&bar_text));
            }
            if let Some(fraction) = fraction {
                self.download_bar.set_fraction(fraction);
            }
        }
        self.download_bar.set_visible(running);
        self.download_cancel.set_visible(running);
        self.download_cancel.set_sensitive(matches!(d.phase, DownloadPhase::Starting | DownloadPhase::Running));
        self.download_open.set_visible(d.phase == DownloadPhase::Saved);
        self.download_dismiss.set_visible(!running);
        self.download_row.set_visible(true);
    }

    /// Without an estimate the bar shows activity instead of a fraction.
    fn pulse_download(&self) {
        let indeterminate =
            self.download.borrow().as_ref().is_some_and(|d| d.running() && d.estimate.filter(|&e| e > 0).is_none());
        if indeterminate {
            self.download_bar.pulse();
        }
    }

    /// A short message over the picture, for a few seconds (a newer one
    /// takes its place).
    fn notify(&self, text: &str) {
        self.notice.set_text(text);
        self.notice.set_visible(true);
        let generation = self.notice_generation.get() + 1;
        self.notice_generation.set(generation);
        let (notice, current) = (self.notice.downgrade(), Rc::clone(&self.notice_generation));
        glib::timeout_add_local_once(NOTICE_FOR, move || {
            if let Some(notice) = notice.upgrade() {
                if current.get() == generation {
                    notice.set_visible(false);
                }
            }
        });
    }
}

/// Plays or is about to: what the play button shows as Pause.
fn plays(phase: PlayerPhase) -> bool {
    matches!(phase, PlayerPhase::Opening | PlayerPhase::Playing)
}

/// What the page knows, for the message over the picture.
struct Situation<'a> {
    device: bool,
    lost: Option<&'a str>,
    readiness: Readiness,
    /// Looking for the latest recorded day.
    choosing: bool,
    day_note: Option<&'a str>,
    day: bool,
    /// The day's recordings: `None` while listed, else how many or why not.
    files: Option<Result<usize, &'a str>>,
    phase: Option<PlayerPhase>,
    /// Footage has reached the picture in this session.
    picture: bool,
    /// After the day's last recording, the picture has played out.
    played_out: bool,
    error: Option<&'a str>,
}

/// The message over the picture, or none when the picture speaks for
/// itself.
fn message_of(s: &Situation) -> Option<String> {
    if !s.device {
        return Some("Select a device".to_string());
    }
    if let Some(reason) = s.lost {
        return Some(format!("Disconnected: {reason}"));
    }
    match s.readiness {
        Readiness::Connecting => return Some("Connecting…".to_string()),
        Readiness::Channels => return Some("Waiting for the channel list…".to_string()),
        Readiness::Ready => {}
    }
    if !s.day {
        return Some(match (s.choosing, s.day_note) {
            (false, Some(note)) => note.to_string(),
            (false, None) => "Choose a day".to_string(),
            (true, _) => "Searching…".to_string(),
        });
    }
    match s.files {
        None => return Some("Searching…".to_string()),
        Some(Err(e)) => return Some(format!("Could not list the recordings: {e}")),
        Some(Ok(0)) => return Some("No recordings on this day".to_string()),
        Some(Ok(_)) => {}
    }
    match s.phase {
        None | Some(PlayerPhase::Idle) => Some("Click the timeline or press Play".to_string()),
        Some(PlayerPhase::Paused) if !s.picture => Some("Click the timeline or press Play".to_string()),
        Some(PlayerPhase::Ended) if s.played_out => Some("End of the day's recordings".to_string()),
        Some(PlayerPhase::Failed) => Some(s.error.unwrap_or("The recording broke off").to_string()),
        _ => None,
    }
}

/// The day `year`-`month`-`day` (as GLib numbers them), if there is one.
fn day_of(year: i32, month: i32, day: i32) -> Option<CivilTime> {
    let day = CivilTime::start_of_day(u16::try_from(year).ok()?, u8::try_from(month).ok()?, u8::try_from(day).ok()?);
    day.is_valid().then_some(day)
}

/// The latest of a month's recorded days.
fn latest_day(year: u16, month: u8, days: &[u8]) -> Option<CivilTime> {
    days.iter()
        .copied()
        .filter_map(|day| day_of(i32::from(year), i32::from(month), i32::from(day)))
        .max()
}

/// Midnight of the day `delta` days after `day`'s.
fn day_after(day: CivilTime, delta: i64) -> CivilTime {
    CivilTime::start_of_day(day.year, day.month, day.day).add_seconds(delta * 86_400)
}

fn previous_month(year: u16, month: u8) -> (u16, u8) {
    if month <= 1 {
        (year.saturating_sub(1), 12)
    } else {
        (year, month - 1)
    }
}

/// Whether a month's days fetched at `fetched` can still be used at `now`.
fn is_fresh(fetched: Instant, now: Instant) -> bool {
    now.saturating_duration_since(fetched) <= MONTH_FRESH_FOR
}

/// The recordings as seconds of `day` (a file running over midnight lies
/// partly below 0 or above a day).
fn spans_of(files: &[FileSpan], day: CivilTime) -> Vec<(f64, f64)> {
    files
        .iter()
        .map(|f| (seconds_of_day(timeline_of(f.start), day), seconds_of_day(timeline_of(f.end), day)))
        .collect()
}

/// The events the filter admits, as the timeline marks them: seconds of
/// `day`, named by their kinds.
fn timeline_events(events: &[RecordedEvent], filter: EventFilter, day: CivilTime) -> Vec<TimelineEvent> {
    events
        .iter()
        .filter(|e| filter.admits(e))
        .map(|e| TimelineEvent {
            start: seconds_of_day(timeline_of(e.start), day),
            end: seconds_of_day(timeline_of(e.end), day),
            label: event_label(&e.kinds),
        })
        .collect()
}

/// "Motion", "Animal, Person": an event's kinds as the user reads them,
/// each once.
fn event_label(kinds: &[String]) -> String {
    let mut labels: Vec<&str> = Vec::new();
    for kind in kinds {
        let label = kind_label(kind);
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    labels.join(", ")
}

/// The starts of the events the filter admits, as seconds of `day`,
/// sorted, each once.
fn event_starts(events: &[RecordedEvent], filter: EventFilter, day: CivilTime) -> Vec<f64> {
    let mut starts: Vec<f64> =
        events.iter().filter(|e| filter.admits(e)).map(|e| seconds_of_day(timeline_of(e.start), day)).collect();
    starts.sort_by(f64::total_cmp);
    starts.dedup();
    starts
}

/// The first of `starts` (sorted) more than `EVENT_JUMP_SLACK` after
/// `position`; with no position, the first of all.
fn next_event_start(starts: &[f64], position: Option<f64>) -> Option<f64> {
    let after = position.map_or(f64::NEG_INFINITY, |p| p + EVENT_JUMP_SLACK);
    starts.iter().copied().find(|&start| start > after)
}

/// The last of `starts` (sorted) more than `EVENT_JUMP_SLACK` before
/// `position`; with no position, the last of all.
fn previous_event_start(starts: &[f64], position: Option<f64>) -> Option<f64> {
    let before = position.map_or(f64::INFINITY, |p| p - EVENT_JUMP_SLACK);
    starts.iter().copied().rev().find(|&start| start < before)
}

/// `YYYY-MM-DD`.
fn date_text(day: CivilTime) -> String {
    format!("{:04}-{:02}-{:02}", day.year, day.month, day.day)
}

/// `YYYY-MM-DD hh:mm:ss`, device-local as given.
fn time_text(t: CivilTime) -> String {
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second)
}

/// "0.25×", "1×", "16×".
fn speed_label(speed: ReplaySpeed) -> String {
    format!("{}×", speed.factor())
}

fn speed_index(speed: ReplaySpeed) -> u32 {
    ReplaySpeed::ALL.iter().position(|s| *s == speed).unwrap_or(0) as u32
}

pub(crate) fn stream_index(stream: RecordStream) -> u32 {
    STREAMS.iter().position(|s| *s == stream).unwrap_or(0) as u32
}

/// "Main stream", "Sub stream".
pub(crate) fn stream_label(stream: RecordStream) -> &'static str {
    STREAM_LABELS[stream_index(stream) as usize]
}

fn filter_index(filter: EventFilter) -> u32 {
    EventFilter::ALL.iter().position(|f| *f == filter).unwrap_or(0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(day: u8, h: u8, m: u8, s: u8) -> CivilTime {
        CivilTime::new(2026, 10, day, h, m, s)
    }

    fn span(start: CivilTime, end: CivilTime) -> FileSpan {
        FileSpan { start, end, encrypted: false }
    }

    /// A ready device with a day of recordings, playing.
    fn playing<'a>() -> Situation<'a> {
        Situation {
            device: true,
            lost: None,
            readiness: Readiness::Ready,
            choosing: false,
            day_note: None,
            day: true,
            files: Some(Ok(3)),
            phase: Some(PlayerPhase::Playing),
            picture: true,
            played_out: false,
            error: None,
        }
    }

    #[test]
    fn the_message_follows_the_device_then_the_day_then_the_player() {
        let message = |s: &Situation| message_of(s);
        assert_eq!(message(&playing()), None);
        assert_eq!(message(&Situation { device: false, ..playing() }).as_deref(), Some("Select a device"));
        assert_eq!(
            message(&Situation { lost: Some("timed out"), readiness: Readiness::Connecting, ..playing() }).as_deref(),
            Some("Disconnected: timed out")
        );
        assert_eq!(message(&Situation { readiness: Readiness::Connecting, ..playing() }).as_deref(), Some("Connecting…"));
        assert_eq!(
            message(&Situation { readiness: Readiness::Channels, ..playing() }).as_deref(),
            Some("Waiting for the channel list…")
        );
        let no_day = Situation { day: false, files: None, phase: None, ..playing() };
        assert_eq!(message(&Situation { choosing: true, ..no_day }).as_deref(), Some("Searching…"));
        assert_eq!(
            message(&Situation { day_note: Some("No recordings in the last two months"), ..no_day }).as_deref(),
            Some("No recordings in the last two months")
        );
        assert_eq!(message(&no_day).as_deref(), Some("Choose a day"));
        assert_eq!(message(&Situation { files: None, phase: None, ..playing() }).as_deref(), Some("Searching…"));
        assert_eq!(
            message(&Situation { files: Some(Err("busy")), phase: None, ..playing() }).as_deref(),
            Some("Could not list the recordings: busy")
        );
        assert_eq!(
            message(&Situation { files: Some(Ok(0)), phase: None, ..playing() }).as_deref(),
            Some("No recordings on this day")
        );
    }

    #[test]
    fn the_message_over_the_picture_for_each_phase() {
        let message = |phase, picture, played_out| {
            message_of(&Situation { phase, picture, played_out, ..playing() })
        };
        let hint = Some("Click the timeline or press Play".to_string());
        assert_eq!(message(None, false, false), hint);
        assert_eq!(message(Some(PlayerPhase::Idle), false, false), hint);
        assert_eq!(message(Some(PlayerPhase::Opening), false, false), None);
        assert_eq!(message(Some(PlayerPhase::Playing), true, false), None);
        // Paused before anything was shown (a refused open, back after a
        // lost connection), or over the picture.
        assert_eq!(message(Some(PlayerPhase::Paused), false, false), hint);
        assert_eq!(message(Some(PlayerPhase::Paused), true, false), None);
        // The day's end shows once the picture has played out.
        assert_eq!(message(Some(PlayerPhase::Ended), true, false), None);
        assert_eq!(message(Some(PlayerPhase::Ended), true, true).as_deref(), Some("End of the day's recordings"));
        assert_eq!(message(Some(PlayerPhase::Failed), true, false).as_deref(), Some("The recording broke off"));
        let failed = Situation { phase: Some(PlayerPhase::Failed), error: Some("The recording broke off: reset"), ..playing() };
        assert_eq!(message_of(&failed).as_deref(), Some("The recording broke off: reset"));
    }

    #[test]
    fn the_play_button_shows_pause_while_opening_or_playing() {
        assert!(plays(PlayerPhase::Opening));
        assert!(plays(PlayerPhase::Playing));
        for phase in [PlayerPhase::Idle, PlayerPhase::Paused, PlayerPhase::Ended, PlayerPhase::Failed] {
            assert!(!plays(phase), "{phase:?}");
        }
    }

    #[test]
    fn days_step_across_month_and_year_ends() {
        assert_eq!(day_after(CivilTime::new(2026, 10, 31, 14, 3, 27), 1), CivilTime::start_of_day(2026, 11, 1));
        assert_eq!(day_after(CivilTime::start_of_day(2026, 11, 1), -1), CivilTime::start_of_day(2026, 10, 31));
        assert_eq!(day_after(CivilTime::start_of_day(2026, 12, 31), 1), CivilTime::start_of_day(2027, 1, 1));
        assert_eq!(day_after(CivilTime::start_of_day(2027, 1, 1), -1), CivilTime::start_of_day(2026, 12, 31));
        assert_eq!(day_after(CivilTime::start_of_day(2028, 2, 28), 1), CivilTime::start_of_day(2028, 2, 29));
        assert_eq!(day_after(CivilTime::start_of_day(2026, 3, 1), -1), CivilTime::start_of_day(2026, 2, 28));
    }

    #[test]
    fn the_month_before_january_is_last_december() {
        assert_eq!(previous_month(2026, 1), (2025, 12));
        assert_eq!(previous_month(2026, 10), (2026, 9));
        assert_eq!(previous_month(2026, 12), (2026, 11));
    }

    #[test]
    fn the_default_day_is_the_latest_recorded_one() {
        assert_eq!(latest_day(2026, 10, &[3, 19, 7]), Some(CivilTime::start_of_day(2026, 10, 19)));
        assert_eq!(latest_day(2026, 10, &[]), None);
        // A day the month does not have is no day.
        assert_eq!(latest_day(2026, 2, &[30, 12]), Some(CivilTime::start_of_day(2026, 2, 12)));
        assert_eq!(latest_day(2026, 2, &[0, 31]), None);
    }

    #[test]
    fn calendar_dates_become_days_only_when_they_exist() {
        assert_eq!(day_of(2026, 10, 19), Some(CivilTime::start_of_day(2026, 10, 19)));
        assert_eq!(day_of(2026, 2, 29), None);
        assert_eq!(day_of(2026, 13, 1), None);
        assert_eq!(day_of(-1, 1, 1), None);
        assert_eq!(day_of(2026, 0, 1), None);
    }

    #[test]
    fn a_month_is_asked_for_again_after_a_minute() {
        let fetched = Instant::now();
        assert!(is_fresh(fetched, fetched));
        assert!(is_fresh(fetched, fetched + Duration::from_secs(60)));
        assert!(!is_fresh(fetched, fetched + Duration::from_secs(61)));
        // A clock that seems to go back counts as fresh.
        assert!(is_fresh(fetched + Duration::from_secs(5), fetched));
    }

    #[test]
    fn recordings_become_seconds_of_the_day_across_midnight() {
        let day = CivilTime::start_of_day(2026, 10, 19);
        let files = [
            span(t(18, 23, 50, 0), t(19, 0, 10, 0)),
            span(t(19, 14, 3, 27), t(19, 14, 10, 0)),
            span(t(19, 23, 55, 0), t(20, 0, 5, 30)),
        ];
        assert_eq!(
            spans_of(&files, day),
            vec![(-600.0, 600.0), (50_607.0, 51_000.0), (86_100.0, 86_730.0)]
        );
        // Any time of the day stands for that day.
        assert_eq!(spans_of(&files, t(19, 14, 0, 0)), spans_of(&files, day));
        assert!(spans_of(&[], day).is_empty());
    }

    #[test]
    fn times_are_shown_as_the_device_gives_them() {
        assert_eq!(date_text(t(9, 0, 0, 0)), "2026-10-09");
        assert_eq!(time_text(t(19, 14, 3, 27)), "2026-10-19 14:03:27");
        assert_eq!(time_text(CivilTime::new(2026, 1, 2, 3, 4, 5)), "2026-01-02 03:04:05");
        // What the cursor timer shows of a sink position with a fraction.
        let position = timeline_of(t(19, 14, 3, 27)) + Duration::from_millis(848);
        assert_eq!(time_text(civil_of(position)), "2026-10-19 14:03:27");
        assert!((seconds_of_day(position, t(19, 0, 0, 0)) - 50_607.848).abs() < 1e-6);
    }

    fn event(start: CivilTime, seconds: i64, kinds: &[&str]) -> RecordedEvent {
        RecordedEvent { start, end: start.add_seconds(seconds), kinds: kinds.iter().map(|k| k.to_string()).collect() }
    }

    /// A Home Hub day: two kinds at one moment, motion, an animal later.
    fn events() -> Vec<RecordedEvent> {
        vec![
            event(t(19, 2, 11, 21), 34, &["dog_cat", "people"]),
            event(t(19, 2, 11, 21), 28, &["md"]),
            event(t(19, 3, 20, 0), 30, &["md"]),
            event(t(19, 11, 7, 27), 30, &["dog_cat"]),
        ]
    }

    #[test]
    fn the_filter_admits_the_kinds_it_names_and_nothing_else() {
        use reoling::protocol::bc::xml::{EVENT_SEARCH_ALARM_TYPES, EVENT_SEARCH_ALARM_TYPE_ITEMS};
        let motion = event(t(19, 1, 0, 0), 10, &["md"]);
        let both = event(t(19, 1, 0, 0), 10, &["dog_cat", "people"]);
        let car = event(t(19, 1, 0, 0), 10, &["vehicle"]);
        let alarm = event(t(19, 1, 0, 0), 10, &["io"]);
        assert!(EventFilter::All.admits(&motion) && EventFilter::All.admits(&both) && EventFilter::All.admits(&alarm));
        assert!(EventFilter::Motion.admits(&motion) && !EventFilter::Motion.admits(&both));
        // An alarm input, a legacy alarm or a video loss is no motion: under
        // "All events" only.
        assert!(EventFilter::ALL.iter().all(|f| f.admits(&alarm) == (*f == EventFilter::All)));
        assert!(EventFilter::Person.admits(&both) && EventFilter::Animal.admits(&both));
        assert!(!EventFilter::Vehicle.admits(&both) && EventFilter::Vehicle.admits(&car));
        for filter in [EventFilter::Package, EventFilter::Zone, EventFilter::Person, EventFilter::Animal] {
            assert!(!filter.admits(&motion), "{filter:?}");
        }
        // Every kind a filter names is one the event search asks the device
        // for (in its kind list or its item list; `package` is only in the
        // former), and no kind is under two filters.
        let asked: HashSet<&str> =
            EVENT_SEARCH_ALARM_TYPES.split(',').map(str::trim).chain(EVENT_SEARCH_ALARM_TYPE_ITEMS).collect();
        let mut seen = HashSet::new();
        for filter in EventFilter::ALL {
            for kind in filter.kinds().unwrap_or(&[]) {
                assert!(asked.contains(kind), "{filter:?} names {kind}, which is not asked for");
                assert!(seen.insert(*kind), "{kind} is under two filters");
            }
        }
        // The dropdown lists them as named, "All events" first.
        let labels: Vec<&str> = EventFilter::ALL.iter().map(|f| f.label()).collect();
        assert_eq!(labels, ["All events", "Any motion", "Person", "Vehicle", "Animal", "Package", "Line crossing / zone"]);
        for (index, filter) in EventFilter::ALL.iter().enumerate() {
            assert_eq!(filter_index(*filter) as usize, index);
        }
    }

    #[test]
    fn events_become_bars_of_the_day_named_by_their_kinds() {
        let day = CivilTime::start_of_day(2026, 10, 19);
        let all = timeline_events(&events(), EventFilter::All, day);
        assert_eq!(all.len(), 4);
        assert_eq!(all[0], TimelineEvent { start: 7_881.0, end: 7_915.0, label: "Animal, Person".to_string() });
        assert_eq!((all[1].start, all[1].end, all[1].label.as_str()), (7_881.0, 7_909.0, "Motion"));
        let animals = timeline_events(&events(), EventFilter::Animal, day);
        assert_eq!(animals.iter().map(|e| e.start).collect::<Vec<_>>(), vec![7_881.0, 40_047.0]);
        assert!(timeline_events(&events(), EventFilter::Vehicle, day).is_empty());
        assert!(timeline_events(&[], EventFilter::All, day).is_empty());
        // A repeated name is given once; a kind the page does not know is
        // named as the device names it.
        let kinds = |kinds: &[&str]| kinds.iter().map(|k| k.to_string()).collect::<Vec<_>>();
        assert_eq!(event_label(&kinds(&["tamper", "md", "pir"])), "Tampering, Motion, PIR motion");
        assert_eq!(event_label(&kinds(&["md", "md"])), "Motion");
        assert_eq!(event_label(&kinds(&["newkind", "loss"])), "newkind, Video loss");
        assert_eq!(kind_label("crossline"), "Line crossing");
        assert_eq!(kind_label("nonmotorveh"), "Non-motor vehicle");
        // Every kind the event search asks for has a name of its own.
        use reoling::protocol::bc::xml::{EVENT_SEARCH_ALARM_TYPES, EVENT_SEARCH_ALARM_TYPE_ITEMS};
        for kind in EVENT_SEARCH_ALARM_TYPES.split(',').map(str::trim).chain(EVENT_SEARCH_ALARM_TYPE_ITEMS) {
            assert_ne!(kind_label(kind), kind, "{kind} would be shown as the device names it");
        }
    }

    #[test]
    fn event_jumps_go_to_the_nearest_start_either_way() {
        let day = CivilTime::start_of_day(2026, 10, 19);
        let starts = event_starts(&events(), EventFilter::All, day);
        assert_eq!(starts, vec![7_881.0, 12_000.0, 40_047.0], "two windows at one moment are one start");
        assert_eq!(event_starts(&events(), EventFilter::Motion, day), vec![7_881.0, 12_000.0]);
        assert_eq!(event_starts(&events(), EventFilter::Animal, day), vec![7_881.0, 40_047.0]);
        assert!(event_starts(&events(), EventFilter::Vehicle, day).is_empty());
        // Nothing on screen: the first, or the last.
        assert_eq!(next_event_start(&starts, None), Some(7_881.0));
        assert_eq!(previous_event_start(&starts, None), Some(40_047.0));
        // Forward: the one just played from (the device starts a little
        // before the seek time) is passed over.
        assert_eq!(next_event_start(&starts, Some(0.0)), Some(7_881.0));
        assert_eq!(next_event_start(&starts, Some(7_878.0)), Some(7_881.0));
        assert_eq!(next_event_start(&starts, Some(7_880.0)), Some(12_000.0));
        assert_eq!(next_event_start(&starts, Some(7_881.5)), Some(12_000.0));
        assert_eq!(next_event_start(&starts, Some(40_047.0)), None);
        // Back: to the start of the one playing, or to the one before.
        assert_eq!(previous_event_start(&starts, Some(40_047.0)), Some(12_000.0));
        assert_eq!(previous_event_start(&starts, Some(12_050.0)), Some(12_000.0));
        assert_eq!(previous_event_start(&starts, Some(12_001.0)), Some(7_881.0));
        assert_eq!(previous_event_start(&starts, Some(7_881.0)), None);
        assert_eq!(next_event_start(&[], Some(0.0)), None);
        assert_eq!(previous_event_start(&[], None), None);
    }

    #[test]
    fn the_download_row_reads_bytes_against_the_estimate() {
        assert_eq!(progress_of(0, None), ("0 B".to_string(), None));
        assert_eq!(progress_of(1_200_000, Some(0)), ("1.2 MB".to_string(), None), "an estimate of 0 is none");
        let (text, fraction) = progress_of(1_200_000, Some(3_400_000));
        assert_eq!(text, "1.2 MB of 3.4 MB · 35 %");
        assert!((fraction.unwrap() - 1_200_000.0 / 3_400_000.0).abs() < 1e-9);
        // Delivered beyond the estimate (it is only that): not full until
        // the file is saved.
        let (text, fraction) = progress_of(935_344, Some(918_014));
        assert_eq!(text, "935.3 KB of 918.0 KB · 99 %");
        assert_eq!(fraction, Some(0.99));
        let (text, fraction) = progress_of(900_000, Some(918_014));
        assert_eq!(text, "900.0 KB of 918.0 KB · 98 %");
        assert!((fraction.unwrap() - 900_000.0 / 918_014.0).abs() < 1e-9, "below the cap, as it is");
        let mut d = Download::new();
        assert_eq!(d.text(), "Starting the download…");
        assert!(d.running());
        d.name = "clip.mp4".to_string();
        d.phase = DownloadPhase::Running;
        assert_eq!(d.text(), "Downloading clip.mp4");
        d.phase = DownloadPhase::Cancelling;
        assert_eq!(d.text(), "Cancelling the download…");
        assert!(d.running());
        d.phase = DownloadPhase::Saved;
        assert_eq!(d.text(), "Saved clip.mp4");
        assert!(!d.running());
    }

    #[test]
    fn the_dropdowns_list_every_speed_and_both_streams() {
        let labels: Vec<String> = ReplaySpeed::ALL.iter().map(|s| speed_label(*s)).collect();
        assert_eq!(labels, ["0.25×", "0.5×", "1×", "2×", "4×", "8×", "16×"]);
        for (index, speed) in ReplaySpeed::ALL.iter().enumerate() {
            assert_eq!(speed_index(*speed) as usize, index);
        }
        assert_eq!(STREAMS[stream_index(RecordStream::Main) as usize], RecordStream::Main);
        assert_eq!(STREAMS[stream_index(RecordStream::Sub) as usize], RecordStream::Sub);
        // As on the live page: the main stream first, the sub stream picked.
        assert_eq!(STREAM_LABELS, ["Main stream", "Sub stream"]);
        assert_eq!(stream_index(RecordStream::Sub), 1);
        assert_eq!((stream_label(RecordStream::Main), stream_label(RecordStream::Sub)), ("Main stream", "Sub stream"));
    }
}
