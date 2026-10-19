//! The Playback page: recorded footage of one device channel, a day at a
//! time. The device and channel come from the sidebar; a calendar picks the
//! day (recorded days are marked), the day timeline shows its recordings and
//! seeks, and the controls play, pause and choose the speed and the stream.
//!
//! The page only says what it wants. The device's network thread does the
//! work and reports back as `PlaybackEvent`s, which are dropped here unless
//! they belong to the device on the page and to its current session (every
//! day, device or channel change starts a new one). Every time shown is
//! device-local, exactly as the device reports it: the host's zone is used
//! for nothing but the calendar's first month.

use crate::ui::bridge::{DeviceLink, FileSpan, PlaybackEvent, PlayerPhase, PlayerState};
use crate::ui::playback_driver::{civil_at, civil_of, seconds_of_day, timeline_of};
use crate::ui::playback_timeline::PlaybackTimeline;
use crate::ui::video_view::VideoView;
use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, Calendar, DropDown, Label, MenuButton, Orientation, Overlay, Popover, Spinner,
    ToggleButton,
};
use reoling::{CivilTime, RecordStream, ReplaySpeed};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
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

/// The stream dropdown, in the live page's order and words.
const STREAMS: [RecordStream; 2] = [RecordStream::Main, RecordStream::Sub];
const STREAM_LABELS: [&str; 2] = ["Main stream", "Sub stream"];

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
    /// Kept while the device is away.
    remembered: Cell<Option<Remembered>>,
    /// To be applied once the session's recordings are listed.
    restore: Cell<Option<Remembered>>,
    timer: RefCell<Option<glib::SourceId>>,
    /// Set while code, not the user, changes a control.
    updating: Cell<bool>,
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
        let spacer = GtkBox::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        controls.append(&spacer);
        controls.append(&speed);
        controls.append(&stream);

        let root = GtkBox::new(Orientation::Vertical, 0);
        root.append(&overlay);
        root.append(&timeline_row);
        root.append(&controls);

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
            handlers,
            shown: Cell::new(false),
            device: RefCell::new(None),
            readiness: Cell::new(Readiness::Connecting),
            lost: RefCell::new(None),
            session: Cell::new(0),
            day: Cell::new(None),
            files: RefCell::new(None),
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
            remembered: Cell::new(None),
            restore: Cell::new(None),
            timer: RefCell::new(None),
            updating: Cell::new(false),
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
        *self.state.borrow_mut() = None;
        *self.last_error.borrow_mut() = None;
        *self.day_note.borrow_mut() = None;
        self.choosing.set(None);
        self.restore.set(None);
        self.pending_seek.set(None);
        self.playing.set(false);
        self.command_gen.set(0);
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
        let (icon, tip) = if self.playing.get() {
            ("media-playback-pause-symbolic", "Pause")
        } else {
            ("media-playback-start-symbolic", "Play")
        };
        if self.play.icon_name().as_deref() != Some(icon) {
            self.play.set_icon_name(icon);
            self.play.set_tooltip_text(Some(tip));
        }
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

fn stream_index(stream: RecordStream) -> u32 {
    STREAMS.iter().position(|s| *s == stream).unwrap_or(0) as u32
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
    }
}
