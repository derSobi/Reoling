//! The day timeline of the Playback page: one selected day of recordings
//! drawn as bands on a 24-hour track, its event windows as an accent band
//! over them, with hour ticks, a hover line and the playback cursor. A
//! click asks for a seek; the wheel zooms around the pointer (down to a
//! two-minute window) and a drag, a horizontal or a Shift+wheel scroll
//! pans. The arrow keys seek from the cursor, `+`/`-` zoom around it and
//! Home shows the whole day again.
//!
//! Everything is in seconds of the selected day (device-local): the widget
//! knows no dates or zones, and leaves the meaning of a seek (which file,
//! which second) to its caller. A plain `DrawingArea` with a Rust callback,
//! as the sidebar does; colours come from the theme.

use gtk4::prelude::*;
use gtk4::{cairo, gdk, DrawingArea, EventControllerFocus, EventControllerKey, EventControllerMotion};
use gtk4::{EventControllerScroll, EventControllerScrollFlags, GestureDrag};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Length of a day, in seconds.
pub const DAY_SECONDS: f64 = 86_400.0;
/// The most zoomed-in window: two minutes.
pub const MIN_SPAN: f64 = 120.0;
/// Zoom factor of one wheel notch.
pub const ZOOM_STEP: f64 = 1.5;

/// The latest time a seek can ask for: the day is half-open.
const LAST_INSTANT: f64 = DAY_SECONDS - 1e-3;
/// Tick spacings to choose from, in seconds.
const TICK_STEPS: [f64; 9] = [60.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0, 10800.0, 21600.0];
/// Ticks are never closer than this.
const TICK_MIN_PX: f64 = 64.0;
/// A press that moves less than this before it is let go is a click.
const CLICK_SLOP_PX: f64 = 4.0;
/// After a zoom or a pan the view stays where the user put it this long
/// before it may follow the cursor again.
const FOLLOW_QUIET: Duration = Duration::from_secs(5);
/// Arrow keys seek this far from the cursor (Shift: the long step).
const KEY_SEEK: f64 = 10.0;
const KEY_SEEK_LONG: f64 = 60.0;
/// A pan of one scroll notch, as a share of the visible window.
const PAN_PER_NOTCH: f64 = 0.1;
/// Where a followed cursor lands after paging, as a share of the window.
const FOLLOW_LEAD: f64 = 0.1;
const HEIGHT: i32 = 56;
/// The event band takes this share of the recording band's height, from
/// its top.
const EVENT_BAND_SHARE: f64 = 0.45;
/// Accent when the theme names none (the usual GNOME blue).
const FALLBACK_ACCENT: (f64, f64, f64) = (0x35 as f64 / 255.0, 0x84 as f64 / 255.0, 0xe4 as f64 / 255.0);

/// One event window of the day: half-open `[start, end)` seconds of the
/// day, and what to call it in the tooltip ("Motion", "Person, Animal").
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineEvent {
    pub start: f64,
    pub end: f64,
    pub label: String,
}

/// The visible window, in seconds of the day.
#[derive(Debug, Clone, Copy, PartialEq)]
struct View {
    start: f64,
    span: f64,
}

impl View {
    const WHOLE_DAY: View = View { start: 0.0, span: DAY_SECONDS };

    fn end(&self) -> f64 {
        self.start + self.span
    }

    fn contains(&self, t: f64) -> bool {
        self.start <= t && t < self.end()
    }
}

/// The x (pixels from the left) of time `t` in `view` drawn `width` wide.
/// Times outside the view give x outside `[0, width]`.
fn time_to_x(view: View, width: f64, t: f64) -> f64 {
    if !(width > 0.0 && view.span > 0.0) {
        return 0.0;
    }
    (t - view.start) * width / view.span
}

/// The time at `x` in `view` drawn `width` wide, kept inside the day
/// (`[0, DAY_SECONDS)`).
fn x_to_time(view: View, width: f64, x: f64) -> f64 {
    let t = if width > 0.0 && x.is_finite() { view.start + x * view.span / width } else { view.start };
    clamp_time(t)
}

/// `t` kept inside the day, `[0, DAY_SECONDS)`.
fn clamp_time(t: f64) -> f64 {
    if t.is_nan() {
        0.0
    } else {
        t.clamp(0.0, LAST_INSTANT)
    }
}

/// `view` with its span within `[MIN_SPAN, DAY_SECONDS]` and the window
/// inside the day.
fn clamp_view(view: View) -> View {
    let span = if view.span.is_nan() { DAY_SECONDS } else { view.span.clamp(MIN_SPAN, DAY_SECONDS) };
    let start = if view.start.is_nan() { 0.0 } else { view.start.clamp(0.0, DAY_SECONDS - span) };
    View { start, span }
}

/// Zooms in by `factor` (below 1: out) so that time `anchor` keeps its x,
/// unless the clamp to the day moves the window.
fn zoom_view(view: View, anchor: f64, factor: f64) -> View {
    let view = clamp_view(view);
    if !(factor > 0.0 && factor.is_finite() && anchor.is_finite()) {
        return view;
    }
    let span = (view.span / factor).clamp(MIN_SPAN, DAY_SECONDS);
    let start = anchor - (anchor - view.start) * span / view.span;
    clamp_view(View { start, span })
}

/// Moves the window by `delta` seconds (later when positive), within the day.
fn pan_view(view: View, delta: f64) -> View {
    if !delta.is_finite() {
        return clamp_view(view);
    }
    clamp_view(View { start: view.start + delta, span: view.span })
}

/// The window to show for cursor `position`: unchanged while it is visible,
/// else paged so that it sits a tenth in from the left.
fn follow_view(view: View, position: f64) -> View {
    if !position.is_finite() || view.contains(position) {
        return view;
    }
    clamp_view(View { start: position - FOLLOW_LEAD * view.span, span: view.span })
}

/// Valid spans (`start < end`, finite), sorted by start then end; overlaps
/// stay. This is the list a time is looked up in.
fn sort_spans(spans: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut sorted: Vec<(f64, f64)> =
        spans.iter().copied().filter(|&(start, end)| start.is_finite() && end.is_finite() && start < end).collect();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    sorted
}

/// The spans as drawn: valid ones only, sorted, and merged where they
/// overlap or touch. Nothing is cut to the day; drawing clips.
fn merge_spans(spans: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (start, end) in sort_spans(spans) {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Valid events (`start < end`, finite), sorted by start then end.
fn sort_events(events: &[TimelineEvent]) -> Vec<TimelineEvent> {
    let mut sorted: Vec<TimelineEvent> =
        events.iter().filter(|e| e.start.is_finite() && e.end.is_finite() && e.start < e.end).cloned().collect();
    sorted.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.end.total_cmp(&b.end)));
    sorted
}

/// The labels of the events holding `t`, each once, in the events' order.
/// `sorted` comes from [`sort_events`].
fn labels_at(sorted: &[TimelineEvent], t: f64) -> Vec<&str> {
    let mut labels: Vec<&str> = Vec::new();
    for event in sorted.iter().take_while(|e| e.start <= t).filter(|e| t < e.end) {
        if !labels.contains(&event.label.as_str()) {
            labels.push(&event.label);
        }
    }
    labels
}

/// What a seek to `t` would play, by the same rules as the client's
/// `file_for_time`: the last span holding `t` (the later file of an overlap)
/// from `t`; else the first span after `t` from its start; else nothing
/// (after the last span). `sorted` comes from [`sort_spans`].
fn file_for_time(sorted: &[(f64, f64)], t: f64) -> Option<(usize, f64)> {
    if let Some(index) = sorted.iter().rposition(|&(start, end)| start <= t && t < end) {
        return Some((index, t));
    }
    sorted.iter().position(|&(start, _)| start > t).map(|index| (index, sorted[index].0))
}

/// The tick spacing for `span` seconds drawn `width` wide: the finest step
/// whose ticks are at least `min_px` apart.
fn tick_step(span: f64, width: f64, min_px: f64) -> f64 {
    let coarsest = TICK_STEPS[TICK_STEPS.len() - 1];
    if !(width > 0.0 && span > 0.0) {
        return coarsest;
    }
    TICK_STEPS.iter().copied().find(|&step| step * width / span >= min_px).unwrap_or(coarsest)
}

/// The tick times of `view` drawn `width` wide: every step inside both the
/// window and the day, 24:00 included.
fn ticks(view: View, width: f64) -> Vec<f64> {
    let step = tick_step(view.span, width, TICK_MIN_PX);
    let first = (view.start.max(0.0) / step).ceil() as i64;
    let last = view.end().min(DAY_SECONDS);
    (first..).map(|k| k as f64 * step).take_while(|&t| t <= last).collect()
}

/// A tick label, `HH:MM`, from 00:00 to 24:00.
fn format_tick(t: f64) -> String {
    let seconds = clamp_seconds(t.round());
    format!("{:02}:{:02}", seconds / 3600, seconds % 3600 / 60)
}

/// A time of the day to the second (floor), `HH:MM:SS`.
fn format_clock(t: f64) -> String {
    let seconds = clamp_seconds(t.floor());
    format!("{:02}:{:02}:{:02}", seconds / 3600, seconds % 3600 / 60, seconds % 60)
}

fn clamp_seconds(t: f64) -> i64 {
    if t.is_nan() {
        0
    } else {
        t.clamp(0.0, DAY_SECONDS) as i64
    }
}

/// The zoom factor of a vertical scroll of `dy` notches: up (negative)
/// zooms in. Fractional deltas (touchpads) zoom smoothly.
fn wheel_factor(dy: f64) -> f64 {
    ZOOM_STEP.powf(-dy)
}

/// A button-1 press in progress.
#[derive(Debug, Clone, Copy)]
struct Drag {
    /// Where it started.
    x: f64,
    /// The view when it started: the drag's offsets add up from there.
    view: View,
    /// It moved far enough to be a pan, not a click.
    panning: bool,
}

struct State {
    /// The selected day (year, month, day), if any.
    day: Option<(u16, u8, u8)>,
    /// The day's spans, sorted, overlaps kept (for lookups).
    spans: Vec<(f64, f64)>,
    /// The same, merged (for drawing).
    merged: Vec<(f64, f64)>,
    /// The day's event windows, sorted (for the tooltip).
    events: Vec<TimelineEvent>,
    /// The same, merged (for drawing).
    events_merged: Vec<(f64, f64)>,
    position: Option<f64>,
    view: View,
    /// Page the view along with the cursor.
    follow: bool,
    /// When the user last zoomed or panned.
    moved_at: Option<Instant>,
    /// The pointer's x while it is over the widget.
    hover_x: Option<f64>,
    drag: Option<Drag>,
    /// The range a download asks for (or the one being chosen), seconds
    /// of the day, start before end.
    selection: Option<(f64, f64)>,
}

impl State {
    fn new() -> Self {
        State {
            day: None,
            spans: Vec::new(),
            merged: Vec::new(),
            events: Vec::new(),
            events_merged: Vec::new(),
            position: None,
            view: View::WHOLE_DAY,
            follow: true,
            moved_at: None,
            hover_x: None,
            drag: None,
            selection: None,
        }
    }

    /// A zoom or pan by the user: the view stays where it was put.
    fn user_moved(&mut self, view: View) {
        self.view = view;
        self.follow = false;
        self.moved_at = Some(Instant::now());
    }
}

pub struct PlaybackTimeline {
    area: DrawingArea,
    state: RefCell<State>,
    on_seek: RefCell<Option<Rc<dyn Fn(f64)>>>,
}

impl PlaybackTimeline {
    pub fn new() -> Rc<Self> {
        let area = DrawingArea::new();
        area.set_content_height(HEIGHT);
        area.set_hexpand(true);
        area.set_focusable(true);
        area.set_focus_on_click(true);
        area.set_has_tooltip(true);
        area.update_property(&[gtk4::accessible::Property::Label("Day timeline")]);

        let this = Rc::new(Self { area, state: RefCell::new(State::new()), on_seek: RefCell::new(None) });

        let weak = Rc::downgrade(&this);
        this.area.set_draw_func(move |area, cr, width, height| {
            if let Some(t) = weak.upgrade() {
                // A failed cairo call only loses this frame.
                let _ = t.draw(area, cr, f64::from(width), f64::from(height));
            }
        });

        let drag = GestureDrag::new();
        drag.set_button(1);
        let weak = Rc::downgrade(&this);
        drag.connect_drag_begin(move |_, x, _| {
            if let Some(t) = weak.upgrade() {
                t.area.grab_focus();
                let mut state = t.state.borrow_mut();
                let view = state.view;
                state.drag = Some(Drag { x, view, panning: false });
            }
        });
        let weak = Rc::downgrade(&this);
        drag.connect_drag_update(move |_, dx, _| {
            if let Some(t) = weak.upgrade() {
                t.drag_to(dx);
            }
        });
        let weak = Rc::downgrade(&this);
        drag.connect_drag_end(move |_, dx, _| {
            if let Some(t) = weak.upgrade() {
                t.drag_to(dx);
                let drag = t.state.borrow_mut().drag.take();
                if let Some(drag) = drag.filter(|d| !d.panning) {
                    let at = x_to_time(drag.view, t.width(), drag.x);
                    t.request_seek(at);
                }
            }
        });
        // A press taken away (a grab, the window losing focus) is no click:
        // forget it before its drag-end comes.
        let weak = Rc::downgrade(&this);
        drag.connect_cancel(move |_, _| {
            if let Some(t) = weak.upgrade() {
                t.state.borrow_mut().drag = None;
            }
        });
        this.area.add_controller(drag);

        let motion = EventControllerMotion::new();
        let weak = Rc::downgrade(&this);
        motion.connect_motion(move |_, x, _| {
            if let Some(t) = weak.upgrade() {
                t.set_hover(Some(x));
            }
        });
        let weak = Rc::downgrade(&this);
        motion.connect_leave(move |_| {
            if let Some(t) = weak.upgrade() {
                t.set_hover(None);
            }
        });
        this.area.add_controller(motion);

        let scroll = EventControllerScroll::new(EventControllerScrollFlags::BOTH_AXES);
        let weak = Rc::downgrade(&this);
        scroll.connect_scroll(move |controller, dx, dy| {
            if let Some(t) = weak.upgrade() {
                let shift = controller.current_event_state().contains(gdk::ModifierType::SHIFT_MASK);
                t.scrolled(dx, dy, shift);
            }
            glib::Propagation::Stop
        });
        this.area.add_controller(scroll);

        let keys = EventControllerKey::new();
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, modifiers| match weak.upgrade() {
            Some(t) if t.key_pressed(key, modifiers.contains(gdk::ModifierType::SHIFT_MASK)) => {
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        this.area.add_controller(keys);

        // The focus outline comes and goes with the keyboard focus.
        let focus = EventControllerFocus::new();
        let weak = Rc::downgrade(&this);
        focus.connect_enter(move |_| {
            if let Some(t) = weak.upgrade() {
                t.area.queue_draw();
            }
        });
        let weak = Rc::downgrade(&this);
        focus.connect_leave(move |_| {
            if let Some(t) = weak.upgrade() {
                t.area.queue_draw();
            }
        });
        this.area.add_controller(focus);

        let weak = Rc::downgrade(&this);
        this.area.connect_query_tooltip(move |_, x, _, keyboard, tooltip| {
            let Some(t) = weak.upgrade() else { return false };
            if keyboard {
                tooltip.set_text(Some(&t.keyboard_tooltip_text()));
                return true;
            }
            match t.tooltip_text(f64::from(x)) {
                Some(text) => {
                    tooltip.set_text(Some(&text));
                    true
                }
                None => false,
            }
        });

        this
    }

    pub fn widget(&self) -> &DrawingArea {
        &self.area
    }

    /// A new day (or none): clears files, events and cursor, shows the
    /// whole day. Only whether there is a day matters here; the widget's
    /// unit is seconds of that day.
    pub fn set_day(&self, day: Option<(u16, u8, u8)>) {
        {
            let mut state = self.state.borrow_mut();
            state.day = day;
            state.spans.clear();
            state.merged.clear();
            state.events.clear();
            state.events_merged.clear();
            state.position = None;
            state.view = View::WHOLE_DAY;
            state.follow = true;
            state.moved_at = None;
            state.drag = None;
            state.selection = None;
        }
        self.area.queue_draw();
    }

    /// The day's recordings as half-open [start, end) seconds of the day, any
    /// order, overlaps allowed, values may lie < 0 or > DAY_SECONDS.
    pub fn set_files(&self, files: &[(f64, f64)]) {
        let spans = sort_spans(files);
        {
            let mut state = self.state.borrow_mut();
            if state.spans == spans {
                return;
            }
            state.merged = merge_spans(&spans);
            state.spans = spans;
        }
        self.area.queue_draw();
    }

    /// The day's event windows to mark, any order, overlaps allowed; they
    /// are drawn merged over the recordings, and the tooltip names the
    /// ones under the pointer. The caller filters.
    pub fn set_events(&self, events: &[TimelineEvent]) {
        let events = sort_events(events);
        {
            let mut state = self.state.borrow_mut();
            if state.events == events {
                return;
            }
            let spans: Vec<(f64, f64)> = events.iter().map(|e| (e.start, e.end)).collect();
            state.events_merged = merge_spans(&spans);
            state.events = events;
        }
        self.area.queue_draw();
    }

    /// The cursor (seconds of the day), or none. While following (since the
    /// last click, and not within a few seconds of a zoom or pan) the view
    /// pages along with it.
    pub fn set_position(&self, seconds: Option<f64>) {
        {
            let mut state = self.state.borrow_mut();
            if state.position == seconds {
                return;
            }
            state.position = seconds;
            let quiet = state.moved_at.map_or(true, |at| at.elapsed() >= FOLLOW_QUIET);
            match seconds {
                Some(position) if state.follow && quiet => state.view = follow_view(state.view, position),
                _ => {}
            }
        }
        self.area.queue_draw();
    }

    /// The range a download asks for (or the one being chosen), as
    /// seconds of the day in any order; drawn as a translucent band.
    /// `None` clears it (a new day does too).
    pub fn set_selection(&self, range: Option<(f64, f64)>) {
        let range = range.map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
        {
            let mut state = self.state.borrow_mut();
            if state.selection == range {
                return;
            }
            state.selection = range;
        }
        self.area.queue_draw();
    }

    /// Called with the clicked/keyed time, seconds of the day in [0, DAY_SECONDS).
    /// Raw: the driver decides which file that means.
    pub fn connect_seek_requested(&self, f: impl Fn(f64) + 'static) {
        *self.on_seek.borrow_mut() = Some(Rc::new(f));
    }

    /// Shows the whole day, and follows the cursor again.
    pub fn reset_view(&self) {
        {
            let mut state = self.state.borrow_mut();
            state.view = View::WHOLE_DAY;
            state.follow = true;
            state.moved_at = None;
        }
        self.area.queue_draw();
    }
}

impl PlaybackTimeline {
    fn width(&self) -> f64 {
        f64::from(self.area.width())
    }

    /// Tells the caller, with no borrow held: it will move the cursor.
    fn request_seek(&self, at: f64) {
        {
            let mut state = self.state.borrow_mut();
            if state.day.is_none() {
                return;
            }
            state.follow = true;
        }
        let callback = self.on_seek.borrow().clone();
        if let Some(callback) = callback {
            callback(clamp_time(at));
        }
    }

    /// A drag `dx` pixels from where it started: a pan once it went past the
    /// click slop.
    fn drag_to(&self, dx: f64) {
        let width = self.width();
        {
            let mut state = self.state.borrow_mut();
            let Some(mut drag) = state.drag else { return };
            if !drag.panning && dx.abs() < CLICK_SLOP_PX {
                return;
            }
            drag.panning = true;
            state.drag = Some(drag);
            if width <= 0.0 {
                return;
            }
            let view = pan_view(drag.view, -dx * drag.view.span / width);
            state.user_moved(view);
        }
        self.area.queue_draw();
    }

    fn set_hover(&self, x: Option<f64>) {
        {
            let mut state = self.state.borrow_mut();
            if state.hover_x == x {
                return;
            }
            state.hover_x = x;
        }
        self.area.queue_draw();
    }

    /// Vertical: zoom around the time under the pointer. Horizontal, or
    /// vertical with Shift: pan.
    fn scrolled(&self, dx: f64, dy: f64, shift: bool) {
        let width = self.width();
        {
            let mut state = self.state.borrow_mut();
            let mut view = state.view;
            let (pan, zoom) = if shift { (dx + dy, 0.0) } else { (dx, dy) };
            if zoom != 0.0 {
                let anchor = match state.hover_x {
                    Some(x) if width > 0.0 => view.start + x * view.span / width,
                    _ => view.start + view.span / 2.0,
                };
                view = zoom_view(view, anchor, wheel_factor(zoom));
            }
            if pan != 0.0 {
                view = pan_view(view, pan * PAN_PER_NOTCH * view.span);
            }
            if view == state.view {
                return;
            }
            state.user_moved(view);
        }
        self.area.queue_draw();
    }

    /// Left/Right seek from the cursor, `+`/`-` zoom around it, Home shows
    /// the whole day. Returns whether the key was used.
    fn key_pressed(&self, key: gdk::Key, shift: bool) -> bool {
        use gdk::Key;
        let (has_day, position, view) = {
            let state = self.state.borrow();
            (state.day.is_some(), state.position, state.view)
        };
        if !has_day {
            return false;
        }
        let step = if shift { KEY_SEEK_LONG } else { KEY_SEEK };
        let zoom = |factor: f64| {
            let anchor = position.filter(|&p| view.contains(p)).unwrap_or(view.start + view.span / 2.0);
            let zoomed = zoom_view(view, anchor, factor);
            if zoomed != view {
                self.state.borrow_mut().user_moved(zoomed);
                self.area.queue_draw();
            }
        };
        match key {
            Key::Left | Key::KP_Left | Key::Right | Key::KP_Right => {
                let Some(position) = position else { return false };
                let delta = if matches!(key, Key::Left | Key::KP_Left) { -step } else { step };
                self.request_seek(position + delta);
            }
            Key::plus | Key::equal | Key::KP_Add => zoom(ZOOM_STEP),
            Key::minus | Key::KP_Subtract => zoom(1.0 / ZOOM_STEP),
            Key::Home | Key::KP_Home => self.reset_view(),
            _ => return false,
        }
        true
    }

    /// With the keyboard: the cursor's time, else what the widget is.
    fn keyboard_tooltip_text(&self) -> String {
        match self.state.borrow().position {
            Some(position) => format_clock(position),
            None => "Day timeline".to_string(),
        }
    }

    /// The time under the pointer, the events there, and in a gap where a
    /// click would play from.
    fn tooltip_text(&self, x: f64) -> Option<String> {
        let state = self.state.borrow();
        if state.day.is_none() {
            return None;
        }
        let at = x_to_time(state.view, self.width(), x);
        let mut text = format_clock(at);
        let labels = labels_at(&state.events, at);
        if !labels.is_empty() {
            text = format!("{text}\n{}", labels.join(", "));
        }
        if state.spans.is_empty() {
            return Some(text);
        }
        Some(match file_for_time(&state.spans, at) {
            Some((_, from)) if from == at => text,
            Some((_, from)) => format!("{text}\nNo recording · plays from {}", format_clock(from)),
            None => format!("{text}\nNothing after this point"),
        })
    }

    fn draw(&self, area: &DrawingArea, cr: &cairo::Context, width: f64, height: f64) -> Result<(), cairo::Error> {
        if width <= 0.0 || height <= 0.0 {
            return Ok(());
        }
        let style = area.style_context();
        let fg = style.color();
        let (r, g, b) = (f64::from(fg.red()), f64::from(fg.green()), f64::from(fg.blue()));
        let accent = style
            .lookup_color("accent_bg_color")
            .or_else(|| style.lookup_color("theme_selected_bg_color"))
            .map(|c| (f64::from(c.red()), f64::from(c.green()), f64::from(c.blue())))
            .unwrap_or(FALLBACK_ACCENT);
        let state = self.state.borrow();
        let view = state.view;
        let focused = area.has_visible_focus();

        // Track.
        cr.set_source_rgba(r, g, b, 0.08);
        rounded_rectangle(cr, 0.0, 0.0, width, height, 6.0);
        cr.fill()?;
        if state.day.is_none() {
            return if focused { focus_outline(cr, width, height, accent) } else { Ok(()) };
        }

        // Ticks and their labels.
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
        cr.set_font_size(11.0);
        cr.set_line_width(1.0);
        let mut label_end = f64::NEG_INFINITY;
        for t in ticks(view, width) {
            let x = crisp(time_to_x(view, width, t).clamp(0.0, width - 1.0));
            cr.set_source_rgba(r, g, b, 0.45);
            cr.move_to(x, 0.0);
            cr.line_to(x, 6.0);
            cr.stroke()?;
            let label = format_tick(t);
            let extents = cr.text_extents(&label)?;
            let mut lx = x + 3.0;
            if lx + extents.x_advance() > width {
                lx = x - 3.0 - extents.x_advance();
            }
            if lx < 0.0 || lx < label_end + 4.0 {
                continue;
            }
            cr.set_source_rgba(r, g, b, 0.7);
            cr.move_to(lx, 18.0);
            cr.show_text(&label)?;
            label_end = lx + extents.x_advance();
        }

        // Recordings, at least a pixel wide so a short clip still shows.
        let band_top = (height * 0.45).round();
        let band_height = (height - 4.0 - band_top).max(1.0);
        cr.set_source_rgba(r, g, b, 0.35);
        for &(start, end) in &state.merged {
            if end <= view.start || start >= view.end() {
                continue;
            }
            let x0 = time_to_x(view, width, start.max(view.start));
            let x1 = time_to_x(view, width, end.min(view.end()));
            cr.rectangle(x0, band_top, (x1 - x0).max(1.0), band_height);
        }
        cr.fill()?;

        // Events, as an accent band over the top of the recordings.
        let event_height = (band_height * EVENT_BAND_SHARE).round().max(2.0);
        cr.set_source_rgba(accent.0, accent.1, accent.2, 0.9);
        for &(start, end) in &state.events_merged {
            if end <= view.start || start >= view.end() {
                continue;
            }
            let x0 = time_to_x(view, width, start.max(view.start));
            let x1 = time_to_x(view, width, end.min(view.end()));
            cr.rectangle(x0, band_top, (x1 - x0).max(1.0), event_height);
        }
        cr.fill()?;

        // The download's range (or the one being chosen): a translucent
        // band over the whole height, with its ends marked where they
        // are in view.
        if let Some((start, end)) = state.selection {
            if end > view.start && start < view.end() {
                let x0 = time_to_x(view, width, start.max(view.start));
                let x1 = time_to_x(view, width, end.min(view.end()));
                cr.set_source_rgba(accent.0, accent.1, accent.2, 0.18);
                cr.rectangle(x0, 0.0, (x1 - x0).max(1.0), height);
                cr.fill()?;
                cr.set_source_rgba(accent.0, accent.1, accent.2, 0.6);
                cr.set_line_width(1.0);
                if start >= view.start {
                    cr.move_to(crisp(x0), 0.0);
                    cr.line_to(crisp(x0), height);
                }
                if end <= view.end() {
                    cr.move_to(crisp(x1), 0.0);
                    cr.line_to(crisp(x1), height);
                }
                cr.stroke()?;
            }
        }

        // Hover line.
        if let Some(x) = state.hover_x.filter(|x| (0.0..width).contains(x)) {
            cr.set_source_rgba(r, g, b, 0.5);
            cr.set_line_width(1.0);
            cr.move_to(crisp(x), 0.0);
            cr.line_to(crisp(x), height);
            cr.stroke()?;
        }

        // Cursor, with a small head at the top.
        if let Some(position) = state.position.filter(|&p| view.contains(p)) {
            let x = time_to_x(view, width, position).round();
            cr.set_source_rgb(accent.0, accent.1, accent.2);
            cr.set_line_width(2.0);
            cr.move_to(x, 0.0);
            cr.line_to(x, height);
            cr.stroke()?;
            cr.move_to(x - 5.0, 0.0);
            cr.line_to(x + 5.0, 0.0);
            cr.line_to(x, 6.0);
            cr.close_path();
            cr.fill()?;
        }
        if focused {
            focus_outline(cr, width, height, accent)?;
        }
        Ok(())
    }
}

/// A 2-px accent line just inside the track: the keyboard focus is here.
fn focus_outline(cr: &cairo::Context, width: f64, height: f64, accent: (f64, f64, f64)) -> Result<(), cairo::Error> {
    if width <= 2.0 || height <= 2.0 {
        return Ok(());
    }
    cr.set_source_rgb(accent.0, accent.1, accent.2);
    cr.set_line_width(2.0);
    rounded_rectangle(cr, 1.0, 1.0, width - 2.0, height - 2.0, 5.0);
    cr.stroke()
}

/// The centre of the pixel column at `x`, where a 1-px line is sharp.
fn crisp(x: f64) -> f64 {
    x.floor() + 0.5
}

fn rounded_rectangle(cr: &cairo::Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let radius = radius.min(width / 2.0).min(height / 2.0);
    cr.new_sub_path();
    cr.arc(x + width - radius, y + radius, radius, -FRAC_PI_2, 0.0);
    cr.arc(x + width - radius, y + height - radius, radius, 0.0, FRAC_PI_2);
    cr.arc(x + radius, y + height - radius, radius, FRAC_PI_2, PI);
    cr.arc(x + radius, y + radius, radius, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: f64 = 3600.0;

    fn hms(h: u32, m: u32, s: u32) -> f64 {
        f64::from(h * 3600 + m * 60 + s)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    /// Views from the whole day down to the narrowest, at both ends of the
    /// day and in between.
    fn views() -> Vec<View> {
        vec![
            View::WHOLE_DAY,
            View { start: 6.0 * HOUR, span: 12.0 * HOUR },
            View { start: 10.0 * HOUR, span: HOUR },
            View { start: 0.0, span: MIN_SPAN },
            View { start: 14.0 * HOUR + 3.0, span: MIN_SPAN },
            View { start: DAY_SECONDS - MIN_SPAN, span: MIN_SPAN },
            View { start: DAY_SECONDS - 3.0 * HOUR, span: 3.0 * HOUR },
        ]
    }

    #[test]
    fn time_and_x_map_back_and_forth_at_every_zoom() {
        for view in views() {
            for width in [1.0, 600.0, 1100.0, 3840.0] {
                for share in [0.0, 0.1, 0.25, 0.5, 0.9, 0.999] {
                    let t = view.start + share * view.span;
                    let x = time_to_x(view, width, t);
                    assert!(close(x, share * width), "{view:?} {width} {share}: x {x}");
                    assert!(close(x_to_time(view, width, x), t), "{view:?} {width} {share}");
                }
                assert!(close(time_to_x(view, width, view.end()), width));
            }
        }
        // Whole day, 1000 px: noon is the middle, an hour is 1000/24 px.
        let whole = View::WHOLE_DAY;
        assert!(close(time_to_x(whole, 1000.0, 12.0 * HOUR), 500.0));
        assert!(close(time_to_x(whole, 1000.0, HOUR), 1000.0 / 24.0));
        // An hour at 10:00 over 600 px: one minute is 10 px.
        let hour = View { start: 10.0 * HOUR, span: HOUR };
        assert!(close(time_to_x(hour, 600.0, hms(10, 1, 0)), 10.0));
        assert!(close(x_to_time(hour, 600.0, 305.0), hms(10, 30, 30)));
        // Outside the view: x outside the widget, as the drawing expects.
        assert!(time_to_x(hour, 600.0, 9.0 * HOUR) < 0.0);
        assert!(time_to_x(hour, 600.0, 12.0 * HOUR) > 600.0);
    }

    #[test]
    fn a_time_from_x_stays_inside_the_day() {
        let whole = View::WHOLE_DAY;
        assert_eq!(x_to_time(whole, 1000.0, -50.0), 0.0);
        let late = x_to_time(whole, 1000.0, 1000.0);
        assert!(late < DAY_SECONDS && late > DAY_SECONDS - 1.0, "{late}");
        assert!(x_to_time(whole, 1000.0, 5000.0) < DAY_SECONDS);
        let end = View { start: DAY_SECONDS - MIN_SPAN, span: MIN_SPAN };
        assert!(x_to_time(end, 600.0, 600.0) < DAY_SECONDS);
        // No width, or no x: the window's start, never NaN.
        assert_eq!(x_to_time(end, 0.0, 10.0), end.start);
        assert_eq!(x_to_time(end, 600.0, f64::NAN), end.start);
        assert_eq!(time_to_x(end, 0.0, 10.0), 0.0);
    }

    #[test]
    fn views_are_clamped_to_the_zoom_limits_and_the_day() {
        // Span limits.
        assert_eq!(clamp_view(View { start: 3600.0, span: 10.0 }), View { start: 3600.0, span: MIN_SPAN });
        assert_eq!(clamp_view(View { start: 0.0, span: 2.0 * DAY_SECONDS }), View::WHOLE_DAY);
        assert_eq!(clamp_view(View { start: 5000.0, span: DAY_SECONDS }), View::WHOLE_DAY);
        // Both ends of the day.
        assert_eq!(clamp_view(View { start: -500.0, span: HOUR }), View { start: 0.0, span: HOUR });
        assert_eq!(
            clamp_view(View { start: 23.5 * HOUR, span: HOUR }),
            View { start: DAY_SECONDS - HOUR, span: HOUR }
        );
        // Inside: unchanged.
        let inside = View { start: 10.0 * HOUR, span: 2.0 * HOUR };
        assert_eq!(clamp_view(inside), inside);
        // Garbage in, a valid view out.
        assert_eq!(clamp_view(View { start: f64::NAN, span: f64::NAN }), View::WHOLE_DAY);
        assert_eq!(clamp_view(View { start: 0.0, span: f64::INFINITY }), View::WHOLE_DAY);
    }

    #[test]
    fn zooming_keeps_the_anchor_where_it_is_on_screen() {
        let width = 1100.0;
        let view = View { start: 8.0 * HOUR, span: 4.0 * HOUR };
        for anchor in [8.0 * HOUR, 9.0 * HOUR + 17.0, 10.0 * HOUR, 11.9 * HOUR] {
            for factor in [ZOOM_STEP, 1.0 / ZOOM_STEP, 2.0, 0.75] {
                let zoomed = zoom_view(view, anchor, factor);
                assert!(close(zoomed.span, view.span / factor));
                assert!(close(time_to_x(zoomed, width, anchor), time_to_x(view, width, anchor)));
            }
        }
        // Zooming in from the whole day at either edge keeps the edge put.
        let left = zoom_view(View::WHOLE_DAY, 0.0, ZOOM_STEP);
        assert_eq!(left.start, 0.0);
        assert!(close(left.span, DAY_SECONDS / ZOOM_STEP));
        let right = zoom_view(View::WHOLE_DAY, DAY_SECONDS, ZOOM_STEP);
        assert!(close(right.end(), DAY_SECONDS));
        assert!(close(time_to_x(right, width, DAY_SECONDS), width));
        // Zooming out at an edge would leave the day: the clamp moves it back.
        let edge = View { start: 0.0, span: HOUR };
        let out = zoom_view(edge, 30.0 * 60.0, 1.0 / 4.0);
        assert_eq!(out, View { start: 0.0, span: 4.0 * HOUR });
    }

    #[test]
    fn zoom_stops_at_two_minutes_and_at_the_whole_day() {
        let mut view = View::WHOLE_DAY;
        for _ in 0..100 {
            view = zoom_view(view, 14.0 * HOUR, wheel_factor(-1.0));
        }
        assert_eq!(view.span, MIN_SPAN);
        assert!(view.contains(14.0 * HOUR));
        for _ in 0..100 {
            view = zoom_view(view, 14.0 * HOUR, wheel_factor(1.0));
        }
        assert_eq!(view, View::WHOLE_DAY);
        // A factor that is no zoom changes nothing.
        let hour = View { start: 10.0 * HOUR, span: HOUR };
        assert_eq!(zoom_view(hour, 10.5 * HOUR, 0.0), hour);
        assert_eq!(zoom_view(hour, 10.5 * HOUR, f64::NAN), hour);
        assert_eq!(zoom_view(hour, f64::NAN, 2.0), hour);
    }

    #[test]
    fn wheel_up_zooms_in_and_fractions_zoom_smoothly() {
        assert!(close(wheel_factor(-1.0), ZOOM_STEP));
        assert!(close(wheel_factor(1.0), 1.0 / ZOOM_STEP));
        assert!(close(wheel_factor(0.0), 1.0));
        assert!(close(wheel_factor(-0.5), ZOOM_STEP.sqrt()));
        assert!(close(wheel_factor(-2.0), ZOOM_STEP * ZOOM_STEP));
    }

    #[test]
    fn panning_moves_the_window_within_the_day() {
        let view = View { start: 10.0 * HOUR, span: HOUR };
        assert_eq!(pan_view(view, 600.0), View { start: 10.0 * HOUR + 600.0, span: HOUR });
        assert_eq!(pan_view(view, -600.0), View { start: 10.0 * HOUR - 600.0, span: HOUR });
        assert_eq!(pan_view(view, -20.0 * HOUR), View { start: 0.0, span: HOUR });
        assert_eq!(pan_view(view, 20.0 * HOUR), View { start: DAY_SECONDS - HOUR, span: HOUR });
        assert_eq!(pan_view(View::WHOLE_DAY, 5000.0), View::WHOLE_DAY);
        assert_eq!(pan_view(view, f64::NAN), view);
    }

    #[test]
    fn following_pages_only_when_the_cursor_leaves_the_window() {
        let view = View { start: 10.0 * HOUR, span: HOUR };
        // Visible (the start is in, the end is not): unchanged.
        assert_eq!(follow_view(view, 10.0 * HOUR), view);
        assert_eq!(follow_view(view, 10.5 * HOUR), view);
        // Past the end: paged, the cursor a tenth in from the left.
        let paged = follow_view(view, 11.0 * HOUR);
        assert_eq!(paged, View { start: 11.0 * HOUR - 360.0, span: HOUR });
        assert!(close(time_to_x(paged, 1000.0, 11.0 * HOUR), 100.0));
        // Before the start (a seek back): the same.
        assert_eq!(follow_view(view, 8.0 * HOUR), View { start: 8.0 * HOUR - 360.0, span: HOUR });
        // Near the ends of the day the clamp wins.
        assert_eq!(follow_view(view, 60.0), View { start: 0.0, span: HOUR });
        assert_eq!(follow_view(view, 23.95 * HOUR), View { start: DAY_SECONDS - HOUR, span: HOUR });
        // Past midnight (a file crossing it): the last window, nothing else.
        assert_eq!(follow_view(view, DAY_SECONDS + 30.0), View { start: DAY_SECONDS - HOUR, span: HOUR });
        assert_eq!(follow_view(View::WHOLE_DAY, 12.0 * HOUR), View::WHOLE_DAY);
        assert_eq!(follow_view(view, f64::NAN), view);
    }

    #[test]
    fn spans_merge_where_they_overlap_or_touch() {
        // Overlap.
        assert_eq!(merge_spans(&[(10.0, 20.0), (15.0, 30.0)]), vec![(10.0, 30.0)]);
        // Abut (half-open spans that touch leave no gap).
        assert_eq!(merge_spans(&[(10.0, 20.0), (20.0, 30.0)]), vec![(10.0, 30.0)]);
        // Contained.
        assert_eq!(merge_spans(&[(10.0, 50.0), (20.0, 30.0)]), vec![(10.0, 50.0)]);
        // Unsorted, with a gap.
        assert_eq!(
            merge_spans(&[(100.0, 200.0), (10.0, 20.0), (15.0, 40.0), (300.0, 310.0)]),
            vec![(10.0, 40.0), (100.0, 200.0), (300.0, 310.0)]
        );
        // Empty.
        assert!(merge_spans(&[]).is_empty());
        // Before midnight and past the day are kept as they are; only
        // drawing clips.
        assert_eq!(
            merge_spans(&[(-300.0, 120.0), (86_000.0, 86_700.0)]),
            vec![(-300.0, 120.0), (86_000.0, 86_700.0)]
        );
        // Empty, reversed and NaN spans are dropped.
        assert_eq!(
            merge_spans(&[(50.0, 50.0), (60.0, 40.0), (f64::NAN, 10.0), (0.0, f64::NAN), (1.0, 2.0)]),
            vec![(1.0, 2.0)]
        );
    }

    #[test]
    fn sorted_spans_keep_their_overlaps() {
        assert_eq!(
            sort_spans(&[(30.0, 40.0), (10.0, 25.0), (20.0, 35.0), (5.0, 5.0)]),
            vec![(10.0, 25.0), (20.0, 35.0), (30.0, 40.0)]
        );
        // Equal starts: the shorter first.
        assert_eq!(sort_spans(&[(10.0, 30.0), (10.0, 20.0)]), vec![(10.0, 20.0), (10.0, 30.0)]);
    }

    /// Two files overlapping by a second, a gap, and a third file.
    fn overlapping_day() -> Vec<(f64, f64)> {
        sort_spans(&[
            (hms(11, 0, 0), hms(11, 30, 0)),
            (hms(8, 21, 7), hms(9, 21, 8)),
            (hms(9, 21, 7), hms(10, 21, 5)),
        ])
    }

    #[test]
    fn a_time_finds_its_file_as_the_client_does() {
        let files = overlapping_day();
        assert_eq!(file_for_time(&files, hms(8, 30, 0)), Some((0, hms(8, 30, 0))));
        // In the overlap: the later file.
        assert_eq!(file_for_time(&files, hms(9, 21, 7)), Some((1, hms(9, 21, 7))));
        // A file's end is not in it (half-open).
        assert_eq!(file_for_time(&files, hms(9, 21, 8)), Some((1, hms(9, 21, 8))));
        // In the gap and before the first file: the next file from its start.
        assert_eq!(file_for_time(&files, hms(10, 40, 0)), Some((2, hms(11, 0, 0))));
        assert_eq!(file_for_time(&files, hms(10, 21, 5)), Some((2, hms(11, 0, 0))));
        assert_eq!(file_for_time(&files, 0.0), Some((0, hms(8, 21, 7))));
        // From the last file's end on: nothing.
        assert_eq!(file_for_time(&files, hms(11, 30, 0)), None);
        assert_eq!(file_for_time(&files, hms(23, 0, 0)), None);
        // Sub-second times and a file from before midnight.
        assert_eq!(file_for_time(&files, hms(11, 29, 59) + 0.9), Some((2, hms(11, 29, 59) + 0.9)));
        let midnight = [(-120.0, 300.0)];
        assert_eq!(file_for_time(&midnight, 0.0), Some((0, 0.0)));
        assert_eq!(file_for_time(&midnight, 300.0), None);
        assert_eq!(file_for_time(&[], 10.0), None);
    }

    fn event(start: f64, end: f64, label: &str) -> TimelineEvent {
        TimelineEvent { start, end, label: label.to_string() }
    }

    #[test]
    fn events_are_sorted_merged_for_drawing_and_named_under_the_pointer() {
        let events = sort_events(&[
            event(hms(11, 7, 27), hms(11, 7, 57), "Animal"),
            event(hms(2, 11, 22), hms(2, 11, 49), "Motion"),
            event(hms(2, 11, 21), hms(2, 11, 55), "Animal, Person"),
            event(hms(2, 11, 50), hms(2, 12, 10), "Motion"),
            // Reversed, empty and NaN windows are dropped.
            event(hms(3, 0, 0), hms(2, 0, 0), "Motion"),
            event(hms(3, 0, 0), hms(3, 0, 0), "Motion"),
            event(f64::NAN, hms(3, 0, 5), "Motion"),
        ]);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].start, hms(2, 11, 21));
        assert_eq!(events[3].start, hms(11, 7, 27));
        // Overlapping and touching windows draw as one bar.
        let spans: Vec<(f64, f64)> = events.iter().map(|e| (e.start, e.end)).collect();
        assert_eq!(merge_spans(&spans), vec![(hms(2, 11, 21), hms(2, 12, 10)), (hms(11, 7, 27), hms(11, 7, 57))]);
        // The tooltip names what lies under the pointer, each label once.
        assert_eq!(labels_at(&events, hms(2, 11, 30)), vec!["Animal, Person", "Motion"]);
        assert_eq!(labels_at(&events, hms(2, 11, 21)), vec!["Animal, Person"]);
        assert_eq!(labels_at(&events, hms(2, 11, 49)), vec!["Animal, Person"], "a window's end is not in it");
        assert_eq!(labels_at(&events, hms(2, 12, 0)), vec!["Motion"]);
        assert!(labels_at(&events, hms(2, 12, 10)).is_empty());
        assert!(labels_at(&events, hms(10, 0, 0)).is_empty());
        assert_eq!(labels_at(&events, hms(11, 7, 40)), vec!["Animal"]);
        assert!(labels_at(&[], hms(11, 7, 40)).is_empty());
        let twice = sort_events(&[event(10.0, 20.0, "Motion"), event(12.0, 30.0, "Motion")]);
        assert_eq!(labels_at(&twice, 15.0), vec!["Motion"]);
    }

    #[test]
    fn ticks_are_at_least_the_minimum_apart() {
        // The whole day over 1100 px: an hour is 45.8 px, two are enough.
        assert_eq!(tick_step(DAY_SECONDS, 1100.0, 64.0), 7200.0);
        // Two minutes over 600 px: one a minute (300 px apart).
        assert_eq!(tick_step(MIN_SPAN, 600.0, 64.0), 60.0);
        // An hour over 1000 px: 1000/12 px every five minutes.
        assert_eq!(tick_step(HOUR, 1000.0, 64.0), 300.0);
        // Too narrow for any: the coarsest.
        assert_eq!(tick_step(DAY_SECONDS, 100.0, 64.0), 21600.0);
        assert_eq!(tick_step(DAY_SECONDS, 0.0, 64.0), 21600.0);
    }

    #[test]
    fn tick_times_cover_the_window_and_never_leave_the_day() {
        let whole = ticks(View::WHOLE_DAY, 1100.0);
        assert_eq!(whole.first(), Some(&0.0));
        assert_eq!(whole.last(), Some(&DAY_SECONDS));
        assert_eq!(whole.len(), 13);
        let late = ticks(View { start: DAY_SECONDS - MIN_SPAN, span: MIN_SPAN }, 600.0);
        assert_eq!(late, vec![DAY_SECONDS - 120.0, DAY_SECONDS - 60.0, DAY_SECONDS]);
        let odd = ticks(View { start: hms(10, 0, 30), span: MIN_SPAN }, 600.0);
        assert_eq!(odd, vec![hms(10, 1, 0), hms(10, 2, 0)]);
    }

    #[test]
    fn labels_run_from_midnight_to_midnight() {
        assert_eq!(format_tick(0.0), "00:00");
        assert_eq!(format_tick(hms(13, 5, 0)), "13:05");
        assert_eq!(format_tick(86_400.0), "24:00");
        assert_eq!(format_tick(-10.0), "00:00");
        assert_eq!(format_tick(90_000.0), "24:00");
        assert_eq!(format_clock(hms(14, 3, 27) + 0.8), "14:03:27");
        assert_eq!(format_clock(LAST_INSTANT), "23:59:59");
        assert_eq!(format_clock(f64::NAN), "00:00:00");
    }
}
