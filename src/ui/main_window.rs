//! The application window: a device sidebar next to the Live View and
//! Playback pages, switched in the header. While Playback is shown, live
//! video is suspended and the sidebar picks the device to play back from;
//! leaving the page brings live video back as it was.

use crate::ui::bridge::{spawn_device, Carried, ConnectTarget, DeviceEvent, DeviceLink, ReplaySinkRequest, SinkRequest};
use crate::ui::device_settings::{self, DeviceSettings};
use crate::ui::device_store::{self, Device};
use crate::ui::settings_pages;
use crate::ui::playback_page::{self, PlaybackPage, Readiness};
use crate::ui::sidebar::{Handlers, Sidebar, Status};
use crate::ui::video_view::VideoView;
use crate::ui::zoom_picture::ZoomPicture;
use crate::ui::audio::{AudioOutput, Microphone};
use crate::ui::remote::{self, RemoteControl};
use crate::ui::camera_settings::{self, CameraSettings};
use crate::ui::settings::Settings;
use crate::ui::{dialogs, secrets};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, Button, DropDown, EventControllerKey,
    GestureClick, HeaderBar, IconTheme, Image, Label, Orientation, Overlay, Paned, PropagationPhase, ScaleButton,
    Revealer, RevealerTransitionType, Stack, StackSwitcher, ToggleButton,
};
use reoling::{
    looks_multi_channel, CameraPage, DeviceKind, DevicePage, SettingsGates, SettingsTarget, SettingsWrite,
    StreamProfile,
};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// The speed the official app moves a camera at.
const PTZ_SPEED: u8 = 32;

/// How long the window-close and signal handlers give the network threads to
/// get the disconnect packets out before the process goes away.
/// Space to the left of the logo, matching what the header leaves above and
/// below it.
const LOGO_MARGIN: i32 = 5;

const DISCONNECT_GRACE: Duration = Duration::from_millis(200);

/// Why a live picture was asked for.
enum SnapPurpose {
    /// A new preset (id, name): saved with this picture and the current position.
    Add(u8, String),
    /// An existing preset's Edit dialog: a new picture for it.
    Edit(u8),
}

/// What a password typed in the login dialog does to the keyring once it
/// has logged in ("Remember password" checked or not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Remember {
    Store,
    Forget,
}

/// A login typed in the login dialog that has not logged in yet. Its user
/// name and password stay in memory only: the device list on disk and the
/// keyring keep the old pair until the typed one has logged in, so a restart
/// in between never pairs the typed user name with the saved password.
struct TypedLogin {
    username: String,
    remember: Remember,
}

/// What a login does to the device's saved password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyringStep {
    Keep,
    Store,
    Forget,
}

/// The keyring changes only after a login: a typed password the device
/// accepted is stored or dropped as the dialog said, and the saved password
/// is forgotten only when the device calls it wrong (`logged_in` false is a
/// `LoginRejected`, never any other refusal). `typed` is the dialog's choice
/// while the password in use was typed there and has not logged in yet; a
/// typed password the device calls wrong says nothing about the saved one.
fn keyring_step(logged_in: bool, typed: Option<Remember>) -> KeyringStep {
    match (logged_in, typed) {
        (true, Some(Remember::Store)) => KeyringStep::Store,
        (true, Some(Remember::Forget)) => KeyringStep::Forget,
        (true, None) | (false, Some(_)) => KeyringStep::Keep,
        (false, None) => KeyringStep::Forget,
    }
}

/// Whether the stream selector can be used: while the watched stream plays,
/// and after its start failed or it broke off, so that another stream can be
/// picked (the selector starts it). Not while a start is under way, nor with
/// nothing watched.
fn stream_selectable(watching: bool, streaming: bool, refused: bool) -> bool {
    watching && (streaming || refused)
}

/// A device's live connection. `id` tells a replaced connection's late events
/// apart from the current one's.
struct Link {
    id: u64,
    link: DeviceLink,
}

/// Where a trial stands (see `Trials`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Its forms go out on the device's link, which also writes how it ends.
    OnLink,
    /// That link has closed for the legacy cipher step: the next connection arms it.
    Reconnecting,
    /// The connection with the step armed is being made: once logged in, it sends the last forms.
    Armed,
}

/// A trial on one device (see `MainWindow::start_trial`): the channel and the stream the device refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Trial {
    channel: u8,
    refused: StreamProfile,
    phase: Phase,
}

/// What `Trials::on_connected` asks the window to do.
#[derive(Debug, PartialEq, Eq)]
enum AfterLogin {
    Nothing,
    /// Send the trial's last forms on the new connection, under the trial's number `gen`.
    Continue { channel: u8, refused: StreamProfile, gen: u64 },
    /// The user no longer watches that channel: the trial is over, and the window writes so.
    Cancelled,
}

/// The one owner of the trials: what each device's trial has reached across its connections, and the refusal that
/// offers one. The window asks it and does what it answers; nothing else keeps trial state.
#[derive(Debug, Default)]
struct Trials {
    running: HashMap<String, Trial>,
    /// The device whose live view the device itself refused last: the channel and the stream it refused.
    refusal: Option<(String, u8, StreamProfile)>,
    /// Each device's trial number, shared with its connections (see `DeviceLink::try_live`): a trial's form is admitted
    /// for sending only while it still equals the number the trial started under, and `cancel` moves it on before it
    /// returns.
    epochs: HashMap<String, reoling::TrialGate>,
}

impl Trials {
    /// `key`'s trial number, for its connections to read.
    fn epoch(&mut self, key: &str) -> reoling::TrialGate {
        self.epochs.entry(key.to_string()).or_default().clone()
    }

    /// No new form of `key`'s current trial is admitted once this has returned (see `TrialGate`).
    fn move_on(&mut self, key: &str) {
        self.epoch(key).cancel();
    }

    /// The device refused `stream` of `channel`: a trial can be offered for it.
    fn refused(&mut self, key: &str, channel: u8, stream: StreamProfile) {
        self.refusal = Some((key.to_string(), channel, stream));
    }

    /// Another stream of `key` starts, or one plays: its refusal no longer stands.
    fn clear_refusal(&mut self, key: &str) {
        if self.refusal.as_ref().is_some_and(|r| r.0 == key) {
            self.refusal = None;
        }
    }

    /// Whether the device's menu offers a trial: the diagnostic log is being written (`recording`), the device
    /// refused the stream being watched, and no trial runs on it.
    fn offer(&self, key: &str, recording: bool) -> bool {
        recording && self.refusal.as_ref().is_some_and(|r| r.0 == key) && !self.running.contains_key(key)
    }

    /// Starts the trial `offer` offers: the channel and the stream to try on the link, and the trial's number.
    fn start(&mut self, key: &str, recording: bool) -> Option<(u8, StreamProfile, u64)> {
        if !self.offer(key, recording) {
            return None;
        }
        let (_, channel, refused) = self.refusal.take()?;
        self.running.insert(key.to_string(), Trial { channel, refused, phase: Phase::OnLink });
        Some((channel, refused, self.epoch(key).number()))
    }

    /// The user ended `key`'s trial (Stop, another device or channel, closing, the log turned off): the phase it
    /// was in, `None` when none ran. No new form of it is admitted once this has returned (`move_on`); the window then
    /// wakes the link (`Phase::OnLink`, `Phase::Armed`) and writes the end line.
    fn cancel(&mut self, key: &str) -> Option<Phase> {
        let phase = self.running.remove(key).map(|t| t.phase)?;
        self.move_on(key);
        Some(phase)
    }

    /// The devices whose trial runs.
    fn running(&self) -> Vec<String> {
        self.running.keys().cloned().collect()
    }

    /// The trial ended by itself (a stream plays, every form was refused, the connection went) or the device left:
    /// the phase it was in, `None` when none ran.
    fn end(&mut self, key: &str) -> Option<Phase> {
        self.running.remove(key).map(|t| t.phase)
    }

    /// The trial's link has closed for the device to connect again: `Some(true)` when the next connection arms the
    /// legacy cipher step, `Some(false)` when the trial is over, `None` when none runs (it was cancelled: the
    /// device connects as always).
    fn on_reconnect(&mut self, key: &str, legacy: bool) -> Option<bool> {
        let trial = self.running.get_mut(key)?;
        if legacy && trial.phase == Phase::OnLink {
            trial.phase = Phase::Reconnecting;
            return Some(true);
        }
        self.running.remove(key);
        Some(false)
    }

    /// A connection to `key` is made now: whether it arms the legacy cipher step (for this connection only).
    fn arm(&mut self, key: &str) -> bool {
        match self.running.get_mut(key) {
            Some(trial) if trial.phase == Phase::Reconnecting => {
                trial.phase = Phase::Armed;
                true
            }
            _ => false,
        }
    }

    /// `key` has logged in; `watching`: the channel of it the user watches, if any.
    fn on_connected(&mut self, key: &str, watching: Option<u8>) -> AfterLogin {
        let Some(trial) = self.running.get_mut(key).filter(|t| t.phase == Phase::Armed) else {
            return AfterLogin::Nothing;
        };
        if watching != Some(trial.channel) {
            self.cancel(key);
            return AfterLogin::Cancelled;
        }
        trial.phase = Phase::OnLink;
        let (channel, refused) = (trial.channel, trial.refused);
        AfterLogin::Continue { channel, refused, gen: self.epoch(key).number() }
    }
}

/// Ends `key`'s trial for the user, as the window does (see `Paths::cancel_trial`): `Trials::cancel`, after
/// which no new form of it is admitted, then `link`, the device's current one, is woken while it runs the trial or is
/// armed for its last step, so that it stops waiting for a form under way. The phase the trial was in.
fn end_trial(trials: &mut Trials, link: Option<&DeviceLink>, key: &str) -> Option<Phase> {
    let phase = trials.cancel(key)?;
    wake(link, phase);
    Some(phase)
}

/// Wakes `link` for a trial cancelled in `phase` (see `end_trial`). A link armed for the cipher step then makes way
/// for an ordinary one.
fn wake(link: Option<&DeviceLink>, phase: Phase) {
    if let (Some(link), Phase::OnLink | Phase::Armed) = (link, phase) {
        link.cancel_trial();
    }
}

/// What the window's actions do to the devices' links, trials and carried commands, without a widget: the window's
/// paths (`Paths`) call these through `wired`, and the tests call the same functions.
struct Wiring<'a> {
    trials: &'a mut Trials,
    links: &'a mut HashMap<String, Link>,
    carried: &'a mut HashMap<String, Carried>,
    /// The devices whose trial was cancelled here, in order: the window writes each one's end line once the call
    /// returns (`wire`).
    ended: Vec<String>,
}

impl Wiring<'_> {
    /// Ends `key`'s trial (see `end_trial`): the phase it was in, `None` when none ran.
    fn cancel_trial(&mut self, key: &str) -> Option<Phase> {
        let phase = end_trial(self.trials, self.links.get(key).map(|l| &l.link), key);
        if phase.is_some() {
            self.ended.push(key.to_string());
        }
        phase
    }

    /// Ends every trial (the log turned off or the window closing).
    fn cancel_all_trials(&mut self) {
        for key in self.trials.running() {
            self.cancel_trial(&key);
        }
    }

    /// `key`'s live view stops (Stop, the Playback page, another device watched): its trial ends, and its link
    /// stops the stream; with no link, no carried command starts one. Whether a trial was cancelled.
    fn stop(&mut self, key: &str) -> bool {
        let cancelled = self.cancel_trial(key).is_some();
        match self.links.get(key) {
            Some(l) => l.link.stop(),
            None => self.carried.get_mut(key).into_iter().for_each(|carried| carried.set_live(None)),
        }
        cancelled
    }

    /// Stop and the Playback page: the live view of `watched`, the device watched, stops (`stop`). The device, or
    /// `None` when nothing was watched and nothing was done.
    fn stop_watched(&mut self, watched: Option<String>) -> Option<String> {
        let key = watched?;
        self.stop(&key);
        Some(key)
    }

    /// The diagnostic log is turned on or off in the settings, or the recording check finds it so: off, every
    /// trial ends.
    fn log_switched(&mut self, on: bool) {
        if !on {
            self.cancel_all_trials();
        }
    }

    /// `key` is to be watched instead of `previous`, the device watched so far: a trial on `previous` ends, and
    /// when it is another device its live view stops (`stop`). Whether a trial was cancelled.
    fn leave(&mut self, previous: &str, key: &str) -> bool {
        if previous == key {
            self.cancel_trial(previous).is_some()
        } else {
            self.stop(previous)
        }
    }

    /// `key`'s `channel` streams in `profile`: on its link or, with no link, in place of the live view carried for
    /// it.
    fn play(&mut self, key: &str, channel: u8, profile: StreamProfile) {
        match self.links.get(key) {
            Some(l) => l.link.play(channel, profile),
            None => {
                self.carried.get_mut(key).into_iter().for_each(|carried| carried.set_live(Some((channel, profile))))
            }
        }
    }

    /// The window closes: every trial ends and every link logs out. Whether any link was told.
    fn shutdown(&mut self) -> bool {
        self.cancel_all_trials();
        for l in self.links.values() {
            l.link.shutdown();
        }
        !self.links.is_empty()
    }

    /// A trial's connection has closed (`DeviceEvent::TrialReconnect`): its link goes, and what it left unserved
    /// waits for the device's next connection (`take_carried`). The trial's next step (`Trials::on_reconnect`).
    fn trial_reconnected(&mut self, key: &str, legacy: bool, carried: Carried) -> Option<bool> {
        let step = self.trials.on_reconnect(key, legacy);
        // Dropped first: the link's queue is then complete when the window changes what is carried.
        self.links.remove(key);
        self.carried.insert(key.to_string(), carried);
        step
    }

    /// What `key`'s next connection serves first.
    fn take_carried(&mut self, key: &str) -> Carried {
        self.carried.remove(key).unwrap_or_default()
    }
}

/// Runs `act` on the links, trials and carried commands (see `Wiring`), then `ended` for each trial it cancelled,
/// in order, once the borrows are gone: an end line reads the trials again.
fn wire<R>(
    trials: &RefCell<Trials>,
    links: &RefCell<HashMap<String, Link>>,
    carried: &RefCell<HashMap<String, Carried>>,
    act: impl FnOnce(&mut Wiring) -> R,
    ended: impl Fn(&str),
) -> R {
    let (out, keys) = {
        let (mut trials, mut links, mut carried) = (trials.borrow_mut(), links.borrow_mut(), carried.borrow_mut());
        let mut wiring = Wiring { trials: &mut trials, links: &mut links, carried: &mut carried, ended: Vec::new() };
        let out = act(&mut wiring);
        (out, wiring.ended)
    };
    for key in keys {
        ended(&key);
    }
    out
}

/// The window's paths that end trials, start or stop streams or close links, each one whole: the provided methods
/// hold the sequence, and the tests run them without a display. A path changes links, trials and carried commands
/// only through `wired`. The required methods are the window's own steps, which change its widgets and its own
/// state. Two of those send a command, because what they need lives only in the window: `end_talk` (TalkStop, when
/// a talk is on, as the microphone and the talk button say) and `reconnect` (the next connection, on the GTK main
/// loop).
trait Paths {
    /// `act` on the links, trials and carried commands, then the end line of each trial it cancelled (`wire`).
    fn wired<R>(&self, act: impl FnOnce(&mut Wiring) -> R) -> R;
    fn playing_key(&self) -> Option<String>;
    /// Whether the diagnostic log is being written (`diag::recording`).
    fn recording(&self) -> bool;
    fn refresh_trial_entries(&self);
    fn set_trials_shown(&self, shown: bool);
    /// Ends any talk; the camera is told on the watched device's link.
    fn end_talk(&self);
    fn show_message(&self, text: &str);
    /// Stop's widget work once the live view of `key` has stopped.
    fn stopped(&self, key: &str);
    /// The Playback page's widget work once the live view of `key` has stopped; it starts again when the page is
    /// left.
    fn suspended(&self, key: &str);
    /// While the Playback page is shown, `play` only notes `key` for when the page is left: true then.
    fn noted_for_playback(&self, key: &str) -> bool;
    /// The channel of `key` to watch; `None` once the device is no longer listed.
    fn channel_of(&self, key: &str) -> Option<u8>;
    /// The picture goes before another stream starts.
    fn clear_picture(&self);
    /// The widget work before `channel` of `key` is asked for; the stream to ask for.
    fn starting(&self, key: &str, channel: u8) -> StreamProfile;
    /// The widget work once it was asked for; `switched`: another device than the one watched before.
    fn started(&self, channel: u8, switched: bool);
    /// A trial's connection to `key` has closed: what the window had from it goes.
    fn reconnecting(&self, key: &str);
    /// Opens the device's next connection.
    fn reconnect(self: &Rc<Self>, key: &str);
    /// Gives the device threads time to send their logouts before the process exits.
    fn grace(&self);

    /// The user ended `key`'s trial, if one runs: no new form of it is admitted once this has returned, the link
    /// stops waiting for the one under way, and the log says so.
    fn cancel_trial(&self, key: &str) {
        self.wired(|w| w.cancel_trial(key));
    }

    /// The stop button: the stream stops, its last picture stays and the button becomes Play. The stream is no
    /// longer the one to restore on the next start.
    fn stop(&self) {
        let watched = self.playing_key();
        let Some(key) = self.wired(|w| w.stop_watched(watched)) else { return };
        self.end_talk();
        self.stopped(&key);
    }

    /// Live video makes way for the Playback page: the stream stops but nothing is forgotten (unlike the Stop
    /// button), and it starts again when the page is left.
    fn suspend_live(&self) {
        let watched = self.playing_key();
        let Some(key) = self.wired(|w| w.stop_watched(watched)) else { return };
        self.end_talk();
        self.suspended(&key);
    }

    /// Streams the device's current channel, stopping whatever else was on. While the Playback page is shown, it
    /// only notes the device for when the page is left.
    fn play(&self, key: &str) {
        if self.noted_for_playback(key) {
            return;
        }
        let Some(channel) = self.channel_of(key) else { return };
        // Another stream starts: a trial on the one watched so far ends, and one is offered only after a refusal
        // of the stream being started now.
        let previous = self.playing_key();
        if let Some(previous) = &previous {
            self.wired(|w| w.leave(previous, key));
            self.wired(|w| w.trials.clear_refusal(previous));
            self.refresh_trial_entries();
        }
        self.clear_picture();
        self.end_talk();
        let profile = self.starting(key, channel);
        self.wired(|w| w.play(key, channel, profile));
        self.started(channel, previous.as_deref() != Some(key));
    }

    /// The diagnostic log was switched in the settings to `on`; `apply` puts the new settings in force. Turned off,
    /// the trials end while the log can still say so. The trial entries follow the file actually written.
    fn diagnostics_set(&self, on: bool, apply: impl FnOnce()) {
        self.wired(|w| w.log_switched(on));
        apply();
        self.recording_found(self.recording());
    }

    /// The diagnostic log is found written (`now`) or not: not written, every trial ends; the trial entries are
    /// offered only while it is.
    fn recording_found(&self, now: bool) {
        self.wired(|w| w.log_switched(now));
        self.set_trials_shown(now);
        self.refresh_trial_entries();
    }

    /// The trial's connection has closed (`DeviceEvent::TrialReconnect`): the device connects again, for the
    /// legacy cipher step or as it always does (always, for a trial the user cancelled meanwhile). No Auto Live
    /// View: the trial sends the next Preview itself, and a stream the user asked for meanwhile is among the
    /// commands the new connection serves first.
    fn trial_reconnect(self: &Rc<Self>, key: &str, legacy: bool, message: Option<&str>, carried: Carried) {
        let step = self.wired(|w| w.trial_reconnected(key, legacy, carried));
        if step == Some(false) && self.playing_key().as_deref() == Some(key) {
            self.show_message(message.unwrap_or(crate::ui::bridge::NO_FORM_ACCEPTED));
        }
        self.reconnecting(key);
        self.refresh_trial_entries();
        self.reconnect(key);
    }

    /// Tells every device we are leaving; the threads need a moment to get the packets out before the process
    /// exits.
    fn shutdown_all(&self) {
        if self.wired(|w| w.shutdown()) {
            self.grace();
        }
    }
}

impl Paths for MainWindow {
    fn wired<R>(&self, act: impl FnOnce(&mut Wiring) -> R) -> R {
        wire(&self.trials, &self.links, &self.carried, act, |key| self.trial_cancelled(key))
    }

    fn playing_key(&self) -> Option<String> {
        MainWindow::playing_key(self)
    }

    fn recording(&self) -> bool {
        recording()
    }

    fn refresh_trial_entries(&self) {
        MainWindow::refresh_trial_entries(self);
    }

    fn set_trials_shown(&self, shown: bool) {
        self.sidebar.set_trials_shown(shown);
    }

    fn end_talk(&self) {
        MainWindow::end_talk(self);
    }

    fn show_message(&self, text: &str) {
        MainWindow::show_message(self, text);
    }

    fn stopped(&self, key: &str) {
        self.video.freeze();
        if let Some(sink) = self.video.current_sink() {
            sink.stop_recording();
        }
        self.set_record_button(false);
        self.audio.reset();
        *self.playing.borrow_mut() = None;
        *self.last_played.borrow_mut() = None;
        *self.stopped.borrow_mut() = Some(key.to_string());
        self.streaming.set(false);
        self.persist();
        self.update_controls();
        // As in `suspended`: the camera window has no watched channel to
        // read now, while the device stays connected.
        if self.camera_settings.is_visible() {
            self.camera_settings.live_stopped();
        } else {
            self.camera_settings.stopped_while_closed(key);
        }
    }

    fn suspended(&self, key: &str) {
        *self.resume_live.borrow_mut() = Some(key.to_string());
        // Ends a recording too.
        self.reset_video();
        *self.playing.borrow_mut() = None;
        self.streaming.set(false);
        self.update_controls();
        // The camera window reads over the watched channel's connection:
        // none is watched now (the device itself stays connected). Closed,
        // it gives up a save under way, whose answers are no longer taken.
        if self.camera_settings.is_visible() {
            self.camera_settings.live_stopped();
        } else {
            self.camera_settings.stopped_while_closed(key);
        }
    }

    fn noted_for_playback(&self, key: &str) -> bool {
        if !self.playback.is_shown() {
            return false;
        }
        *self.resume_live.borrow_mut() = Some(key.to_string());
        if self.playback.device_key().as_deref() == Some(key) {
            self.sidebar.select(key);
        }
        true
    }

    fn channel_of(&self, key: &str) -> Option<u8> {
        self.device(key).map(|d| d.channel)
    }

    fn clear_picture(&self) {
        // A stream asked for anew: the selector offers what the channel advertises.
        self.trial_stream.borrow_mut().take();
        self.reset_video();
        self.set_spotlight_button(false);
    }

    fn starting(&self, key: &str, channel: u8) -> StreamProfile {
        *self.stopped.borrow_mut() = None;
        *self.playing.borrow_mut() = Some(key.to_string());
        *self.last_played.borrow_mut() = Some(key.to_string());
        self.streaming.set(false);
        self.refused.set(false);
        self.refresh_streams();
        self.persist();
        self.sidebar.select(key);
        self.sidebar.mark_channel(key, channel);
        self.show_message("Starting…");
        self.profile.get()
    }

    fn started(&self, channel: u8, switched: bool) {
        self.update_controls();
        // The camera window shows the watched channel: when that is another
        // one now (or the same one after a stop or a lost connection), it is
        // opened again for it, where it is; its old tabs would never fill.
        if self.camera_settings.is_visible() && (switched || !self.camera_settings.is_open_for(channel)) {
            self.open_camera_settings(false);
        }
    }

    fn reconnecting(&self, key: &str) {
        self.connected.borrow_mut().remove(key);
        self.channel_lists.borrow_mut().remove(key);
        // As for a new login: the Playback page waits for the new connection, the camera window gives up what it
        // had out on the old one.
        self.playback.device_reconnecting(key);
        self.camera_settings.reconnecting(key);
    }

    fn reconnect(self: &Rc<Self>, key: &str) {
        self.connect(key, false);
    }

    fn grace(&self) {
        std::thread::sleep(DISCONNECT_GRACE);
    }
}

/// The streams a channel advertises: what the device's channel list says (`listed`), and for a device without
/// one, a standalone camera, main and sub, with the third stream between them when its support table tells of
/// one (`ChannelAbilities::extern_stream`).
fn advertised_streams(
    listed: Option<Vec<StreamProfile>>,
    abilities: Option<reoling::ChannelAbilities>,
) -> Vec<StreamProfile> {
    listed.unwrap_or_else(|| {
        if abilities.is_some_and(|a| a.extern_stream) {
            vec![StreamProfile::Main, StreamProfile::Extern, StreamProfile::Sub]
        } else {
            vec![StreamProfile::Main, StreamProfile::Sub]
        }
    })
}

/// The streams the selector offers: what the channel advertises, and the stream a trial got the device to play
/// (`accepted`) when the channel does not advertise it, so the selector shows what plays.
fn offered_streams(mut advertised: Vec<StreamProfile>, accepted: Option<StreamProfile>) -> Vec<StreamProfile> {
    if let Some(stream) = accepted.filter(|s| !advertised.contains(s)) {
        advertised.insert(0, stream);
    }
    advertised
}

/// The line added under a refusal while the diagnostic log is on.
const TRIAL_HINT: &str = "To try other request forms, choose “Try other live view requests…” in the device's menu.";

pub struct MainWindow {
    window: ApplicationWindow,
    header: HeaderBar,
    video: Rc<VideoView>,
    message: Label,
    sidebar: Rc<Sidebar>,
    sidebar_toggle: ToggleButton,
    bars: GtkBox,
    fullscreen_button: Button,
    stream: DropDown,
    stop: Button,
    snapshot: Button,
    siren: Button,
    spotlight: ToggleButton,
    ptz: Button,
    remote: Rc<RemoteControl>,
    camera_settings_button: Button,
    camera_settings: Rc<CameraSettings>,
    /// The read-only settings of a device itself.
    device_settings: Rc<DeviceSettings>,
    talk: ToggleButton,
    updating_talk: Cell<bool>,
    /// A talk was asked for and the camera has not answered yet.
    talk_pending: Cell<bool>,
    talk_retried: Cell<bool>,
    microphone: RefCell<Option<Microphone>>,
    updating_spotlight: Cell<bool>,
    record: ToggleButton,
    /// Set while the code, not the user, flips the record button.
    updating_record: Cell<bool>,
    notice: Label,
    notice_generation: Cell<u64>,
    live: GtkBox,
    fullscreen_bar: Revealer,
    bar_generation: Cell<u64>,
    pointer_on_bar: Cell<bool>,
    /// The device whose stream the user stopped; the Play button restarts it.
    stopped: RefCell<Option<String>>,
    previous: Button,
    next: Button,
    /// Whether the picture on screen is really flowing.
    streaming: Cell<bool>,
    /// The watched channel's last start failed or its stream broke off (the
    /// camera refused it, say): the stream selector stays usable, so that
    /// another stream can be tried. `play` and the first frame clear it; a
    /// late failure of the channel watched before and a stream that waits
    /// for a download do not set it.
    refused: Cell<bool>,
    /// The stream the user prefers, and the ones the watched device offers
    /// (what the dropdown lists, in the same order).
    profile: Cell<StreamProfile>,
    stream_options: RefCell<Vec<StreamProfile>>,
    refreshing_streams: Cell<bool>,

    devices: RefCell<Vec<Device>>,
    passwords: RefCell<HashMap<String, String>>,
    /// Devices whose password was typed in the login dialog and has not
    /// logged in yet.
    typed_logins: RefCell<HashMap<String, TypedLogin>>,
    /// Every device is connected all the time; streaming is separate.
    links: RefCell<HashMap<String, Link>>,
    preset_queue: RefCell<std::collections::VecDeque<u8>>,
    preset_in_flight: Cell<bool>,
    preset_queue_generation: Cell<u64>,
    snap_purpose: RefCell<Option<SnapPurpose>>,
    /// The devices that have logged in on their current link.
    connected: RefCell<HashSet<String>>,
    /// The devices already warned in this run that an answer came from another address than the saved one, and
    /// those whose warning waits for the lost connection to be put away (`DeviceEvent::LanAnswerHeld`).
    lan_warned: RefCell<HashSet<String>>,
    lan_warning_due: RefCell<HashSet<String>>,
    /// The trials, and the refusal that offers one (see `Trials`).
    trials: RefCell<Trials>,
    /// The stream a trial got the watched device to play on a channel: (device, channel, stream). Kept until another
    /// stream is asked for (see `offered_streams`).
    trial_stream: RefCell<Option<(String, u8, StreamProfile)>>,
    /// What a connection a trial ended left unserved, for the device's next connection (see `connect`).
    carried: RefCell<HashMap<String, Carried>>,
    next_link_id: Cell<u64>,
    /// The device being watched, and the one to watch as soon as it is
    /// connected.
    playing: RefCell<Option<String>>,
    autoplay: RefCell<Option<String>>,
    /// Restored on the next start.
    last_played: RefCell<Option<String>>,
    settings: RefCell<Settings>,
    audio: Arc<AudioOutput>,
    sink_request_tx: tokio::sync::mpsc::Sender<SinkRequest>,
    /// The Playback page's picture: recorded footage only, fed by the device
    /// threads through `replay_sink_request_tx`.
    replay_video: Rc<VideoView>,
    replay_sink_request_tx: tokio::sync::mpsc::Sender<ReplaySinkRequest>,
    playback: Rc<PlaybackPage>,
    /// The live stream the Playback page suspended (or the device picked
    /// while it was shown): started again when the page is left.
    resume_live: RefCell<Option<String>>,
    /// Devices whose current connection has sent its channel list (an NVR's
    /// or Hub's recording requests name the camera it lists).
    channel_lists: RefCell<HashSet<String>>,
}

pub fn build(app: &Application) -> Rc<MainWindow> {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Reoling")
        .default_width(1100)
        .default_height(700)
        .build();

    let settings = Settings::load();
    // The configured names are masked before the diagnostic log's first line.
    let (devices, last) = device_store::load();
    for device in &devices {
        conceal_device(device);
    }
    settings.apply();
    // One line on stderr when this system lacks something (see
    // `requirements`); nothing otherwise. Here, not in main(): only the
    // first instance builds the window.
    if let Some(line) = crate::ui::requirements::check().summary_line() {
        eprintln!("{line}");
    }

    let audio = Arc::new(AudioOutput::new(settings.volume));
    let video = Rc::new(VideoView::new());

    // Every connection's network thread asks here for a `VideoSink` once it
    // knows the codec; building the pipeline touches the `ZoomPicture`, so it
    // has to happen on the GTK main thread (see `bridge::spawn_connection`).
    let (sink_request_tx, mut sink_request_rx) = tokio::sync::mpsc::channel::<SinkRequest>(8);
    let video_for_sink = Rc::clone(&video);
    glib::spawn_future_local(async move {
        while let Some((video_type, reply)) = sink_request_rx.recv().await {
            let _ = reply.send(video_for_sink.ensure_sink(video_type));
        }
    });

    // The same for recorded footage, into its own view (answered once the
    // Playback page exists, below).
    let replay_video = Rc::new(VideoView::new_replay(settings.volume));
    let (replay_sink_request_tx, mut replay_sink_request_rx) = tokio::sync::mpsc::channel::<ReplaySinkRequest>(8);

    let main = Rc::new_cyclic(|weak: &std::rc::Weak<MainWindow>| {
        let w = weak.clone();
        let on = |f: fn(&Rc<MainWindow>)| {
            let w = w.clone();
            move || {
                if let Some(m) = w.upgrade() {
                    f(&m)
                }
            }
        };
        let with_key = |f: fn(&Rc<MainWindow>, &str)| {
            let w = w.clone();
            move |key: &str| {
                if let Some(m) = w.upgrade() {
                    f(&m, key)
                }
            }
        };
        let w_channel = w.clone();
        let sidebar = Sidebar::new(Handlers {
            on_add: Box::new(on(MainWindow::add_device_dialog)),
            on_select: Box::new(with_key(MainWindow::activate)),
            on_channel: Box::new(move |key, channel| {
                if let Some(m) = w_channel.upgrade() {
                    m.pick_channel(key, channel)
                }
            }),
            on_settings: Box::new(with_key(MainWindow::open_device_settings)),
            on_relogin: Box::new(with_key(MainWindow::relogin)),
            on_remove: Box::new(with_key(MainWindow::ask_remove)),
            on_trial: Box::new(with_key(MainWindow::start_trial)),
        });

        let header = HeaderBar::new();
        let brand = GtkBox::new(Orientation::Horizontal, 8);
        let logo = Image::from_icon_name("de.dersobi.reoling");
        logo.set_pixel_size(24);
        // The header centres it vertically; give it as much room on the left.
        logo.set_margin_start(LOGO_MARGIN);
        let brand_name = Label::new(Some("Reoling"));
        brand_name.add_css_class("heading");
        brand.append(&logo);
        brand.append(&brand_name);
        header.pack_start(&brand);
        let settings_button = Button::from_icon_name("preferences-system-symbolic");
        settings_button.set_tooltip_text(Some("Settings"));
        header.pack_end(&settings_button);
        let w = weak.clone();
        settings_button.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.settings_dialog();
            }
        });
        window.set_titlebar(Some(&header));

        let sidebar_toggle = ToggleButton::new();
        sidebar_toggle.set_icon_name(sidebar_icon());
        sidebar_toggle.set_tooltip_text(Some("Show or hide the device list"));
        sidebar_toggle.set_active(true);

        let pages = Stack::new();
        let switcher = StackSwitcher::new();
        switcher.set_stack(Some(&pages));
        header.set_title_widget(Some(&switcher));

        // Live view: video (with a status message over it), stream controls,
        // navigation/fullscreen row.
        let message = Label::new(Some("Add a device to start"));
        message.add_css_class("title-2");
        message.add_css_class("dim-label");
        message.set_can_target(false);
        message.set_halign(gtk4::Align::Center);
        message.set_valign(gtk4::Align::Center);
        message.set_wrap(true);
        message.set_justify(gtk4::Justification::Center);
        let overlay = Overlay::new();
        let picture = video.widget();
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        overlay.set_child(Some(picture));
        overlay.add_overlay(&message);

        // A short confirmation ("Snapshot saved…") near the top of the video.
        let notice = Label::new(None);
        notice.add_css_class("osd");
        notice.set_halign(gtk4::Align::Center);
        notice.set_valign(gtk4::Align::Start);
        notice.set_margin_top(12);
        notice.set_visible(false);
        notice.set_can_target(false);
        overlay.add_overlay(&notice);

        // In fullscreen the controls slide in over the bottom of the video
        // whenever the pointer moves, under the picture and as wide as it.
        let fullscreen_bar = Revealer::new();
        fullscreen_bar.set_transition_type(RevealerTransitionType::SlideUp);
        fullscreen_bar.set_valign(gtk4::Align::End);
        fullscreen_bar.set_visible(false);
        overlay.add_overlay(&fullscreen_bar);
        place_bar_under_picture(&overlay, &fullscreen_bar, picture);

        let stop = Button::from_icon_name("media-playback-stop-symbolic");
        stop.set_tooltip_text(Some("Stop"));
        stop.set_sensitive(false);
        let stream = DropDown::from_strings(&["Main stream", "Sub stream"]);
        stream.set_selected(1);
        stream.set_tooltip_text(Some("Stream"));
        stream.set_sensitive(false);
        let controls = GtkBox::new(Orientation::Horizontal, 8);
        controls.set_margin_top(6);
        controls.set_margin_bottom(6);
        controls.set_margin_start(8);
        controls.set_margin_end(8);
        controls.append(&stop);
        let snapshot = Button::from_icon_name("camera-photo-symbolic");
        snapshot.set_tooltip_text(Some("Save a snapshot"));
        snapshot.set_sensitive(false);
        let record = ToggleButton::new();
        record.set_icon_name("media-record-symbolic");
        record.set_tooltip_text(Some("Record"));
        record.set_sensitive(false);
        controls.append(&snapshot);
        controls.append(&record);
        controls.append(&gtk4::Separator::new(Orientation::Vertical));
        // Camera controls, per channel. Talk is not built yet.
        let talk = ToggleButton::new();
        talk.set_icon_name("audio-input-microphone-symbolic");
        talk.set_tooltip_text(Some("Talk"));
        talk.set_sensitive(false);
        let w = weak.clone();
        talk.connect_toggled(move |button| {
            if let Some(m) = w.upgrade() {
                if !m.updating_talk.get() {
                    m.toggle_talk(button.is_active());
                }
            }
        });
        let siren = Button::from_icon_name(icon_of(&["alarm-symbolic", "dialog-warning-symbolic"]));
        siren.set_tooltip_text(Some("Sound the siren"));
        siren.set_sensitive(false);
        let spotlight = ToggleButton::new();
        spotlight.set_icon_name(icon_of(&["weather-clear-symbolic", "keyboard-brightness-symbolic"]));
        spotlight.set_tooltip_text(Some("Spotlight"));
        spotlight.set_sensitive(false);
        // The camera's remote control opens in a window of its own.
        let ptz = Button::from_icon_name(icon_of(&["input-dpad-symbolic", "object-move-symbolic", "find-location-symbolic"]));
        ptz.set_tooltip_text(Some("Camera control"));
        ptz.set_sensitive(false);
        let w = weak.clone();
        ptz.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.open_remote();
            }
        });
        let (w_move, w_stop, w_zoom, w_focus) = (weak.clone(), weak.clone(), weak.clone(), weak.clone());
        let (w_add, w_goto, w_delete) = (weak.clone(), weak.clone(), weak.clone());
        let (w_calibrate, w_mp_config, w_mp_reset, w_mp_goto, w_mp_image, w_preset_all, w_preset_rename, w_preset_snap, w_next_id) = (
            weak.clone(),
            weak.clone(),
            weak.clone(),
            weak.clone(),
            weak.clone(),
            weak.clone(),
            weak.clone(),
            weak.clone(),
            weak.clone(),
        );
        let remote = RemoteControl::new(remote::Handlers {
            on_move: Box::new(move |command| {
                if let Some(m) = w_move.upgrade() {
                    m.control(move |link, channel| link.ptz(channel, command, PTZ_SPEED));
                }
            }),
            on_stop: Box::new(move || {
                if let Some(m) = w_stop.upgrade() {
                    m.control(|link, channel| link.ptz(channel, "stop", 0));
                }
            }),
            on_zoom: Box::new(move |position| {
                if let Some(m) = w_zoom.upgrade() {
                    m.control(move |link, channel| link.set_zoom(channel, position));
                    m.refresh_zoom_focus_soon();
                }
            }),
            on_focus: Box::new(move |position| {
                if let Some(m) = w_focus.upgrade() {
                    m.control(move |link, channel| link.set_focus(channel, position));
                    m.refresh_zoom_focus_soon();
                }
            }),
            on_next_preset_id: Box::new(move || w_next_id.upgrade().and_then(|m| m.next_preset_id())),
            on_add_preset: Box::new(move |id, name, has_picture| {
                if let Some(m) = w_add.upgrade() {
                    m.add_preset(id, name, has_picture);
                }
            }),
            on_goto_preset: Box::new(move |id| {
                if let Some(m) = w_goto.upgrade() {
                    m.control(move |link, channel| link.goto_preset(channel, id));
                }
            }),
            on_delete_preset: Box::new(move |id| {
                if let Some(m) = w_delete.upgrade() {
                    m.control(move |link, channel| link.delete_preset(channel, id));
                    // The list only changes on the device; ask again shortly.
                    m.query_presets_soon();
                }
            }),
            on_calibrate: Box::new(move || {
                if let Some(m) = w_calibrate.upgrade() {
                    m.control(|link, channel| link.calibrate(channel));
                    m.notify("Calibrating… this can take a few seconds");
                }
            }),
            on_monitor_point_config: Box::new(move |enabled, timeout| {
                if let Some(m) = w_mp_config.upgrade() {
                    m.control(move |link, channel| link.set_monitor_point_config(channel, enabled, timeout));
                }
            }),
            on_reset_monitor_point: Box::new(move |enabled, timeout| {
                if let Some(m) = w_mp_reset.upgrade() {
                    m.control(move |link, channel| link.set_monitor_point_here(channel, enabled, timeout));
                    m.query_monitor_point_soon();
                }
            }),
            on_go_to_monitor_point: Box::new(move |timeout| {
                if let Some(m) = w_mp_goto.upgrade() {
                    m.control(move |link, channel| link.go_to_monitor_point(channel, timeout));
                }
            }),
            on_refresh_monitor_point_image: Box::new(move || {
                if let Some(m) = w_mp_image.upgrade() {
                    m.control(|link, channel| link.query_monitor_point_image(channel));
                }
            }),
            on_refresh_all_presets: Box::new(move || {
                if let Some(m) = w_preset_all.upgrade() {
                    let ids: Vec<u8> = m
                        .playing_key()
                        .and_then(|k| m.device(&k))
                        .map(|d| d.presets.iter().map(|p| p.id).collect())
                        .unwrap_or_default();
                    if std::env::var("REOLING_DEBUG_PRESETS").is_ok() {
                        eprintln!("PRESETS refresh all: {ids:?}");
                    }
                    m.enqueue_preset_images(&ids);
                }
            }),
            on_snapshot_preset: Box::new(move |id| {
                if let Some(m) = w_preset_snap.upgrade() {
                    m.begin_snapshot(SnapPurpose::Edit(id), 0);
                }
            }),
            on_rename_preset: Box::new(move |id, name, with_image| {
                if let Some(m) = w_preset_rename.upgrade() {
                    m.control(move |link, channel| link.modify_preset(channel, id, name.clone(), with_image));
                    m.query_presets_soon();
                }
            }),
        });
        // The camera's own settings (encoding…) open in a window of their own.
        let camera_settings_button = Button::from_icon_name(icon_of(&["emblem-system-symbolic", "preferences-system-symbolic"]));
        camera_settings_button.set_tooltip_text(Some("Camera settings"));
        camera_settings_button.set_sensitive(false);
        let w = weak.clone();
        camera_settings_button.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.open_camera_settings(true);
            }
        });
        let (w_reload, w_save, w_page, w_write) = (weak.clone(), weak.clone(), weak.clone(), weak.clone());
        let camera_settings = CameraSettings::new(camera_settings::Handlers {
            on_reload: Box::new(move |_channel| {
                if let Some(m) = w_reload.upgrade() {
                    let needs_profiles = m.camera_settings.needs_profiles();
                    m.control(move |link, channel| {
                        link.query_encoding(channel);
                        link.query_channel_version(channel);
                        if needs_profiles {
                            link.query_encode_profiles(channel);
                        }
                    });
                }
            }),
            on_save: Box::new(move |_channel, body| {
                if let Some(m) = w_save.upgrade() {
                    m.control(move |link, channel| link.set_encoding(channel, body.clone()));
                }
            }),
            on_read_page: Box::new(move |channel, page, read| {
                w_page.upgrade().is_some_and(|m| m.read_camera_page(channel, page, read))
            }),
            on_write: Box::new(move |key, channel, page, group, writes| {
                w_write.upgrade().is_some_and(|m| m.write_camera_page(key, channel, page, group, writes))
            }),
        });
        // A device's own settings, opened from its sidebar card.
        let w_device_page = weak.clone();
        let device_settings = DeviceSettings::new(device_settings::Handlers {
            on_read_page: Box::new(move |key, page| w_device_page.upgrade().is_some_and(|m| m.read_device_page(key, page))),
        });
        controls.append(&talk);
        controls.append(&siren);
        controls.append(&spotlight);
        controls.append(&ptz);
        controls.append(&camera_settings_button);
        let spacer = GtkBox::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        controls.append(&spacer);
        controls.append(&stream);

        // Bottom row. Buttons without a function yet stay in place, disabled.
        let previous = Button::from_icon_name("go-previous-symbolic");
        previous.set_tooltip_text(Some("Previous channel"));
        previous.set_sensitive(false);
        let next = Button::from_icon_name("go-next-symbolic");
        next.set_tooltip_text(Some("Next channel"));
        next.set_sensitive(false);
        let scrollview = Button::from_icon_name("media-playlist-repeat-symbolic");
        scrollview.set_tooltip_text(Some("Scrollview"));
        scrollview.set_sensitive(false);
        let volume = ScaleButton::new(
            0.0,
            1.0,
            0.05,
            &[
                "audio-volume-muted-symbolic",
                "audio-volume-high-symbolic",
                "audio-volume-low-symbolic",
                "audio-volume-medium-symbolic",
            ],
        );
        volume.set_tooltip_text(Some("Volume"));
        volume.set_value(settings.volume);
        let w = weak.clone();
        volume.connect_value_changed(move |_, value| {
            if let Some(m) = w.upgrade() {
                m.audio.set_volume(value);
                m.replay_video.set_volume(value);
                m.settings.borrow_mut().volume = value;
                m.settings.borrow().save();
            }
        });
        let split = Button::from_icon_name("view-grid-symbolic");
        split.set_tooltip_text(Some("Split view"));
        split.set_sensitive(false);
        let fullscreen_button = Button::from_icon_name("view-fullscreen-symbolic");
        fullscreen_button.set_tooltip_text(Some("Fullscreen"));
        let navigation = GtkBox::new(Orientation::Horizontal, 8);
        navigation.set_margin_top(6);
        navigation.set_margin_bottom(6);
        navigation.set_margin_start(8);
        navigation.set_margin_end(8);
        navigation.append(&sidebar_toggle);
        navigation.append(&previous);
        navigation.append(&next);
        navigation.append(&scrollview);
        let spacer = GtkBox::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        navigation.append(&spacer);
        navigation.append(&volume);
        navigation.append(&split);
        navigation.append(&fullscreen_button);

        let bars = GtkBox::new(Orientation::Vertical, 0);
        bars.append(&controls);
        bars.append(&navigation);

        let live = GtkBox::new(Orientation::Vertical, 0);
        live.append(&overlay);
        live.append(&bars);
        pages.add_titled(&live, Some("live"), "Live View");

        // Recorded footage. Commands only go to a connected device's link.
        let w_link = weak.clone();
        let w_names = weak.clone();
        let w_dir = weak.clone();
        let w_set_dir = weak.clone();
        let playback = PlaybackPage::new(
            Rc::clone(&replay_video),
            playback_page::Handlers {
                with_link: Box::new(move |key, f| {
                    if let Some(m) = w_link.upgrade() {
                        if let Some(l) = m.links.borrow().get(key) {
                            f(&l.link);
                        }
                    }
                }),
                names: Box::new(move |key, channel| {
                    let device = w_names.upgrade().and_then(|m| m.device(key));
                    download_names(device.as_ref(), channel)
                }),
                download_dir: Box::new(move || w_dir.upgrade().and_then(|m| m.media_dir(glib::UserDirectory::Videos))),
                set_download_dir: Box::new(move |dir| {
                    if let Some(m) = w_set_dir.upgrade() {
                        m.settings.borrow_mut().recording_dir = Some(dir);
                        m.settings.borrow().save();
                    }
                }),
            },
        );
        playback.sidebar_toggle().set_icon_name(sidebar_icon());
        sidebar_toggle
            .bind_property("active", playback.sidebar_toggle(), "active")
            .bidirectional()
            .sync_create()
            .build();
        pages.add_titled(playback.widget(), Some("playback"), "Playback");
        let w = weak.clone();
        pages.connect_visible_child_name_notify(move |pages| {
            if let Some(m) = w.upgrade() {
                let on_playback = pages.visible_child_name().as_deref() == Some("playback");
                if on_playback && !m.playback.is_shown() {
                    m.enter_playback();
                } else if !on_playback && m.playback.is_shown() {
                    m.leave_playback();
                }
            }
        });

        let paned = Paned::new(Orientation::Horizontal);
        paned.set_start_child(Some(sidebar.widget()));
        paned.set_end_child(Some(&pages));
        paned.set_resize_start_child(false);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        window.set_child(Some(&paned));

        // Wiring that only needs the weak handle.
        let w = weak.clone();
        stop.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.stop_or_play();
            }
        });
        let w = weak.clone();
        siren.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.control(|link, channel| link.siren(channel));
            }
        });
        let w = weak.clone();
        spotlight.connect_toggled(move |button| {
            if let Some(m) = w.upgrade() {
                if !m.updating_spotlight.get() {
                    let on = button.is_active();
                    m.control(move |link, channel| link.spotlight(channel, on));
                }
            }
        });
        let w = weak.clone();
        snapshot.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.take_snapshot();
            }
        });
        let w = weak.clone();
        record.connect_toggled(move |button| {
            if let Some(m) = w.upgrade() {
                if !m.updating_record.get() {
                    m.toggle_recording(button.is_active());
                }
            }
        });
        // Fullscreen: any pointer movement brings the controls up for a while.
        // A motion event at the last position is no movement: Wayland sends
        // those while the bar itself slides in or out.
        let w = weak.clone();
        let last_position = Cell::new(None);
        let motion = gtk4::EventControllerMotion::new();
        motion.connect_motion(move |_, x, y| {
            if last_position.replace(Some((x, y))) == Some((x, y)) {
                return;
            }
            if let Some(m) = w.upgrade() {
                m.show_fullscreen_bar();
            }
        });
        overlay.add_controller(motion);
        let w = weak.clone();
        let over_bar = gtk4::EventControllerMotion::new();
        over_bar.connect_enter(move |_, _, _| {
            if let Some(m) = w.upgrade() {
                m.pointer_on_bar.set(true);
            }
        });
        let w = weak.clone();
        over_bar.connect_leave(move |_| {
            if let Some(m) = w.upgrade() {
                m.pointer_on_bar.set(false);
                m.show_fullscreen_bar();
            }
        });
        bars.add_controller(over_bar);
        let w = weak.clone();
        stream.connect_selected_notify(move |dropdown| {
            if let Some(m) = w.upgrade() {
                if m.refreshing_streams.get() {
                    return;
                }
                let chosen = m.stream_options.borrow().get(dropdown.selected() as usize).copied();
                if let Some(profile) = chosen {
                    m.profile.set(profile);
                }
                if let Some(key) = m.playing_key() {
                    // Not from inside the dropdown's own handler: its popup
                    // is still closing.
                    glib::idle_add_local_once(move || m.play(&key));
                }
            }
        });
        let w = weak.clone();
        previous.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.step_channel(-1)
            }
        });
        let w = weak.clone();
        next.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.step_channel(1)
            }
        });
        let w = weak.clone();
        fullscreen_button.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                let on = !m.window.is_fullscreen();
                m.set_fullscreen(on)
            }
        });
        let sidebar_for_toggle = Rc::clone(&sidebar);
        sidebar_toggle.connect_toggled(move |t| sidebar_for_toggle.widget().set_visible(t.is_active()));

        // Double-click on the video and Esc toggle/leave fullscreen.
        let w = weak.clone();
        let click = GestureClick::new();
        click.set_button(1);
        click.connect_pressed(move |_, presses, _, _| {
            if presses == 2 {
                if let Some(m) = w.upgrade() {
                    let on = !m.window.is_fullscreen();
                    m.set_fullscreen(on)
                }
            }
        });
        video.widget().add_controller(click);
        let w = weak.clone();
        let keys = EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk4::gdk::Key::Escape {
                if let Some(m) = w.upgrade() {
                    if m.window.is_fullscreen() {
                        m.set_fullscreen(false);
                        return glib::Propagation::Stop;
                    }
                }
            }
            glib::Propagation::Proceed
        });
        window.add_controller(keys);
        // In fullscreen the window takes Esc before the widget with the
        // keyboard focus does: the volume button would take it to close its
        // already closed slider, and the paned, which GTK gives the focus when
        // the controls hide, to cancel a keyboard move of its divider. An open
        // popover of the controls still gets it, and closes. Only our own
        // fullscreen, with the controls on the video, is meant: not one the
        // window manager made by itself.
        let w = weak.clone();
        let escape = EventControllerKey::new();
        escape.set_propagation_phase(PropagationPhase::Capture);
        escape.connect_key_pressed(move |_, key, _, _| {
            if key == gtk4::gdk::Key::Escape {
                if let Some(m) = w.upgrade() {
                    if m.window.is_fullscreen()
                        && m.fullscreen_bar.child().is_some()
                        && !popover_open_below(&m.bars)
                    {
                        m.set_fullscreen(false);
                        return glib::Propagation::Stop;
                    }
                }
            }
            glib::Propagation::Proceed
        });
        window.add_controller(escape);
        // The window manager makes the window fullscreen after `set_fullscreen`
        // has returned: the controls' hide timer starts from here, so the bar
        // also goes away when the pointer never moves. When the window manager
        // ends fullscreen by itself, the controls go back below the video.
        let w = weak.clone();
        window.connect_fullscreened_notify(move |window| {
            if let Some(m) = w.upgrade() {
                if m.fullscreen_bar.child().is_none() {
                    return;
                }
                if window.is_fullscreen() {
                    m.show_fullscreen_bar();
                } else {
                    // Not from inside `unfullscreen()`, which may notify before
                    // `set_fullscreen(false)` has moved the controls back.
                    glib::idle_add_local_once(move || {
                        if !m.window.is_fullscreen() && m.fullscreen_bar.child().is_some() {
                            m.set_fullscreen(false);
                        }
                    });
                }
            }
        });

        // Best-effort disconnect on close: the network thread needs a moment
        // to get the packet out before the process exits.
        let w = weak.clone();
        window.connect_close_request(move |_| {
            if let Some(m) = w.upgrade() {
                m.shutdown_all();
            }
            glib::Propagation::Proceed
        });

        MainWindow {
            window: window.clone(),
            header,
            video: Rc::clone(&video),
            message,
            sidebar,
            sidebar_toggle,
            bars,
            fullscreen_button,
            stream,
            stop,
            snapshot,
            siren,
            spotlight,
            ptz,
            remote,
            camera_settings_button,
            camera_settings,
            device_settings,
            talk,
            updating_talk: Cell::new(false),
            talk_pending: Cell::new(false),
            talk_retried: Cell::new(false),
            microphone: RefCell::new(None),
            updating_spotlight: Cell::new(false),
            record,
            updating_record: Cell::new(false),
            notice,
            notice_generation: Cell::new(0),
            live: live.clone(),
            fullscreen_bar,
            bar_generation: Cell::new(0),
            pointer_on_bar: Cell::new(false),
            stopped: RefCell::new(None),
            previous,
            next,
            streaming: Cell::new(false),
            refused: Cell::new(false),
            // Start on the lightest stream; heavier ones are opt-in.
            profile: Cell::new(StreamProfile::Sub),
            stream_options: RefCell::new(vec![StreamProfile::Main, StreamProfile::Sub]),
            refreshing_streams: Cell::new(false),
            devices: RefCell::new(Vec::new()),
            passwords: RefCell::new(HashMap::new()),
            typed_logins: RefCell::new(HashMap::new()),
            links: RefCell::new(HashMap::new()),
            preset_queue: RefCell::new(std::collections::VecDeque::new()),
            preset_in_flight: Cell::new(false),
            preset_queue_generation: Cell::new(0),
            snap_purpose: RefCell::new(None),
            connected: RefCell::new(HashSet::new()),
            lan_warned: RefCell::new(HashSet::new()),
            lan_warning_due: RefCell::new(HashSet::new()),
            trials: RefCell::new(Trials::default()),
            trial_stream: RefCell::new(None),
            carried: RefCell::new(HashMap::new()),
            next_link_id: Cell::new(1),
            playing: RefCell::new(None),
            autoplay: RefCell::new(None),
            last_played: RefCell::new(None),
            settings: RefCell::new(settings),
            audio: Arc::clone(&audio),
            sink_request_tx,
            replay_video: Rc::clone(&replay_video),
            replay_sink_request_tx,
            playback,
            resume_live: RefCell::new(None),
            channel_lists: RefCell::new(HashSet::new()),
        }
    });

    // Recorded footage gets a pipeline only for the Playback page's current
    // session, and only while the page is shown: a late frame of a day,
    // device or page left meanwhile would otherwise show on the new one, or
    // build a pipeline nobody sees. Such a request goes unanswered and its
    // frame is dropped. A codec change (sub to main stream) builds a new
    // pipeline. A sink this system cannot build is answered with why; the
    // page shows it and pauses (see `PlaybackPage::picture_unavailable`).
    let weak = Rc::downgrade(&main);
    glib::spawn_future_local(async move {
        while let Some((session, video_type, reply)) = replay_sink_request_rx.recv().await {
            let Some(m) = weak.upgrade() else { break };
            if m.playback.shows_session(session) {
                let _ = reply.send(m.replay_video.ensure_sink_for(video_type));
            }
        }
    });

    // Why a picture cannot be shown (an element or a decoder this system
    // lacks, see `requirements`): said over the live picture; the Playback
    // page pauses with it.
    let weak = Rc::downgrade(&main);
    main.video.connect_problem(move |text| {
        if let Some(m) = weak.upgrade() {
            m.picture_problem(text);
        }
    });
    let weak = Rc::downgrade(&main);
    main.replay_video.connect_problem(move |text| {
        if let Some(m) = weak.upgrade() {
            m.playback.picture_unavailable(text);
        }
    });

    *main.last_played.borrow_mut() = last.clone();
    if main.settings.borrow().auto_live_view {
        *main.autoplay.borrow_mut() = last;
    }
    main.video.widget().set_keep_aspect_ratio(!main.settings.borrow().stretch);
    main.replay_video.widget().set_keep_aspect_ratio(!main.settings.borrow().stretch);
    main.sidebar.set_trials_shown(recording());
    // The trial entries follow the file actually written: a write error stops it on a device's thread.
    let weak = Rc::downgrade(&main);
    let was_recording = Cell::new(recording());
    glib::timeout_add_local(crate::ui::settings_window::RECORDING_CHECK, move || {
        let Some(main) = weak.upgrade() else { return glib::ControlFlow::Break };
        let now = recording();
        if was_recording.replace(now) != now {
            main.recording_found(now);
        }
        glib::ControlFlow::Continue
    });
    for device in devices {
        main.sidebar.add_device(&device);
        main.devices.borrow_mut().push(device);
    }
    if main.devices.borrow().is_empty() {
        main.show_message("Add a device to start");
    } else {
        main.show_message("Select a device");
    }
    let keys: Vec<String> = main.devices.borrow().iter().map(|d| d.key.clone()).collect();
    for key in keys {
        main.connect(&key, false);
    }

    // SIGINT/SIGTERM bypass the close-request signal; disconnect and quit.
    for signum in [2, 15] {
        let m = Rc::clone(&main);
        let app = app.clone();
        glib::source::unix_signal_add_local(signum, move || {
            m.shutdown_all();
            app.quit();
            glib::ControlFlow::Break
        });
    }

    window.present();
    main.warn_about_requirements();
    main
}

/// Whether the diagnostic log is being written now (the switch on and the file open).
fn recording() -> bool {
    matches!(reoling::diag::recording(), reoling::diag::Recording::On(_))
}

/// Masks the device's name, key (UID or address), user name and channel names in the diagnostic log.
fn conceal_device(device: &Device) {
    for text in [&device.name, &device.key, &device.username] {
        reoling::diag::conceal(text);
    }
    for channel in &device.channels {
        reoling::diag::conceal(&channel.name);
    }
}

/// The first of these icons the theme has (the last if none).
fn icon_of(names: &[&'static str]) -> &'static str {
    let has = |name: &str| {
        gtk4::gdk::Display::default()
            .map(|d| IconTheme::for_display(&d).has_icon(name))
            .unwrap_or(false)
    };
    names.iter().copied().find(|n| has(n)).unwrap_or(names[names.len() - 1])
}

/// A download's file is named after the device and its camera on
/// `channel`: the channel's name, else its number as the live recordings
/// use it (`ch1` for channel 0).
fn download_names(device: Option<&Device>, channel: u8) -> (String, String) {
    let numbered = || format!("ch{}", u16::from(channel) + 1);
    let Some(device) = device else { return ("device".to_string(), numbered()) };
    let camera = device
        .channels
        .iter()
        .find(|c| c.channel_id == channel)
        .map(|c| c.name.trim())
        .filter(|name| !name.is_empty())
        .map_or_else(numbered, str::to_string);
    (device.name.clone(), camera)
}

/// The fullscreen controls' left edge and width over a video `width` wide
/// whose picture shows at `picture` (left edge, width): under the picture and
/// as wide as it, but as wide as the controls ask for (`natural`) where the
/// picture is narrower and the video has room, and never narrower than they
/// need (`minimum`). Centred under the picture, inside the video where they
/// fit, else from its left edge as before. Within 2% of the video's width
/// they take all of it, as before.
fn bar_span(width: i32, picture: (i32, i32), minimum: i32, natural: i32) -> (i32, i32) {
    let (left, shown) = picture;
    let bar = shown.max(natural).min(width).max(minimum);
    // A stream not quite of the screen's shape (896 x 512 on a 16:9 screen
    // shows 1890 of 1920 px): the whole width, not a gap at each side.
    if width.saturating_sub(bar) < width / 50 {
        return (0, bar.max(width));
    }
    let x = left.saturating_add(shown.saturating_sub(bar) / 2).min(width.saturating_sub(bar)).max(0);
    (x, bar)
}

/// In fullscreen the controls lie over the bottom of the video, as wide as
/// the picture and under it rather than across the whole screen: on a screen
/// wider than the picture (an ultrawide monitor, a 4:3 or portrait stream)
/// they stay next to it. Stretched, the picture fills the video and so does
/// the bar, as before. The overlay asks on every allocation, so the bar
/// follows a resize, a new stream or a new picture size (the picture asks for
/// one) and each step of its slide, whose height is measured anew every time.
///
/// A new stream (another channel, stream or Play) clears the picture until
/// its first frame: meanwhile the bar stays where the last picture was rather
/// than spanning the screen for a moment and moving away from the pointer.
/// That place is forgotten when the bar leaves or comes onto the video
/// (fullscreen ends or begins) and when the video changes size; without a
/// picture the bar then spans the video, as before.
fn place_bar_under_picture(overlay: &Overlay, bar: &Revealer, picture: &ZoomPicture) {
    // The video's size and the picture's left edge and width on it.
    let last = Rc::new(Cell::new(None::<((i32, i32), (i32, i32))>));
    let forget = Rc::clone(&last);
    bar.connect_visible_notify(move |_| forget.set(None));
    let bar = bar.downgrade();
    let picture = picture.downgrade();
    overlay.connect_get_child_position(move |overlay, child| {
        let bar = bar.upgrade().filter(|bar| bar.upcast_ref::<gtk4::Widget>() == child)?;
        let size = (overlay.width(), overlay.height());
        let shown = picture
            .upgrade()?
            .picture_bounds()
            .map(|shown| (shown.x(), shown.width()))
            .or_else(|| last.get().filter(|(at, _)| *at == size).map(|(_, shown)| shown));
        last.set(shown.map(|shown| (size, shown)));
        let (minimum, natural, _, _) = bar.measure(Orientation::Horizontal, -1);
        let (x, width) = bar_span(overlay.width(), shown?, minimum, natural);
        // At the bottom with the height the bar asks for, as by default.
        let (minimum, natural, _, _) = bar.measure(Orientation::Vertical, width);
        let height = natural.min(overlay.height()).max(minimum);
        Some(gtk4::gdk::Rectangle::new(x, overlay.height() - height, width, height))
    });
}

/// Whether a popover of a control inside `widget` is on screen.
fn popover_open_below(widget: &impl IsA<gtk4::Widget>) -> bool {
    let mut child = widget.first_child();
    while let Some(c) = child {
        if (c.is::<gtk4::Popover>() && c.is_mapped()) || popover_open_below(&c) {
            return true;
        }
        child = c.next_sibling();
    }
    false
}

fn sidebar_icon() -> &'static str {
    let has = |name: &str| {
        gtk4::gdk::Display::default()
            .map(|d| IconTheme::for_display(&d).has_icon(name))
            .unwrap_or(false)
    };
    if has("sidebar-show-symbolic") {
        "sidebar-show-symbolic"
    } else {
        "view-list-symbolic"
    }
}

impl MainWindow {
    fn show_message(&self, text: &str) {
        self.message.set_text(text);
        self.message.set_visible(true);
    }

    /// The watched stream cannot be shown (see `VideoView::connect_problem`):
    /// why, over the picture. The stream runs on (its sound, a recording),
    /// and the stream selector stays usable for one that can be shown.
    fn picture_problem(&self, text: &str) {
        if self.playing_key().is_some() {
            self.show_message(text);
        }
    }

    /// Says at start what this system lacks (see `requirements`), unless the
    /// user asked not to hear about this very set of missing parts again.
    fn warn_about_requirements(self: &Rc<Self>) {
        let report = crate::ui::requirements::check();
        let Some(key) = report.dismiss_key() else { return };
        if self.settings.borrow().requirements_dismissed.as_deref() == Some(key.as_str()) {
            return;
        }
        let this = Rc::clone(self);
        crate::ui::requirements::show_dialog(self.window.upcast_ref(), report, move || {
            let mut settings = this.settings.borrow_mut();
            settings.requirements_dismissed = Some(key.clone());
            settings.save();
        });
    }

    fn device(&self, key: &str) -> Option<Device> {
        self.devices.borrow().iter().find(|d| d.key == key).cloned()
    }

    fn persist(&self) {
        device_store::save(&self.devices.borrow(), self.last_played.borrow().as_deref());
    }

    fn playing_key(&self) -> Option<String> {
        self.playing.borrow().clone()
    }

    fn set_fullscreen(self: &Rc<Self>, on: bool) {
        if on {
            // First the keyboard focus goes, so that keys right after going
            // fullscreen reach the window rather than the paned: a control
            // that has the focus and moves onto the video, or hides with the
            // header bar or the sidebar, makes GTK hand it to the paned on the
            // next frame (clearing it afterwards does not stop that). Escape
            // does not depend on it: in fullscreen the window takes it before
            // any focused widget.
            gtk4::prelude::GtkWindowExt::set_focus(&self.window, None::<&gtk4::Widget>);
            self.window.fullscreen();
            // The control rows move onto the video, as a bar that slides in.
            self.live.remove(&self.bars);
            self.fullscreen_bar.set_child(Some(&self.bars));
            self.bars.add_css_class("osd");
            self.fullscreen_bar.set_visible(true);
            self.fullscreen_bar.set_reveal_child(true);
            self.show_fullscreen_bar();
        } else {
            self.window.unfullscreen();
            self.fullscreen_bar.set_reveal_child(false);
            self.fullscreen_bar.set_child(None::<&gtk4::Widget>);
            self.fullscreen_bar.set_visible(false);
            self.bars.remove_css_class("osd");
            self.live.append(&self.bars);
            self.bar_generation.set(self.bar_generation.get() + 1);
        }
        self.header.set_visible(!on);
        self.sidebar_toggle.set_visible(!on);
        self.sidebar.widget().set_visible(!on && self.sidebar_toggle.is_active());
        self.fullscreen_button.set_icon_name(if on {
            "view-restore-symbolic"
        } else {
            "view-fullscreen-symbolic"
        });
        self.fullscreen_button.set_tooltip_text(Some(if on { "Normal window" } else { "Fullscreen" }));
    }

    /// Brings the fullscreen controls up and hides them again after a pause
    /// in pointer movement (never while the pointer is on them or one of
    /// their popovers is open).
    fn show_fullscreen_bar(self: &Rc<Self>) {
        if !self.window.is_fullscreen() {
            return;
        }
        self.fullscreen_bar.set_reveal_child(true);
        let generation = self.bar_generation.get() + 1;
        self.bar_generation.set(generation);
        let this = Rc::clone(self);
        glib::timeout_add_local_once(Duration::from_millis(2500), move || {
            if this.bar_generation.get() == generation
                && this.window.is_fullscreen()
                && !this.pointer_on_bar.get()
            {
                // The volume slider or the stream list would close with the
                // bar: wait until it is closed.
                if popover_open_below(&this.bars) {
                    this.show_fullscreen_bar();
                } else {
                    this.fullscreen_bar.set_reveal_child(false);
                }
            }
        });
    }

    /// A short message over the video.
    fn notify(self: &Rc<Self>, text: &str) {
        self.notice.set_text(text);
        self.notice.set_visible(true);
        let generation = self.notice_generation.get() + 1;
        self.notice_generation.set(generation);
        let this = Rc::clone(self);
        glib::timeout_add_local_once(Duration::from_secs(4), move || {
            if this.notice_generation.get() == generation {
                this.notice.set_visible(false);
            }
        });
    }

    /// Where snapshots (Pictures) or recordings (Videos) go — the folder
    /// chosen in the settings, else `~/Pictures/Reoling` / `~/Videos/Reoling`
    /// — created on demand.
    fn media_dir(&self, kind: glib::UserDirectory) -> Option<std::path::PathBuf> {
        let chosen = {
            let settings = self.settings.borrow();
            match kind {
                glib::UserDirectory::Pictures => settings.screenshot_dir.clone(),
                _ => settings.recording_dir.clone(),
            }
        };
        crate::ui::settings::media_dir(chosen.as_deref(), kind)
    }

    /// `<device>-<channel>-<date>-<time>.<extension>` for what is playing.
    fn media_file_name(&self, extension: &str) -> String {
        let device = self.playing_key().and_then(|k| self.device(&k));
        let name = device
            .as_ref()
            .map(|d| format!("{}-ch{}", d.name, u16::from(d.channel) + 1))
            .unwrap_or_else(|| "Reoling".to_string());
        let name: String = name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
            .collect();
        let now = glib::DateTime::now_local()
            .ok()
            .and_then(|t| t.format("%Y%m%d-%H%M%S").ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        format!("{name}-{now}.{extension}")
    }

    /// Preset thumbnails are read one at a time — the camera breaks on
    /// overlapping image-file requests — each next one starting when the
    /// previous answered (or a 6 s watchdog says it never will).
    fn enqueue_preset_images(self: &Rc<Self>, ids: &[u8]) {
        {
            let mut queue = self.preset_queue.borrow_mut();
            for id in ids {
                if !queue.contains(id) {
                    queue.push_back(*id);
                }
            }
        }
        self.pump_preset_images();
    }

    fn pump_preset_images(self: &Rc<Self>) {
        if self.preset_in_flight.get() {
            return;
        }
        let Some(id) = self.preset_queue.borrow_mut().pop_front() else { return };
        if std::env::var("REOLING_DEBUG_PRESETS").is_ok() {
            eprintln!("PRESETS asking for the picture of preset {id}");
        }
        self.preset_in_flight.set(true);
        self.control(move |link, channel| link.query_preset_image(channel, id));
        let generation = self.preset_queue_generation.get() + 1;
        self.preset_queue_generation.set(generation);
        let this = Rc::clone(self);
        glib::timeout_add_local_once(Duration::from_secs(6), move || {
            if this.preset_queue_generation.get() == generation {
                this.preset_in_flight.set(false);
                this.pump_preset_images();
            }
        });
    }

    fn preset_image_done(self: &Rc<Self>) {
        if std::env::var("REOLING_DEBUG_PRESETS").is_ok() {
            eprintln!("PRESETS a picture request finished");
        }
        self.preset_in_flight.set(false);
        self.preset_queue_generation.set(self.preset_queue_generation.get() + 1);
        self.pump_preset_images();
    }

    /// Sends a control command to the channel being watched.
    fn control(&self, send: impl FnOnce(&crate::ui::bridge::DeviceLink, u8)) {
        let Some(key) = self.playing_key() else { return };
        let Some(device) = self.device(&key) else { return };
        if let Some(l) = self.links.borrow().get(&key) {
            send(&l.link, device.channel);
        }
    }

    /// The camera window for the watched channel; `raise` brings it over
    /// the main window (the gear button), while the re-opening for another
    /// channel (see `play`) leaves it where it is.
    fn open_camera_settings(&self, raise: bool) {
        let Some(key) = self.playing_key() else { return };
        let Some(device) = self.device(&key) else { return };
        let channel_name = device
            .channels
            .iter()
            .find(|c| c.channel_id == device.channel)
            .map(|c| c.name.clone())
            .filter(|n| !n.is_empty());
        let title = match (device.multi_channel, channel_name) {
            (true, Some(name)) => format!("{name} — {}", device.name),
            (true, None) => format!("Channel {} — {}", device.channel + 1, device.name),
            (false, _) => device.name.clone(),
        };
        // A single camera is its own model; behind an NVR or Hub the
        // channel's camera says which model it is (asked for on open).
        let model = if device.multi_channel { String::new() } else { device.model.clone() };
        let kind = DeviceKind::from_model(&device.model);
        let abilities = device.abilities.get(&device.channel).copied();
        self.camera_settings.open(&key, &title, device.channel, &model, &device.encode_profiles, kind, abilities);
        if raise {
            self.camera_settings.present();
        }
    }

    /// One tab of the camera window's channel, over the watched device's
    /// link, under the window's number for this read (the page comes back
    /// with it); `false` when nothing is being watched.
    fn read_camera_page(&self, channel: u8, page: CameraPage, read: u64) -> bool {
        let Some(key) = self.playing_key() else { return false };
        let Some(device) = self.device(&key) else { return false };
        let links = self.links.borrow();
        let Some(l) = links.get(&key) else { return false };
        let gates = SettingsGates::new(
            DeviceKind::from_model(&device.model),
            device.abilities.get(&channel).copied(),
            &device.username,
        );
        l.link.query_settings(SettingsTarget::Camera { channel, page }, gates, read);
        true
    }

    /// One group of writes of the camera window's device (`key`) and
    /// channel, over that device's link; `false` unless it is the one being
    /// watched, on that channel. Only a Home Hub's cameras are written
    /// (their shapes are the captured ones).
    fn write_camera_page(
        &self,
        key: &str,
        channel: u8,
        page: CameraPage,
        group: u64,
        writes: Vec<SettingsWrite>,
    ) -> bool {
        if self.playing_key().as_deref() != Some(key) {
            return false;
        }
        let Some(device) = self.device(key) else { return false };
        let kind = DeviceKind::from_model(&device.model);
        if kind != DeviceKind::Hub || device.channel != channel {
            return false;
        }
        let links = self.links.borrow();
        let Some(l) = links.get(key) else { return false };
        l.link.write_settings(SettingsTarget::Camera { channel, page }, kind, group, writes);
        true
    }

    /// One tab of an NVR's or Hub's own settings, on that device's link
    /// (whether or not it is the one being watched); `false` when the device
    /// is not connected. Its reads go unnumbered (0): that window writes
    /// nothing.
    fn read_device_page(&self, key: &str, page: DevicePage) -> bool {
        let Some(device) = self.device(key) else { return false };
        let links = self.links.borrow();
        let Some(l) = links.get(key) else { return false };
        let gates = SettingsGates::new(DeviceKind::from_model(&device.model), None, &device.username);
        l.link.query_settings(SettingsTarget::Device { page }, gates, 0);
        true
    }

    /// "Device settings…" on a sidebar card: the device's own settings
    /// window (an NVR, a Home Hub or a camera on its own).
    fn open_device_settings(self: &Rc<Self>, key: &str) {
        if !self.connected.borrow().contains(key) || !self.links.borrow().contains_key(key) {
            return;
        }
        let Some(device) = self.device(key) else { return };
        // The UID is the app's own knowledge, shown masked; nothing is read.
        let uid_hint = match &device.target {
            ConnectTarget::Uid(uid) => Some(settings_pages::mask_uid(uid)),
            ConnectTarget::Ip { .. } => None,
        };
        self.device_settings.set_channels(&device.channels);
        self.device_settings.open(key, &device.name, &device.model, DeviceKind::from_model(&device.model), uid_hint.as_deref());
    }

    /// Whether the card's menu offers the device's own settings: any
    /// connected device. A camera on its own answers the same device pages
    /// as an NVR or Home Hub, less the tabs `device_settings::has_tab` hides.
    fn refresh_settings_entry(&self, key: &str) {
        let available = self.connected.borrow().contains(key) && self.device(key).is_some();
        self.sidebar.set_settings_available(key, available);
    }

    fn open_remote(&self) {
        if self.remote.is_visible() {
            self.remote.hide();
        } else {
            self.remote.present();
            self.query_zoom();
            self.query_presets();
            self.query_monitor_point();
        }
    }

    /// Zooming makes the camera focus by itself, and a focus change can be
    /// clamped: read both back a moment after a command, and again once the
    /// autofocus has had time to settle.
    fn refresh_zoom_focus_soon(self: &Rc<Self>) {
        for delay in [Duration::from_millis(800), Duration::from_millis(2500)] {
            let this = Rc::clone(self);
            glib::timeout_add_local_once(delay, move || this.query_zoom());
        }
    }

    /// Saves the current position as a new preset with an id the camera does
    /// not already use (1..=63, the range this implementation assumes).
    fn next_preset_id(self: &Rc<Self>) -> Option<u8> {
        let key = self.playing_key()?;
        let used: std::collections::HashSet<u8> = self
            .device(&key)
            .map(|d| d.presets.iter().map(|p| p.id).collect())
            .unwrap_or_default();
        let id = (1..=63u8).find(|id| !used.contains(id));
        if id.is_none() {
            self.notify("No free preset slot (63 max)");
        }
        id
    }

    /// Saves the current position as a new preset. Like the official app,
    /// with the picture the dialog already took (and uploaded), or — if that
    /// one never arrived — a fresh one taken now.
    fn add_preset(self: &Rc<Self>, id: u8, name: String, has_picture: bool) {
        if has_picture {
            self.control(move |link, channel| link.save_preset_with_image(channel, id, name.clone()));
            self.query_presets_soon();
        } else {
            self.begin_snapshot(SnapPurpose::Add(id, name), 0);
        }
    }

    /// Takes a picture of what the camera sees now. Shares the one-at-a-time
    /// slot of the preset-picture queue (`preset_in_flight`): the camera
    /// breaks on overlapping image transfers.
    fn begin_snapshot(self: &Rc<Self>, purpose: SnapPurpose, tries: u32) {
        if self.preset_in_flight.get() {
            if tries < 15 {
                let this = Rc::clone(self);
                glib::timeout_add_local_once(Duration::from_millis(400), move || {
                    this.begin_snapshot(purpose, tries + 1)
                });
            } else {
                self.notify("The camera is busy, try again");
            }
            return;
        }
        let id = match &purpose {
            SnapPurpose::Add(id, _) | SnapPurpose::Edit(id) => *id,
        };
        self.preset_in_flight.set(true);
        *self.snap_purpose.borrow_mut() = Some(purpose);
        let generation = self.preset_queue_generation.get() + 1;
        self.preset_queue_generation.set(generation);
        let this = Rc::clone(self);
        glib::timeout_add_local_once(Duration::from_secs(10), move || {
            if this.preset_queue_generation.get() != generation {
                return;
            }
            this.preset_in_flight.set(false);
            match this.snap_purpose.borrow_mut().take() {
                Some(SnapPurpose::Add(id, name)) => {
                    this.notify("Could not take a picture; saving the preset without one");
                    this.control(move |link, channel| link.set_preset(channel, id, name.clone()));
                    this.query_presets_soon();
                }
                Some(SnapPurpose::Edit(_)) => this.notify("Could not take a picture"),
                None => {}
            }
            this.pump_preset_images();
        });
        self.control(move |link, channel| link.capture_preset_image(channel, id));
    }

    /// The snapshot arrived: shrink it to the size the official app stores
    /// (169×95), upload it, and finish whatever asked for it.
    fn snapshot_ready(self: &Rc<Self>, preset_id: u8, jpeg: &[u8]) {
        let Some(purpose) = self.snap_purpose.borrow_mut().take() else { return };
        self.preset_image_done();
        let small = remote::decode_and_scale(jpeg, 169, 95)
            .and_then(|(pixbuf, _, _)| pixbuf.save_to_bufferv("jpeg", &[("quality", "80")]).ok());
        let Some(small) = small else {
            self.notify("Could not read the camera's picture");
            return;
        };
        {
            let small = small.clone();
            self.control(move |link, channel| link.upload_preset_image(channel, preset_id, small));
        }
        match purpose {
            SnapPurpose::Add(id, name) => {
                self.control(move |link, channel| link.save_preset_with_image(channel, id, name.clone()));
                self.query_presets_soon();
            }
            SnapPurpose::Edit(id) => self.remote.show_new_picture(id, &small),
        }
    }

    fn query_presets(&self) {
        self.control(|link, channel| link.query_presets(channel));
    }

    fn query_presets_soon(self: &Rc<Self>) {
        for delay in [Duration::from_millis(500), Duration::from_millis(1500)] {
            let this = Rc::clone(self);
            glib::timeout_add_local_once(delay, move || this.query_presets());
        }
    }

    /// Asks the watched camera for Monitor Point's state, if it has one.
    fn query_monitor_point(&self) {
        let has_monitor_point = self
            .playing_key()
            .and_then(|k| self.device(&k))
            .and_then(|d| d.abilities.get(&d.channel).copied())
            .is_some_and(|a| a.monitor_point);
        if has_monitor_point {
            self.control(|link, channel| link.query_monitor_point(channel));
        }
    }

    fn query_monitor_point_soon(self: &Rc<Self>) {
        for delay in [Duration::from_millis(500), Duration::from_millis(1500)] {
            let this = Rc::clone(self);
            glib::timeout_add_local_once(delay, move || this.query_monitor_point());
        }
    }

    /// Asks the watched camera where its zoom and focus stand, if it has them.
    fn query_zoom(&self) {
        let has_zoom = self
            .playing_key()
            .and_then(|k| self.device(&k))
            .and_then(|d| d.abilities.get(&d.channel).copied())
            .is_some_and(|a| a.zoom);
        if has_zoom {
            self.control(|link, channel| link.query_zoom_focus(channel));
        }
    }

    fn set_talk_button(&self, on: bool) {
        self.updating_talk.set(true);
        self.talk.set_active(on);
        self.updating_talk.set(false);
    }

    /// The Talk button: opens a session with the camera and, once it accepts,
    /// starts sending the microphone.
    fn toggle_talk(self: &Rc<Self>, on: bool) {
        if on {
            self.talk_pending.set(true);
            self.talk_retried.set(false);
            self.control(|link, channel| link.talk_start(channel));
        } else {
            self.end_talk();
        }
    }

    /// Ends any talk: the microphone stops, the camera is told, the button
    /// pops out.
    fn end_talk(&self) {
        let was = self.talk_pending.replace(false) || self.microphone.borrow_mut().take().is_some();
        self.set_talk_button(false);
        if was {
            self.control(|link, channel| link.talk_stop(channel));
        }
        self.update_controls();
    }

    /// The camera answered the talk configuration.
    fn talk_answered(self: &Rc<Self>, code: u16) {
        if !self.talk_pending.get() {
            return;
        }
        if code == 422 && !self.talk_retried.get() {
            // Another talk is still open on the camera (or one of ours died):
            // close it and ask again, as the official app does.
            self.talk_retried.set(true);
            self.control(|link, channel| link.talk_stop(channel));
            self.control(|link, channel| link.talk_start(channel));
            return;
        }
        if code != 200 {
            self.talk_pending.set(false);
            self.set_talk_button(false);
            self.notify(&format!("Talk: the camera refused (code {code})"));
            return;
        }
        self.talk_pending.set(false);
        let Some(key) = self.playing_key() else { return };
        let Some(device) = self.device(&key) else { return };
        let sink = self.links.borrow().get(&key).map(|l| l.link.talk_sink(device.channel));
        let Some(sink) = sink else { return };
        match Microphone::start(sink) {
            Ok(mic) => {
                *self.microphone.borrow_mut() = Some(mic);
                self.notify("Talking… press the microphone again to stop");
            }
            Err(e) => {
                self.set_talk_button(false);
                self.control(|link, channel| link.talk_stop(channel));
                self.notify(&format!("No microphone: {e}"));
            }
        }
    }

    fn set_spotlight_button(&self, on: bool) {
        self.updating_spotlight.set(true);
        self.spotlight.set_active(on);
        self.updating_spotlight.set(false);
    }

    fn take_snapshot(self: &Rc<Self>) {
        let Some(texture) = self.video.snapshot() else {
            self.notify("Nothing to save yet");
            return;
        };
        let Some(dir) = self.media_dir(glib::UserDirectory::Pictures) else {
            self.notify("Could not create the screenshot folder");
            return;
        };
        let path = dir.join(self.media_file_name("png"));
        match texture.save_to_png(&path) {
            Ok(()) => self.notify(&format!("Snapshot saved to {}", path.display())),
            Err(e) => self.notify(&format!("Could not save the snapshot: {e}")),
        }
    }

    fn toggle_recording(self: &Rc<Self>, on: bool) {
        let Some(sink) = self.video.current_sink() else {
            self.set_record_button(false);
            return;
        };
        if !on {
            if sink.stop_recording() {
                self.notify("Recording saved");
            }
            return;
        }
        let Some(dir) = self.media_dir(glib::UserDirectory::Videos) else {
            self.notify("Could not create the recording folder");
            self.set_record_button(false);
            return;
        };
        let path = dir.join(self.media_file_name("mkv"));
        match sink.start_recording(&path) {
            Ok(()) => self.notify(&format!("Recording to {}", path.display())),
            Err(e) => {
                self.notify(&format!("Could not record: {e}"));
                self.set_record_button(false);
            }
        }
    }

    /// Sets the record button without triggering a recording change.
    fn set_record_button(&self, on: bool) {
        self.updating_record.set(true);
        self.record.set_active(on);
        self.updating_record.set(false);
    }

    /// Clears the picture and ends any recording (the pipeline is going).
    fn reset_video(&self) {
        self.video.reset();
        self.audio.reset();
        self.set_record_button(false);
    }

    /// Lists in the dropdown the streams the watched device/channel offers.
    /// Keeps the preferred stream if it is offered, else falls back to the
    /// lightest one. Returns whether the stream in use changed.
    fn refresh_streams(&self) -> bool {
        let device = self.playing_key().and_then(|k| self.device(&k));
        let accepted = self.trial_stream.borrow().as_ref().and_then(|(key, channel, stream)| {
            device.as_ref().filter(|d| &d.key == key && d.channel == *channel).map(|_| *stream)
        });
        // A device that sent a channel list (an NVR or Hub) names each channel's streams there.
        let abilities = device
            .as_ref()
            .filter(|d| !self.channel_lists.borrow().contains(&d.key))
            .and_then(|d| d.abilities.get(&d.channel).copied());
        let listed = device.and_then(|d| d.channels.into_iter().find(|c| c.channel_id == d.channel)).map(|c| c.streams);
        let offered = offered_streams(advertised_streams(listed, abilities), accepted);
        let before = self.profile.get();
        let chosen = if offered.contains(&before) {
            before
        } else {
            offered.last().copied().unwrap_or(StreamProfile::Sub)
        };
        let label = |p: &StreamProfile| match p {
            StreamProfile::Main => "Main stream",
            StreamProfile::Extern => "Extern stream",
            StreamProfile::Sub => "Sub stream",
        };
        // Touch the dropdown only where it differs: this runs from its own
        // "selected" handler too, and rebuilding a dropdown whose popup is
        // still open from inside that handler freezes GTK.
        let index = offered.iter().position(|p| *p == chosen).unwrap_or(0) as u32;
        if *self.stream_options.borrow() != offered {
            self.refreshing_streams.set(true);
            let names: Vec<&str> = offered.iter().map(label).collect();
            self.stream.set_model(Some(&gtk4::StringList::new(&names)));
            self.stream.set_selected(index);
            self.refreshing_streams.set(false);
        } else if self.stream.selected() != index {
            self.refreshing_streams.set(true);
            self.stream.set_selected(index);
            self.refreshing_streams.set(false);
        }
        *self.stream_options.borrow_mut() = offered;
        self.profile.set(chosen);
        chosen != before
    }

    /// Controls that only make sense while watching, or with a multi-camera
    /// device (NVR / Home Hub), follow the current state.
    fn update_controls(&self) {
        let playing = self.playing_key();
        let multi = playing
            .as_deref()
            .and_then(|k| self.device(k))
            .map(|d| d.multi_channel)
            .unwrap_or(false);
        let stopped = self.stopped.borrow().is_some();
        let (icon, tip) = if playing.is_none() && stopped {
            ("media-playback-start-symbolic", "Play")
        } else {
            ("media-playback-stop-symbolic", "Stop")
        };
        self.stop.set_icon_name(icon);
        self.stop.set_tooltip_text(Some(tip));
        self.stop.set_sensitive(playing.is_some() || stopped);
        self.snapshot.set_sensitive(self.streaming.get());
        // What this channel's camera can do; until the device has said, assume
        // it can (a wrong guess is answered with a refusal).
        let abilities = playing
            .as_deref()
            .and_then(|k| self.device(k))
            .and_then(|d| d.abilities.get(&d.channel).copied());
        let (siren, spotlight) = abilities.map_or((true, true), |a| (a.siren, a.spotlight));
        let has_ptz = abilities.is_some_and(|a| a.ptz());
        let can_talk = abilities.is_some_and(|a| a.talk);
        if !self.talk.is_active() {
            self.talk.set_sensitive(self.streaming.get() && can_talk);
        }
        self.talk.set_tooltip_text(Some(if can_talk { "Talk" } else { "This camera has no two-way audio" }));
        self.ptz.set_sensitive(self.streaming.get() && has_ptz);
        self.camera_settings_button.set_sensitive(self.streaming.get());
        let (move_ok, zoom_ok) = abilities.map_or((false, false), |a| (a.pan || a.tilt, a.zoom));
        let (calibration_ok, monitor_point_ok) = abilities.map_or((false, false), |a| (a.calibration, a.monitor_point));
        let streaming = self.streaming.get();
        self.remote.set_enabled(
            streaming && move_ok,
            streaming && zoom_ok,
            streaming && zoom_ok,
            streaming && calibration_ok,
            streaming && monitor_point_ok,
        );
        if let Some(d) = playing.as_deref().and_then(|k| self.device(k)) {
            let camera = d.channels.iter().find(|c| c.channel_id == d.channel).map(|c| c.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| format!("Channel {}", u16::from(d.channel) + 1));
            self.remote.set_target(&format!("{} — {camera}", d.name));
        }
        self.ptz.set_tooltip_text(Some(if has_ptz { "Camera control" } else { "This camera cannot move" }));
        self.siren.set_sensitive(self.streaming.get() && siren);
        self.spotlight.set_sensitive(self.streaming.get() && spotlight);
        self.siren.set_tooltip_text(Some(if siren { "Sound the siren" } else { "This camera has no siren" }));
        self.spotlight
            .set_tooltip_text(Some(if spotlight { "Spotlight" } else { "This camera has no spotlight" }));
        self.record.set_sensitive(self.streaming.get());
        self.stream.set_sensitive(stream_selectable(playing.is_some(), self.streaming.get(), self.refused.get()));
        self.previous.set_sensitive(multi);
        self.next.set_sensitive(multi);
    }

    fn settings_dialog(self: &Rc<Self>) {
        let this = Rc::clone(self);
        let current = self.settings.borrow().clone();
        crate::ui::settings_window::open(self.window.upcast_ref(), &current, move |mut new| {
            // The volume slider lives in the main window; keep its value.
            new.volume = this.settings.borrow().volume;
            this.video.widget().set_keep_aspect_ratio(!new.stretch);
            this.replay_video.widget().set_keep_aspect_ratio(!new.stretch);
            let decoding_changed = {
                let old = this.settings.borrow();
                old.decoding != new.decoding
                    || old.hardware_decoder != new.hardware_decoder
                    || old.latency != new.latency
            };
            this.diagnostics_set(new.diagnostics, || {
                new.apply();
                new.save();
            });
            *this.settings.borrow_mut() = new;
            // A running stream keeps its decoder; restart it to use the new one.
            if decoding_changed {
                if let Some(key) = this.playing_key() {
                    this.play(&key);
                }
            }
        });
    }

    fn add_device_dialog(self: &Rc<Self>) {
        let this = Rc::clone(self);
        let added: Vec<String> = self
            .devices
            .borrow()
            .iter()
            .filter_map(|d| match &d.target {
                ConnectTarget::Uid(uid) => Some(uid.clone()),
                ConnectTarget::Ip { .. } => None,
            })
            .collect();
        dialogs::add_device(self.window.upcast_ref(), added, move |target| {
            let device = Device::new(target);
            conceal_device(&device);
            this.sidebar.add_device(&device);
            this.devices.borrow_mut().push(device.clone());
            this.persist();
            this.activate(&device.key);
        });
    }

    fn ask_login(self: &Rc<Self>, device: Device) {
        let this = Rc::clone(self);
        let key = device.key.clone();
        // A typed login that has not logged in yet is what the user last typed.
        let username = self.typed_logins.borrow().get(&key).map_or(device.username.clone(), |t| t.username.clone());
        dialogs::login(
            self.window.upcast_ref(),
            &device.name,
            &username,
            move |username, password, keep| {
                // The saved user name and password stay until the typed ones
                // have logged in.
                this.passwords.borrow_mut().insert(key.clone(), password);
                let remember = if keep { Remember::Store } else { Remember::Forget };
                this.typed_logins.borrow_mut().insert(key.clone(), TypedLogin { username, remember });
                this.cancel_trial(&key);
                this.links.borrow_mut().remove(&key);
                this.connected.borrow_mut().remove(&key);
                this.channel_lists.borrow_mut().remove(&key);
                // The old connection goes without a word; the Playback page
                // waits for the new one, the camera window gives up a save
                // it had out on the old one.
                this.playback.device_reconnecting(&key);
                this.camera_settings.reconnecting(&key);
                *this.autoplay.borrow_mut() = Some(key.clone());
                this.connect(&key, true);
            },
        );
    }

    /// "Try other live view requests…": only while the diagnostic log is on, for the watched device after it
    /// refused live view, and one trial per device at a time.
    fn start_trial(self: &Rc<Self>, key: &str) {
        let watching = self.playing_key().as_deref() == Some(key);
        if !watching || !self.links.borrow().contains_key(key) {
            return;
        }
        let Some((channel, refused, gen)) = self.trials.borrow_mut().start(key, recording()) else { return };
        if let Some(l) = self.links.borrow().get(key) {
            l.link.try_live(channel, refused, gen);
        }
        self.refresh_trial_entries();
        self.show_message("Trying other live view requests…");
    }

    fn trial_cancelled(&self, key: &str) {
        // Written here, at once: the link's own lines may come later, after the log was turned off.
        self.trial_line(key, "TRIAL ended: cancelled");
        self.refresh_trial_entries();
    }

    /// A trial line the window writes for `key`, under the device's place in the list.
    fn trial_line(&self, key: &str, text: &str) {
        let place = self.devices.borrow().iter().position(|d| d.key == key).map_or(0, |at| at + 1);
        reoling::diag::masked(format_args!("[dev {place}] {text}"));
    }

    /// Each device menu's trial entry, as `Trials::offer` says.
    fn refresh_trial_entries(&self) {
        let recording = recording();
        let trials = self.trials.borrow();
        for device in self.devices.borrow().iter() {
            self.sidebar.set_trial_available(&device.key, trials.offer(&device.key, recording));
        }
    }

    fn relogin(self: &Rc<Self>, key: &str) {
        if let Some(device) = self.device(key) {
            self.ask_login(device);
        }
    }

    /// Opens the device's connection (login, name, channels) without
    /// streaming anything. With `interactive`, asks for the login when no
    /// password is known.
    fn connect(self: &Rc<Self>, key: &str, interactive: bool) {
        if self.links.borrow().contains_key(key) {
            return;
        }
        let Some(device) = self.device(key) else { return };
        self.sidebar.set_status(key, &Status::Connecting);
        let this = Rc::clone(self);
        let key = key.to_string();
        glib::spawn_future_local(async move {
            let cached = this.passwords.borrow().get(&key).cloned();
            let password = match cached {
                Some(p) => Some(p),
                None => secrets::lookup(key.clone()).await,
            };
            let Some(password) = password else {
                this.sidebar.set_status(&key, &Status::LoginNeeded("No saved password".into()));
                if interactive {
                    this.ask_login(device);
                }
                return;
            };
            if this.links.borrow().contains_key(&key) || this.device(&key).is_none() {
                return; // connected or removed while the keyring answered
            }
            this.passwords.borrow_mut().insert(key.clone(), password.clone());
            // A typed login goes with its own user name, the saved one with the device's.
            let typed_username = this.typed_logins.borrow().get(&key).map(|t| t.username.clone());
            let username = typed_username.unwrap_or_else(|| device.username.clone());
            reoling::diag::conceal(&username);
            // The device's place in the list names it in the diagnostic log.
            let place = this.devices.borrow().iter().position(|d| d.key == key).map_or(0, |at| at as u32 + 1);
            let legacy_cipher = this.trials.borrow_mut().arm(&key);
            let trial_epoch = this.trials.borrow_mut().epoch(&key);
            let carried = this.wired(|w| w.take_carried(&key));
            let network = reoling::transport::lan_search::current_network();
            let saved = device.saved_address_for(network);
            if device.lan_ip.is_some() && saved.is_none() {
                let unused = "NEGOTIATION device: the saved address is not used on this network";
                reoling::diag::masked(format_args!("{unused}"));
            }
            let link = spawn_device(
                device.target.clone(),
                username,
                password,
                saved,
                device.lan_rejected,
                device.learns_address_on(network),
                this.sink_request_tx.clone(),
                this.replay_sink_request_tx.clone(),
                Arc::clone(&this.audio),
                place,
                legacy_cipher,
                trial_epoch,
                carried,
            );
            let events = link.events.clone();
            let id = this.next_link_id.get();
            this.next_link_id.set(id + 1);
            this.links.borrow_mut().insert(key.clone(), Link { id, link });
            while let Ok(event) = events.recv().await {
                if this.links.borrow().get(&key).map(|l| l.id) != Some(id) {
                    break; // replaced or removed
                }
                this.on_event(&key, event);
            }
        });
    }

    fn update_keyring(&self, key: &str, step: KeyringStep) {
        match step {
            KeyringStep::Keep => {}
            KeyringStep::Store => {
                let password = self.passwords.borrow().get(key).cloned();
                let name = self.device(key).map(|d| d.name).unwrap_or_default();
                if let Some(password) = password {
                    glib::spawn_future_local(secrets::store(key.to_string(), name, password));
                }
            }
            KeyringStep::Forget => {
                glib::spawn_future_local(secrets::forget(key.to_string()));
            }
        }
    }

    fn on_event(self: &Rc<Self>, key: &str, event: DeviceEvent) {
        match event {
            DeviceEvent::Connected(identity) => {
                // A typed user name that has logged in is the device's now;
                // apply_identity below writes the device list to disk.
                let typed = self.typed_logins.borrow_mut().remove(key);
                if let Some(t) = &typed {
                    if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
                        d.username = t.username.clone();
                    }
                }
                self.connected.borrow_mut().insert(key.to_string());
                self.sidebar.set_status(key, &Status::Connected);
                self.apply_identity(key, &identity);
                if self.device_settings.is_open_for(key) {
                    self.device_settings.reconnected();
                }
                self.update_keyring(key, keyring_step(true, typed.map(|t| t.remember)));
                self.playback.set_readiness(key, self.playback_readiness(key));
                // Connected again for a trial's legacy cipher step: its last forms go out now, if the user still
                // watches that channel.
                let watched = self.playing_key().as_deref() == Some(key);
                let watching = self.device(key).filter(|_| watched).map(|d| d.channel);
                let after = self.trials.borrow_mut().on_connected(key, watching);
                match after {
                    AfterLogin::Continue { channel, refused, gen } => {
                        if let Some(l) = self.links.borrow().get(key) {
                            l.link.try_live_after_cipher(channel, refused, gen);
                        }
                    }
                    AfterLogin::Cancelled => {
                        wake(self.links.borrow().get(key).map(|l| &l.link), Phase::Armed);
                        self.trial_cancelled(key);
                    }
                    AfterLogin::Nothing => {}
                }
                if self.autoplay.borrow().as_deref() == Some(key) {
                    self.autoplay.borrow_mut().take();
                    self.play(key);
                }
            }
            DeviceEvent::Channels(channels) => {
                for channel in &channels {
                    reoling::diag::conceal(&channel.name);
                }
                self.channel_lists.borrow_mut().insert(key.to_string());
                self.apply_channels(key, channels);
                self.playback.set_readiness(key, self.playback_readiness(key));
                if self.playing_key().as_deref() == Some(key) && self.refresh_streams() {
                    self.play(key); // the stream we started is not on offer
                }
            }
            DeviceEvent::ChannelName { channel_id, name } => {
                if name.is_empty() {
                    return;
                }
                reoling::diag::conceal(&name);
                if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
                    if let Some(c) = d.channels.iter_mut().find(|c| c.channel_id == channel_id) {
                        c.name = name;
                    }
                }
                if let Some(d) = self.device(key) {
                    self.sidebar.set_channels(key, &d.channels, d.channel);
                    if self.device_settings.is_open_for(key) {
                        self.device_settings.set_channels(&d.channels);
                    }
                }
            }
            DeviceEvent::ControlReply { msg_id: 57, code } => {
                self.camera_settings.saved(code);
                if code == 200 && self.playing_key().as_deref() == Some(key) {
                    // The camera restarts its encoder with the new settings
                    // (resolution or codec may have changed): rebuild the
                    // picture from a fresh stream once it has.
                    let weak = Rc::downgrade(self);
                    let key = key.to_string();
                    glib::timeout_add_local_once(Duration::from_secs(2), move || {
                        if let Some(m) = weak.upgrade() {
                            if m.playing_key().as_deref() == Some(key.as_str()) {
                                m.play(&key);
                            }
                        }
                    });
                }
            }
            DeviceEvent::EncodeProfiles(profiles) => {
                if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
                    d.encode_profiles = profiles;
                }
            }
            DeviceEvent::Encoding { channel_id, encoding } => {
                if self.playing_key().as_deref() == Some(key) && self.camera_settings.is_open_for(channel_id) {
                    self.camera_settings.set_encoding(encoding);
                }
            }
            DeviceEvent::ChannelEncodeProfiles { channel_id, profiles } => {
                if self.playing_key().as_deref() == Some(key) && self.camera_settings.is_open_for(channel_id) && !profiles.is_empty() {
                    self.camera_settings.set_channel_profiles(profiles);
                }
            }
            DeviceEvent::ChannelVersion { channel_id, model, firmware } => {
                if self.playing_key().as_deref() == Some(key) && self.camera_settings.is_open_for(channel_id) {
                    if let Some(model) = model.filter(|m| !m.trim().is_empty()) {
                        self.camera_settings.set_model(model.trim(), firmware.as_deref());
                    }
                }
            }
            DeviceEvent::ControlReply { msg_id, code } => {
                let ok = code == 200;
                match msg_id {
                    201 => {
                        self.talk_answered(code);
                    }
                    18 | 295 => {
                        if !ok {
                            self.notify(&format!("Move: the camera refused (code {code})"));
                        }
                    }
                    263 => {
                        if ok {
                            self.notify("Siren sounding");
                        } else {
                            self.notify(&format!("Siren: the camera refused (code {code})"));
                        }
                    }
                    288 => {
                        if ok {
                            self.notify("Spotlight command sent");
                        } else {
                            self.notify(&format!("Spotlight: the camera refused (code {code})"));
                            // It did not switch; show the truth.
                            let on = self.spotlight.is_active();
                            self.set_spotlight_button(!on);
                        }
                    }
                    341 => {
                        // This reply is the only signal the wire protocol
                        // gives at all — whether it means "request queued"
                        // or "physically finished" is not confirmed either
                        // way, so the busy state clears on it regardless.
                        self.remote.set_calibrating(false);
                        if ok {
                            self.notify("Calibration finished");
                        } else {
                            self.notify(&format!("Calibration: the camera refused (code {code})"));
                        }
                    }
                    // Sent for three different actions (a plain config
                    // change, "Reset", "Return") — only a failure is worth a
                    // message; a config change alone would spam one on every
                    // settled slider drag otherwise.
                    331 if !ok => {
                        self.notify(&format!("Monitor Point: the camera refused (code {code})"));
                    }
                    _ => {}
                }
            }
            DeviceEvent::Abilities(abilities) => {
                if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
                    d.abilities.extend(abilities);
                }
                // A standalone camera's support table also tells of its third stream.
                if self.playing_key().as_deref() == Some(key) && self.refresh_streams() {
                    self.play(key); // the stream we started is not on offer
                }
                self.update_controls();
                // The camera window's gated rows follow the abilities.
                if self.playing_key().as_deref() == Some(key) {
                    if let Some(d) = self.device(key) {
                        if let Some(a) = d.abilities.get(&d.channel) {
                            self.camera_settings.set_abilities(d.channel, *a);
                        }
                    }
                }
            }
            DeviceEvent::ZoomFocus { channel_id, zoom, focus } => {
                let watching = self.playing_key().and_then(|k| self.device(&k)).is_some_and(|d| d.channel == channel_id);
                if watching {
                    if let Some((min, max, current)) = zoom {
                        self.remote.set_zoom_range(min, max, current);
                    }
                    if let Some((min, max, current)) = focus {
                        self.remote.set_focus_range(min, max, current);
                    }
                }
            }
            DeviceEvent::Presets { channel_id, presets } => {
                if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
                    if d.channel == channel_id {
                        d.presets = presets.clone();
                    }
                }
                let watching = self.playing_key().and_then(|k| self.device(&k)).is_some_and(|d| d.channel == channel_id);
                if watching {
                    self.remote.set_presets(&presets);
                }
            }
            DeviceEvent::MonitorPoint { channel_id, state } => {
                let watching = self.playing_key().and_then(|k| self.device(&k)).is_some_and(|d| d.channel == channel_id);
                if watching {
                    self.remote.set_monitor_point(state);
                    if state.valid {
                        self.control(|link, channel| link.query_monitor_point_image(channel));
                    } else {
                        self.remote.clear_monitor_point_image();
                    }
                }
            }
            DeviceEvent::MonitorPointImage { channel_id, jpeg } => {
                let watching = self.playing_key().and_then(|k| self.device(&k)).is_some_and(|d| d.channel == channel_id);
                if watching {
                    self.remote.set_monitor_point_image(&jpeg);
                }
            }
            DeviceEvent::PresetImage { channel_id, preset_id, jpeg } => {
                let watching = self.playing_key().and_then(|k| self.device(&k)).is_some_and(|d| d.channel == channel_id);
                if watching {
                    self.remote.set_preset_image(preset_id, &jpeg);
                }
                self.preset_image_done();
            }
            DeviceEvent::Snapshot { preset_id, jpeg } => {
                self.snapshot_ready(preset_id, &jpeg);
            }
            DeviceEvent::ImageNotFound { preset_id } => {
                if preset_id.is_some() {
                    self.preset_image_done();
                }
            }
            DeviceEvent::ControlFailed(reason) => {
                self.notify(&format!("Command not sent: {reason}"));
                self.camera_settings.failed(&format!("Not sent: {reason}"));
            }
            DeviceEvent::Playing => {
                if self.playing_key().as_deref() == Some(key) {
                    // A stream no decoder takes keeps saying so (see
                    // `picture_problem`): the watch on its bus and this event
                    // come by different ways, in either order.
                    match self.video.problem() {
                        Some(text) => self.show_message(&text),
                        None => self.message.set_visible(false),
                    }
                    self.streaming.set(true);
                    self.refused.set(false);
                    self.trials.borrow_mut().end(key);
                    self.trials.borrow_mut().clear_refusal(key);
                    self.refresh_trial_entries();
                    self.update_controls();
                    self.query_zoom();
                    self.query_monitor_point();
                }
            }
            DeviceEvent::PlayFailed { channel, reason, refused } => {
                // A trial ends with a stream, or with this.
                self.trials.borrow_mut().end(key);
                if self.playing_key().as_deref() == Some(key) {
                    self.streaming.set(false);
                    let watched = self.device(key).is_some_and(|d| d.channel == channel);
                    if watched {
                        self.refused.set(true);
                    }
                    // Kept with the log off too: turning it on afterwards offers the trial (see `settings_dialog`).
                    if refused && watched {
                        self.trials.borrow_mut().refused(key, channel, self.profile.get());
                    } else {
                        self.trials.borrow_mut().clear_refusal(key);
                    }
                    self.refresh_trial_entries();
                    if self.trials.borrow().offer(key, recording()) {
                        self.show_message(&format!("{reason}\n{TRIAL_HINT}"));
                    } else {
                        self.show_message(&reason);
                    }
                    self.update_controls();
                }
            }
            DeviceEvent::TrialReconnect { legacy, message, carried } => {
                self.trial_reconnect(key, legacy, message, carried);
            }
            DeviceEvent::TrialAccepted { channel, stream } => {
                // The selector shows the stream that plays.
                let watched = self.device(key).is_some_and(|d| d.channel == channel);
                if self.playing_key().as_deref() == Some(key) && watched {
                    *self.trial_stream.borrow_mut() = Some((key.to_string(), channel, stream));
                    self.profile.set(stream);
                    self.refresh_streams();
                }
            }
            DeviceEvent::PlayDeferred(reason) => {
                if self.playing_key().as_deref() == Some(key) {
                    self.streaming.set(false);
                    self.show_message(&reason);
                    self.update_controls();
                }
            }
            DeviceEvent::LoginRejected => {
                // A trial's connection made again and refused: the trial is over, the next connection is normal.
                self.trials.borrow_mut().end(key);
                self.links.borrow_mut().remove(key);
                self.connected.borrow_mut().remove(key);
                self.channel_lists.borrow_mut().remove(key);
                self.passwords.borrow_mut().remove(key);
                let typed = self.typed_logins.borrow_mut().remove(key);
                self.update_keyring(key, keyring_step(false, typed.map(|t| t.remember)));
                self.sidebar.set_status(key, &Status::LoginNeeded("Incorrect password".into()));
                self.lost_playing(key, "Incorrect password");
                self.playback.device_lost(key, "Incorrect password");
                self.refresh_settings_entry(key);
                if self.device_settings.is_open_for(key) {
                    self.device_settings.lost("Incorrect password");
                }
            }
            DeviceEvent::LanAddress { ip, network, confirmed } => {
                // Dropped when the device was removed meanwhile.
                if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
                    d.learn_address(ip, network, confirmed);
                }
                self.persist();
            }
            // Once per device and run; shown when the lost connection has been put away (below).
            DeviceEvent::LanAnswerHeld => {
                if self.lan_warned.borrow_mut().insert(key.to_string()) {
                    self.lan_warning_due.borrow_mut().insert(key.to_string());
                }
            }
            DeviceEvent::Lost(reason) => {
                // A trial on this link (one whose last forms were queued too) ends here; its link said nothing.
                if self.trials.borrow_mut().end(key) == Some(Phase::OnLink) {
                    self.trial_line(key, "TRIAL ended: the connection was lost");
                }
                self.links.borrow_mut().remove(key);
                self.connected.borrow_mut().remove(key);
                self.channel_lists.borrow_mut().remove(key);
                self.sidebar.set_status(key, &Status::Failed(reason.clone()));
                self.lost_playing(key, &reason);
                self.playback.device_lost(key, &reason);
                self.refresh_settings_entry(key);
                if self.device_settings.is_open_for(key) {
                    self.device_settings.lost(&reason);
                }
                if self.lan_warning_due.borrow_mut().remove(key) {
                    self.warn_lan_answer_held(key);
                }
            }
            DeviceEvent::Playback(event) => self.playback.on_event(key, event),
            DeviceEvent::Download(event) => self.playback.on_download(key, event),
            // The settings pages: a camera tab goes to the camera window when
            // it shows that channel of the watched device or, once closed,
            // while that tab's save or re-read is under way (the read a save
            // makes before each row's write, or the one after a save, may come
            // then; see `takes`) and the window is still that device's, a
            // device tab to the device window when it shows this device.
            DeviceEvent::Settings { page, read } => match page.target() {
                target @ SettingsTarget::Camera { .. } => {
                    if self.playing_key().as_deref() == Some(key) && self.camera_settings.takes(key, target) {
                        self.camera_settings.set_page(&page, read);
                    }
                }
                SettingsTarget::Device { .. } => {
                    if self.device_settings.is_open_for(key) {
                        self.device_settings.set_page(&page);
                    }
                }
            },
            // A save goes on while the camera window is closed: after a row's
            // 200, the read for the next row and that row's write still go
            // out while the live video plays that channel (the window drops
            // the save when it is opened anew; on another channel of the
            // device the read goes out but not the write, see
            // `write_camera_page`). Stop, the Playback page and a lost
            // connection give it up (see `stopped`, `suspended` and
            // `lost_playing`): its answers would no longer come here.
            DeviceEvent::SettingsWritten { target, group, results } => {
                if let SettingsTarget::Camera { channel, .. } = target {
                    if self.playing_key().as_deref() == Some(key) && self.camera_settings.is_for(key, channel) {
                        self.camera_settings.written(target, group, &results);
                    }
                }
            }
            DeviceEvent::SettingsDeferred(target) => match target {
                SettingsTarget::Camera { .. } => {
                    if self.playing_key().as_deref() == Some(key) && self.camera_settings.takes(key, target) {
                        self.camera_settings.page_deferred(&target);
                    }
                }
                SettingsTarget::Device { .. } => {
                    if self.device_settings.is_open_for(key) {
                        self.device_settings.page_deferred(&target);
                    }
                }
            },
        }
    }

    /// Whether the Playback page can ask device `key` for recordings: logged
    /// in, and for an NVR or Hub, its channel list in on this connection.
    fn playback_readiness(&self, key: &str) -> Readiness {
        if !self.connected.borrow().contains(key) {
            return Readiness::Connecting;
        }
        let multi = self.device(key).is_some_and(|d| d.multi_channel);
        if multi && !self.channel_lists.borrow().contains(key) {
            return Readiness::Channels;
        }
        Readiness::Ready
    }

    fn enter_playback(self: &Rc<Self>) {
        self.suspend_live();
        let key = [
            self.resume_live.borrow().clone(),
            self.autoplay.borrow().clone(),
            self.stopped.borrow().clone(),
            self.last_played.borrow().clone(),
        ]
        .into_iter()
        .flatten()
        .find(|k| self.device(k).is_some());
        let Some(key) = key else {
            self.playback.enter(None, Readiness::Connecting);
            return;
        };
        let Some(device) = self.device(&key) else { return };
        self.sidebar.select(&key);
        self.sidebar.mark_channel(&key, device.channel);
        self.playback.enter(Some((&key, device.channel)), self.playback_readiness(&key));
        if !self.connected.borrow().contains(&key) {
            self.connect(&key, true);
        }
    }

    fn leave_playback(self: &Rc<Self>) {
        // First: clears the page's `shown`, which `play` checks.
        self.playback.leave();
        let Some(key) = self.resume_live.borrow_mut().take() else { return };
        if self.connected.borrow().contains(&key) {
            self.play(&key);
        } else if self.links.borrow().contains_key(&key) {
            // Still logging in: live starts once it has.
            self.show_message("Starting…");
            *self.autoplay.borrow_mut() = Some(key);
        } else {
            // Gone while the page was shown; a click on it connects again,
            // as after any lost connection.
            self.show_message("Select a device");
        }
    }

    fn lost_playing(&self, key: &str, reason: &str) {
        if self.stopped.borrow().as_deref() == Some(key) {
            *self.stopped.borrow_mut() = None;
            self.update_controls();
        }
        if self.playing_key().as_deref() == Some(key) {
            *self.playing.borrow_mut() = None;
            self.streaming.set(false);
            self.reset_video();
            self.show_message(reason);
            self.update_controls();
            if self.camera_settings.is_visible() {
                self.camera_settings.lost(reason);
            } else {
                self.camera_settings.lost_while_closed(key);
            }
        }
    }

    /// Takes the device's own name and kind once it has told us.
    fn apply_identity(&self, key: &str, identity: &reoling::DeviceIdentity) {
        let text = |s: &Option<String>| s.clone().unwrap_or_default().trim().to_string();
        let (name, model) = (text(&identity.name), text(&identity.model));
        reoling::diag::conceal(&name);
        let multi = looks_multi_channel(&name, &model);
        if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
            if !model.is_empty() {
                d.model = model.clone();
            }
            if !name.is_empty() {
                d.name = name.clone();
            }
            if !name.is_empty() || !model.is_empty() {
                d.multi_channel = multi;
            }
        }
        if !name.is_empty() {
            self.sidebar.set_name(key, &name);
        }
        self.persist();
        self.update_controls();
        self.refresh_settings_entry(key);
    }

    /// Takes the channel list the device pushed. Several channels means an
    /// NVR / Home Hub whatever its model string says.
    fn apply_channels(self: &Rc<Self>, key: &str, channels: Vec<reoling::ChannelInfo>) {
        if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
            if channels.len() > 1 {
                d.multi_channel = true;
            }
            // The device re-sends its list from time to time, without the names
            // we had to ask for separately; keep those.
            let mut channels = channels;
            for channel in channels.iter_mut().filter(|c| c.name.is_empty()) {
                if let Some(old) = d.channels.iter().find(|o| o.channel_id == channel.channel_id) {
                    channel.name = old.name.clone();
                }
            }
            d.channels = channels;
        }
        if let Some(d) = self.device(key) {
            self.sidebar.set_channels(key, &d.channels, d.channel);
            if self.device_settings.is_open_for(key) {
                self.device_settings.set_channels(&d.channels);
            }
        }
        self.persist();
        self.update_controls();
        self.refresh_settings_entry(key);
    }

    /// A click on a device's card: watch it (connecting first if need be).
    /// While the Playback page is shown, play back its recordings instead.
    fn activate(self: &Rc<Self>, key: &str) {
        let Some(device) = self.device(key) else { return };
        if self.playback.is_shown() {
            self.sidebar.select(key);
            self.sidebar.mark_channel(key, device.channel);
            *self.resume_live.borrow_mut() = Some(key.to_string());
            self.playback.select(key, device.channel, self.playback_readiness(key));
            if !self.connected.borrow().contains(key) {
                self.connect(key, true);
            }
            return;
        }
        if self.connected.borrow().contains(key) {
            self.play(key);
        } else {
            *self.autoplay.borrow_mut() = Some(key.to_string());
            self.connect(key, true);
        }
    }

    /// A click on one of a device's channels.
    fn pick_channel(self: &Rc<Self>, key: &str, channel: u8) {
        if let Some(d) = self.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
            d.channel = channel;
        }
        self.persist();
        self.activate(key);
    }


    /// The button beside the picture: Stop while streaming, Play after.
    fn stop_or_play(self: &Rc<Self>) {
        if self.playing_key().is_some() {
            self.stop();
            return;
        }
        // Copied out first: `play` changes `stopped`, so a borrow must not
        // still be held when it runs.
        let stopped = self.stopped.borrow().clone();
        if let Some(key) = stopped {
            self.play(&key);
        }
    }

    fn step_channel(self: &Rc<Self>, delta: i32) {
        let Some(key) = self.playing_key() else { return };
        let Some(device) = self.device(&key) else { return };
        let ids: Vec<u8> = device.channels.iter().filter(|c| c.online).map(|c| c.channel_id).collect();
        let Some(index) = ids.iter().position(|c| *c == device.channel) else { return };
        let next = (index as i32 + delta).rem_euclid(ids.len() as i32) as usize;
        self.pick_channel(&key, ids[next]);
    }

    /// The warning for a device that could not be reached while a host answered for it on the local network from
    /// another address than the saved one. "Continue" forgets what was saved and connects again, by the rules
    /// of a first connection; "Disconnect" leaves the device as it is, not connected.
    fn warn_lan_answer_held(self: &Rc<Self>, key: &str) {
        let name = self.device(key).map(|d| d.name).unwrap_or_default();
        let (this, key) = (Rc::clone(self), key.to_string());
        dialogs::warn_lan_answer_held(self.window.upcast_ref(), &name, move || {
            if let Some(d) = this.devices.borrow_mut().iter_mut().find(|d| d.key == key) {
                d.forget_address();
            }
            this.persist();
            this.connect(&key, false);
        });
    }

    /// "Remove device" in a card's menu: asks first, since the device and its saved password go at once.
    fn ask_remove(self: &Rc<Self>, key: &str) {
        let name = self.device(key).map(|d| d.name).unwrap_or_default();
        let (this, key) = (Rc::clone(self), key.to_string());
        dialogs::confirm_remove(self.window.upcast_ref(), &name, move || this.remove(&key));
    }

    fn remove(self: &Rc<Self>, key: &str) {
        self.cancel_trial(key);
        self.trials.borrow_mut().clear_refusal(key);
        self.carried.borrow_mut().remove(key);
        if self.playing_key().as_deref() == Some(key) {
            *self.playing.borrow_mut() = None;
            self.streaming.set(false);
            self.reset_video();
            self.show_message("Select a device");
            self.update_controls();
        }
        if self.last_played.borrow().as_deref() == Some(key) {
            *self.last_played.borrow_mut() = None;
        }
        if self.stopped.borrow().as_deref() == Some(key) {
            *self.stopped.borrow_mut() = None;
        }
        if self.resume_live.borrow().as_deref() == Some(key) {
            *self.resume_live.borrow_mut() = None;
        }
        self.playback.device_removed(key);
        if self.device_settings.is_open_for(key) {
            self.device_settings.lost("the device was removed");
        }
        self.camera_settings.removed(key);
        self.links.borrow_mut().remove(key); // dropping the link disconnects
        self.connected.borrow_mut().remove(key);
        self.channel_lists.borrow_mut().remove(key);
        self.sidebar.remove_device(key);
        self.devices.borrow_mut().retain(|d| d.key != key);
        self.persist();
        self.passwords.borrow_mut().remove(key);
        self.typed_logins.borrow_mut().remove(key);
        glib::spawn_future_local(secrets::forget(key.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::{bar_span, keyring_step, place_bar_under_picture, stream_selectable, KeyringStep, Remember};
    use super::{end_trial, wire, AfterLogin, Link, Paths, Phase, Trials, Wiring};
    use crate::ui::bridge::tests::{listened, Bench, Commands};
    use crate::ui::bridge::{Carried, DeviceEvent};
    use reoling::protocol::bc::model::{MSG_ID_LOGOUT, MSG_ID_VIDEO};
    use reoling::StreamProfile;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::time::Duration;

    /// A refusal while the log is off offers a trial once it is written; one trial per device.
    #[test]
    fn a_refusal_offers_a_trial_once_the_log_is_written() {
        let mut trials = Trials::default();
        trials.refused("hub", 2, StreamProfile::Main);
        assert!(!trials.offer("hub", false));
        assert!(trials.offer("hub", true));
        assert!(!trials.offer("nvr", true));
        assert_eq!(trials.start("hub", false), None);
        assert_eq!(trials.start("hub", true), Some((2, StreamProfile::Main, 0)));
        assert!(!trials.offer("hub", true), "it runs");
        assert_eq!(trials.start("hub", true), None);
        trials.end("hub");
        assert!(!trials.offer("hub", true), "a new refusal offers the next one");
    }

    /// Stop before the cipher step's reconnection: the trial is over, the device connects as always and the new
    /// connection sends no form.
    #[test]
    fn stop_before_the_reconnect_step_leaves_a_plain_reconnection() {
        let mut trials = Trials::default();
        trials.refused("hub", 0, StreamProfile::Sub);
        trials.start("hub", true);
        assert_eq!(trials.cancel("hub"), Some(Phase::OnLink));
        assert_eq!(trials.on_reconnect("hub", true), None, "not made again");
        assert!(!trials.arm("hub"));
        assert_eq!(trials.on_connected("hub", Some(0)), AfterLogin::Nothing);
        assert_eq!(trials.cancel("hub"), None, "said once");
    }

    /// Stop after the reconnect step (the armed connection is being made): no form goes out on it.
    #[test]
    fn stop_after_the_reconnect_step_sends_no_form() {
        let mut trials = Trials::default();
        trials.refused("hub", 0, StreamProfile::Sub);
        trials.start("hub", true);
        assert_eq!(trials.on_reconnect("hub", true), Some(true));
        assert!(trials.arm("hub"));
        assert!(!trials.arm("hub"), "one connection only");
        assert_eq!(trials.cancel("hub"), Some(Phase::Armed));
        assert_eq!(trials.on_connected("hub", Some(0)), AfterLogin::Nothing);
    }

    /// Another device or channel watched once the armed connection is up: the trial ends there.
    #[test]
    fn the_last_forms_go_out_only_while_the_channel_is_watched() {
        let mut trials = Trials::default();
        for watching in [Some(0), Some(1), None] {
            trials.refused("hub", 0, StreamProfile::Main);
            trials.start("hub", true);
            trials.on_reconnect("hub", true);
            trials.arm("hub");
            let after = trials.on_connected("hub", watching);
            let wanted = if watching == Some(0) {
                AfterLogin::Continue { channel: 0, refused: StreamProfile::Main, gen: 0 }
            } else {
                AfterLogin::Cancelled
            };
            assert_eq!(after, wanted, "{watching:?}");
            trials.end("hub");
        }
        // A timeout or a lost connection: over, and the next connection is made as always.
        trials.refused("hub", 0, StreamProfile::Main);
        trials.start("hub", true);
        assert_eq!(trials.on_reconnect("hub", false), Some(false));
        assert!(!trials.arm("hub"));
    }

    /// The window's links, trials and carried commands for one device's `Bench`, as `wire` lends them.
    struct Panel {
        trials: Trials,
        links: HashMap<String, Link>,
        carried: HashMap<String, Carried>,
    }

    impl Panel {
        fn new(bench: &Bench, trials: Trials) -> Self {
            let links = HashMap::from([("hub".to_string(), Link { id: 0, link: bench.window_link() })]);
            Self { trials, links, carried: HashMap::new() }
        }

        fn wiring(&mut self) -> Wiring<'_> {
            Wiring { trials: &mut self.trials, links: &mut self.links, carried: &mut self.carried, ended: Vec::new() }
        }
    }

    /// The window as its paths see it (`Paths`), without a display: "hub" (channel 5) has a link whose commands
    /// the test reads, "nvr" (channel 0) has none, and each step of the window's own is said by name. `heard` gives
    /// the commands and the steps in the order they came.
    struct Shell {
        trials: RefCell<Trials>,
        links: RefCell<HashMap<String, Link>>,
        carried: RefCell<HashMap<String, Carried>>,
        commands: RefCell<Commands>,
        said: RefCell<Vec<String>>,
        playing: RefCell<Option<String>>,
        recording: Cell<bool>,
        playback_shown: Cell<bool>,
    }

    impl Shell {
        /// Watching "hub", the log written, and a trial running on "hub" unless `trial` is false.
        fn new(trial: bool) -> Rc<Self> {
            let mut trials = Trials::default();
            trials.refused("hub", 5, StreamProfile::Main);
            if trial {
                trials.start("hub", true).expect("offered");
            }
            let (link, commands) = listened();
            Rc::new(Self {
                trials: RefCell::new(trials),
                links: RefCell::new(HashMap::from([("hub".to_string(), Link { id: 0, link })])),
                carried: RefCell::new(HashMap::new()),
                commands: RefCell::new(commands),
                said: RefCell::new(Vec::new()),
                playing: RefCell::new(Some("hub".to_string())),
                recording: Cell::new(true),
                playback_shown: Cell::new(false),
            })
        }

        fn say(&self, step: String) {
            let mut said = self.said.borrow_mut();
            said.extend(self.commands.borrow_mut().said());
            said.push(step);
        }

        /// What came since last asked.
        fn heard(&self) -> Vec<String> {
            let mut said = self.said.take();
            said.extend(self.commands.borrow_mut().said());
            said
        }
    }

    impl Paths for Shell {
        fn wired<R>(&self, act: impl FnOnce(&mut Wiring) -> R) -> R {
            wire(&self.trials, &self.links, &self.carried, act, |key| self.say(format!("ended {key}")))
        }
        fn playing_key(&self) -> Option<String> {
            self.playing.borrow().clone()
        }
        fn recording(&self) -> bool {
            self.recording.get()
        }
        fn refresh_trial_entries(&self) {
            self.say("entries".into());
        }
        fn set_trials_shown(&self, shown: bool) {
            self.say(format!("trials shown {shown}"));
        }
        fn end_talk(&self) {
            self.say("talk ended".into());
        }
        fn show_message(&self, text: &str) {
            self.say(format!("message {text}"));
        }
        fn stopped(&self, key: &str) {
            *self.playing.borrow_mut() = None;
            self.say(format!("stopped {key}"));
        }
        fn suspended(&self, key: &str) {
            *self.playing.borrow_mut() = None;
            self.say(format!("suspended {key}"));
        }
        fn noted_for_playback(&self, key: &str) -> bool {
            let shown = self.playback_shown.get();
            if shown {
                self.say(format!("noted {key}"));
            }
            shown
        }
        fn channel_of(&self, key: &str) -> Option<u8> {
            [("hub", 5), ("nvr", 0)].into_iter().find(|d| d.0 == key).map(|d| d.1)
        }
        fn clear_picture(&self) {
            self.say("picture cleared".into());
        }
        fn starting(&self, key: &str, channel: u8) -> StreamProfile {
            *self.playing.borrow_mut() = Some(key.to_string());
            self.say(format!("starting {key} {channel}"));
            StreamProfile::Sub
        }
        fn started(&self, channel: u8, switched: bool) {
            self.say(format!("started {channel} switched {switched}"));
        }
        fn reconnecting(&self, key: &str) {
            self.say(format!("reconnecting {key}"));
        }
        fn reconnect(self: &Rc<Self>, key: &str) {
            self.say(format!("reconnect {key}"));
        }
        fn grace(&self) {
            self.say("grace".into());
        }
    }

    /// Each of the window's paths, run whole as the window runs it: the commands its links get and the window's
    /// own steps, in order, with "hub" watched and its trial running. Every path that ends the trial cancels it
    /// first (CancelTrial wakes the link) and writes its end line before the window's widget work.
    #[test]
    fn the_window_paths_send_the_same_commands_in_the_same_order() {
        type Path = fn(&Rc<Shell>);
        fn log(shell: &Rc<Shell>, on: bool) {
            shell.diagnostics_set(on, || {
                shell.recording.set(on);
                shell.say("applied".into());
            })
        }
        let rows: [(&str, Path, &[&str]); 9] = [
            ("stop", |s| s.stop(), &["CancelTrial", "Stop", "ended hub", "talk ended", "stopped hub"]),
            ("playback page", |s| s.suspend_live(), &[
                "CancelTrial", "Stop", "ended hub", "talk ended", "suspended hub",
            ]),
            ("another channel", |s| s.play("hub"), &[
                "CancelTrial", "ended hub", "entries", "picture cleared", "talk ended", "starting hub 5", "Play 5 Sub",
                "started 5 switched false",
            ]),
            ("another device", |s| s.play("nvr"), &[
                "CancelTrial", "Stop", "ended hub", "entries", "picture cleared", "talk ended", "starting nvr 0",
                "started 0 switched true",
            ]),
            ("cancel", |s| s.cancel_trial("hub"), &["CancelTrial", "ended hub"]),
            ("log off in the settings", |s| log(s, false), &[
                "CancelTrial", "ended hub", "applied", "trials shown false", "entries",
            ]),
            ("log found off", |s| s.recording_found(false), &[
                "CancelTrial", "ended hub", "trials shown false", "entries",
            ]),
            ("shutdown", |s| s.shutdown_all(), &["CancelTrial", "Shutdown", "ended hub", "grace"]),
            ("log on in the settings", |s| log(s, true), &["applied", "trials shown true", "entries"]),
        ];
        for (path, run, expected) in rows {
            let shell = Shell::new(true);
            let gate = shell.trials.borrow_mut().epoch("hub");
            let gen = gate.number();
            run(&shell);
            assert_eq!(shell.heard(), expected, "{path}");
            let ends = expected.contains(&"ended hub");
            assert_eq!(gate.cancelled(gen), ends, "{path}: no further form of the trial is admitted");
            assert_eq!(shell.trials.borrow().running().is_empty(), ends, "{path}");
        }
    }

    /// With nothing watched Stop and the Playback page do nothing and `play` only starts; with the Playback page
    /// shown `play` only notes the device; with no trial running nothing is cancelled, and leaving a device drops
    /// its refusal. A file that cannot be written once the log is turned on ends the trial after the new settings
    /// are in force.
    #[test]
    fn the_window_paths_end_no_trial_they_do_not_have_to() {
        let shell = Shell::new(true);
        *shell.playing.borrow_mut() = None;
        shell.stop();
        shell.suspend_live();
        assert_eq!(shell.heard(), [] as [&str; 0]);
        shell.play("hub");
        let started = ["picture cleared", "talk ended", "starting hub 5", "Play 5 Sub", "started 5 switched true"];
        assert_eq!(shell.heard(), started);
        let shell = Shell::new(true);
        shell.playback_shown.set(true);
        shell.play("nvr");
        assert_eq!(shell.heard(), ["noted nvr"]);
        let shell = Shell::new(false);
        shell.stop();
        assert_eq!(shell.heard(), ["Stop", "talk ended", "stopped hub"]);
        let shell = Shell::new(false);
        assert!(shell.trials.borrow().offer("hub", true));
        shell.play("nvr");
        let started = ["Stop", "entries", "picture cleared", "talk ended", "starting nvr 0", "started 0 switched true"];
        assert_eq!(shell.heard(), started);
        assert!(!shell.trials.borrow().offer("hub", true), "the refusal of the stream left no longer stands");
        let shell = Shell::new(true);
        shell.diagnostics_set(true, || {
            shell.recording.set(false);
            shell.say("applied".into());
        });
        assert_eq!(shell.heard(), ["applied", "CancelTrial", "ended hub", "trials shown false", "entries"]);
    }

    /// A trial's connection closes (`TrialReconnect`): the link goes (dropped, it shuts its connection down) and
    /// the device connects again. A running trial goes on to its cipher step (the next connection is armed for it)
    /// or is over, which the watched device's view says; a cancelled one says nothing and arms nothing.
    #[test]
    fn a_trial_reconnection_goes_on_ends_or_says_nothing() {
        let over = format!("message {}", crate::ui::bridge::NO_FORM_ACCEPTED);
        let quiet: &[&str] = &["Shutdown", "reconnecting hub", "entries", "reconnect hub"];
        let rows: [(&str, bool, bool, bool, Option<&'static str>, &[&str]); 5] = [
            ("cipher step", true, true, true, None, quiet),
            ("over, watched", true, false, true, None, &[
                "Shutdown", &over, "reconnecting hub", "entries", "reconnect hub",
            ]),
            ("over, its message", true, false, true, Some("gone"), &[
                "Shutdown", "message gone", "reconnecting hub", "entries", "reconnect hub",
            ]),
            ("over, not watched", true, false, false, None, quiet),
            ("cancelled", false, false, true, None, quiet),
        ];
        for (case, running, legacy, watched, message, expected) in rows {
            let shell = Shell::new(true);
            if !running {
                shell.cancel_trial("hub");
                shell.heard();
            }
            if !watched {
                *shell.playing.borrow_mut() = None;
            }
            shell.trial_reconnect("hub", legacy, message, Carried::default());
            assert_eq!(shell.heard(), expected, "{case}");
            assert!(!shell.links.borrow().contains_key("hub"), "{case}");
            let armed = shell.trials.borrow_mut().arm("hub");
            assert_eq!(armed, running && legacy, "{case}: the next connection arms the cipher step");
            assert!(shell.trials.borrow().running().len() == usize::from(armed), "{case}");
        }
    }

    /// What the user does to end a trial while the device leaves its first form unanswered, through the functions
    /// the window calls (`Wiring`): Stop and the Playback page (`stop_watched`), another channel or device (`leave`,
    /// then `play`), the log turned off in the settings or found off by the recording check (`log_switched`), closing
    /// (`shutdown`). Each cancels the trial, so no further Preview of it is admitted, and names it for its end line
    /// (`ended`); the connection logs out within 2 s, and the cancelled trial's reconnection (`trial_reconnected`)
    /// starts nothing by itself: only a stream the user asked for, carried over, plays on the next connection.
    #[tokio::test]
    async fn every_way_of_ending_a_trial_ends_it_at_once() {
        for action in ["stop", "another channel", "another device", "diagnostics off", "shutdown"] {
            let mut trials = Trials::default();
            let epoch = trials.epoch("hub");
            let mut bench = Bench::new(epoch.clone(), false, false, Carried::default()).await;
            let mut window = Panel::new(&bench, trials);
            let mut w = window.wiring();
            w.trials.refused("hub", 2, StreamProfile::Main);
            let (channel, refused, gen) = w.trials.start("hub", true).expect("offered");
            bench.link.try_live(channel, refused, gen);
            assert_eq!(bench.next_request().await.msg_id, MSG_ID_VIDEO, "{action}: the first form");
            match action {
                "stop" => assert_eq!(w.stop_watched(Some("hub".into())).as_deref(), Some("hub"), "{action}"),
                "another channel" | "another device" => {
                    let (key, channel) = if action == "another channel" { ("hub", 5) } else { ("nvr", 0) };
                    w.leave("hub", key);
                    w.play(key, channel, StreamProfile::Sub);
                }
                "diagnostics off" => w.log_switched(false),
                "shutdown" => assert!(w.shutdown(), "{action}: a link is told"),
                _ => unreachable!(),
            }
            assert_eq!(w.ended, ["hub"], "{action}: the trial is cancelled and its end line is due");
            assert!(epoch.cancelled(gen), "{action}: no further form of it is admitted");
            assert_eq!(bench.end_within(Duration::from_secs(2)).await, [MSG_ID_LOGOUT], "{action}");
            let event = bench.link.events.try_recv().ok();
            if action == "shutdown" {
                assert!(event.is_none(), "{action}: nothing after the window closed");
                continue;
            }
            let Some(DeviceEvent::TrialReconnect { legacy: false, message: None, carried }) = event else {
                panic!("{action}: the window hears the connection make way");
            };
            assert_eq!(w.trial_reconnected("hub", false, carried), None, "{action}: the trial is over");
            assert!(!w.trials.arm("hub"), "{action}: the next connection is an ordinary one");
            assert!(!w.links.contains_key("hub"), "{action}: the old link is gone");
            drop(bench.link);
            let mut next = Bench::new(epoch, true, false, w.take_carried("hub")).await;
            if action == "another channel" {
                let preview = next.next_request().await;
                assert_eq!((preview.msg_id, preview.channel_id), (MSG_ID_VIDEO, 5), "{action}: the user's stream");
            }
            next.link.shutdown();
            let rest = next.end_within(Duration::from_secs(2)).await;
            assert_eq!(rest, [MSG_ID_LOGOUT], "{action}: nothing else on the next connection");
        }
    }

    /// With nothing watched, Stop and the Playback page do nothing, and the log turned on or found on ends no trial.
    #[tokio::test]
    async fn with_nothing_watched_or_the_log_on_no_trial_ends() {
        let mut trials = Trials::default();
        let epoch = trials.epoch("hub");
        let mut bench = Bench::new(epoch.clone(), false, false, Carried::default()).await;
        let mut window = Panel::new(&bench, trials);
        let mut w = window.wiring();
        w.trials.refused("hub", 2, StreamProfile::Main);
        let (channel, refused, gen) = w.trials.start("hub", true).expect("offered");
        bench.link.try_live(channel, refused, gen);
        assert_eq!(bench.next_request().await.msg_id, MSG_ID_VIDEO);
        assert_eq!(w.stop_watched(None), None);
        w.log_switched(true);
        assert!(w.ended.is_empty(), "{:?}", w.ended);
        assert!(!epoch.cancelled(gen));
        assert_eq!(w.trials.running(), ["hub"]);
        bench.link.shutdown();
        assert_eq!(bench.end_within(Duration::from_secs(2)).await, [MSG_ID_LOGOUT]);
    }

    /// The user changes what to watch while the device has no link (after `TrialReconnect`, before the next
    /// connection): the newest wish replaces the Play carried from the trial's connection. Stop and another device
    /// leave no stream to start; another channel starts that one only.
    #[tokio::test]
    async fn live_view_asked_for_without_a_link_replaces_the_carried_play() {
        for (action, wanted) in [("stop", None), ("another device", None), ("another channel", Some(7))] {
            let mut trials = Trials::default();
            let epoch = trials.epoch("hub");
            let mut bench = Bench::new(epoch.clone(), false, false, Carried::default()).await;
            let mut window = Panel::new(&bench, trials);
            let mut w = window.wiring();
            w.trials.refused("hub", 2, StreamProfile::Main);
            let (channel, refused, gen) = w.trials.start("hub", true).expect("offered");
            bench.link.try_live(channel, refused, gen);
            assert_eq!(bench.next_request().await.msg_id, MSG_ID_VIDEO);
            // Channel 5 asked for during the trial: carried.
            assert!(w.leave("hub", "hub"));
            w.play("hub", 5, StreamProfile::Sub);
            assert_eq!(bench.end_within(Duration::from_secs(2)).await, [MSG_ID_LOGOUT]);
            let Ok(DeviceEvent::TrialReconnect { carried, .. }) = bench.link.events.try_recv() else {
                panic!("{action}: the connection makes way");
            };
            w.trial_reconnected("hub", false, carried);
            drop(bench.link);
            match action {
                "stop" => assert!(!w.stop("hub")),
                "another device" => {
                    assert!(!w.leave("hub", "nvr"));
                    w.play("nvr", 0, StreamProfile::Sub);
                }
                "another channel" => {
                    assert!(!w.leave("hub", "hub"));
                    w.play("hub", 7, StreamProfile::Sub);
                }
                _ => unreachable!(),
            }
            let mut next = Bench::new(epoch, true, false, w.take_carried("hub")).await;
            if let Some(channel) = wanted {
                let preview = next.next_request().await;
                assert_eq!((preview.msg_id, preview.channel_id), (MSG_ID_VIDEO, channel), "{action}");
            }
            next.link.shutdown();
            assert_eq!(next.end_within(Duration::from_secs(2)).await, [MSG_ID_LOGOUT], "{action}: no other stream");
        }
    }

    /// Channel 5 is asked for during a trial, so a Play for it is carried. Then Stop, another channel (7), another
    /// device, or two of them in either order, given while the trial's link is still stored, while the device has no
    /// link, or once the next link exists but has not begun to serve (as during its login). The next connection
    /// starts only the stream the user asked for last, and nothing when that was Stop or another device.
    #[tokio::test]
    async fn the_next_connection_starts_only_the_live_view_asked_for_last() {
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum At {
            OldLink,
            NoLink,
            NewLink,
        }
        let orders: [&[&str]; 7] =
            [&[], &["stop"], &["play 7"], &["stop", "play 7"], &["play 7", "stop"], &["nvr"], &["nvr", "play 7"]];
        for at in [At::OldLink, At::NoLink, At::NewLink] {
            for actions in orders {
                let wanted = match actions.last() {
                    None => Some(5),
                    Some(&"play 7") => Some(7),
                    Some(_) => None,
                };
                let act = |w: &mut Wiring| {
                    for action in actions {
                        match *action {
                            "stop" => {
                                w.stop("hub");
                            }
                            "nvr" => {
                                w.leave("hub", "nvr");
                                w.play("nvr", 0, StreamProfile::Sub);
                            }
                            _ => {
                                w.leave("hub", "hub");
                                w.play("hub", 7, StreamProfile::Sub);
                            }
                        }
                    }
                };
                let mut trials = Trials::default();
                let epoch = trials.epoch("hub");
                let mut bench = Bench::new(epoch.clone(), false, false, Carried::default()).await;
                let mut window = Panel::new(&bench, trials);
                let mut w = window.wiring();
                w.trials.refused("hub", 2, StreamProfile::Main);
                let (channel, refused, gen) = w.trials.start("hub", true).expect("offered");
                bench.link.try_live(channel, refused, gen);
                assert_eq!(bench.next_request().await.msg_id, MSG_ID_VIDEO);
                assert!(w.leave("hub", "hub"));
                w.play("hub", 5, StreamProfile::Sub);
                if at == At::OldLink {
                    act(&mut w);
                }
                assert_eq!(bench.end_within(Duration::from_secs(2)).await, [MSG_ID_LOGOUT], "{at:?} {actions:?}");
                let Ok(DeviceEvent::TrialReconnect { carried, .. }) = bench.link.events.try_recv() else {
                    panic!("{at:?} {actions:?}: the connection makes way");
                };
                w.trial_reconnected("hub", false, carried);
                drop(bench.link);
                if at == At::NoLink {
                    act(&mut w);
                }
                let mut next = Bench::new(epoch, true, false, w.take_carried("hub")).await;
                if at == At::NewLink {
                    w.links.insert("hub".into(), Link { id: 1, link: next.window_link() });
                    act(&mut w);
                }
                if let Some(channel) = wanted {
                    let preview = next.next_request().await;
                    assert_eq!((preview.msg_id, preview.channel_id), (MSG_ID_VIDEO, channel), "{at:?} {actions:?}");
                }
                next.link.shutdown();
                let rest = next.end_within(Duration::from_secs(2)).await;
                assert_eq!(rest, [MSG_ID_LOGOUT], "{at:?} {actions:?}: no other Preview");
            }
        }
    }

    /// A channel list's word on a channel's streams stands. A device without one, a standalone camera, has main
    /// and sub, and the third stream when its support table says so.
    #[test]
    fn a_camera_without_a_channel_list_offers_the_streams_its_support_table_tells() {
        use super::advertised_streams;
        use reoling::ChannelAbilities;
        let third = ChannelAbilities { extern_stream: true, ..Default::default() };
        let all = [StreamProfile::Main, StreamProfile::Extern, StreamProfile::Sub];
        let two = [StreamProfile::Main, StreamProfile::Sub];
        assert_eq!(advertised_streams(None, Some(third)), all);
        assert_eq!(advertised_streams(None, Some(ChannelAbilities::default())), two);
        assert_eq!(advertised_streams(None, None), two);
        assert_eq!(advertised_streams(Some(vec![StreamProfile::Sub]), Some(third)), [StreamProfile::Sub]);
    }

    /// A stream a trial got the device to play is offered even when the channel does not advertise it.
    #[test]
    fn the_selector_offers_the_stream_a_trial_plays() {
        use super::offered_streams;
        let sub = vec![StreamProfile::Sub];
        assert_eq!(offered_streams(sub.clone(), Some(StreamProfile::Main)), [StreamProfile::Main, StreamProfile::Sub]);
        assert_eq!(offered_streams(sub.clone(), Some(StreamProfile::Sub)), [StreamProfile::Sub]);
        assert_eq!(offered_streams(sub, None), [StreamProfile::Sub]);
    }

    /// A trial cancelled before its link read it, or before the reconnection for its cipher step, sends nothing:
    /// its number has moved on, also for the last forms a later login would ask for.
    #[test]
    fn a_cancelled_trial_number_has_moved_on() {
        let mut trials = Trials::default();
        let epoch = trials.epoch("hub");
        trials.refused("hub", 0, StreamProfile::Sub);
        let (.., gen) = trials.start("hub", true).expect("offered");
        assert_eq!(end_trial(&mut trials, None, "hub"), Some(Phase::OnLink));
        assert_ne!(epoch.number(), gen, "no further form of it is admitted");
        assert_eq!(end_trial(&mut trials, None, "hub"), None, "said once");
        // Armed, then another channel watched once logged in: cancelled the same way.
        trials.refused("hub", 0, StreamProfile::Sub);
        let (.., gen) = trials.start("hub", true).expect("offered");
        trials.on_reconnect("hub", true);
        trials.arm("hub");
        assert_eq!(trials.on_connected("hub", Some(1)), AfterLogin::Cancelled);
        assert_ne!(epoch.number(), gen);
    }
    use crate::ui::zoom_picture::ZoomPicture;
    use gtk4::prelude::*;
    use gtk4::{gdk, Box as GtkBox, Label, Orientation, Overlay, Revealer, RevealerTransitionType, Window};

    #[test]
    fn the_fullscreen_bar_spans_the_picture() {
        // A 16:9 picture on a 5120 x 1440 screen: 2560 wide, 1280 from each side.
        assert_eq!(bar_span(5120, (1280, 2560), 520, 560), (1280, 2560));
        // On a 16:9 screen it fills the screen, as before, also when the
        // picture's size rounds up a pixel past it.
        assert_eq!(bar_span(1920, (0, 1920), 520, 560), (0, 1920));
        assert_eq!(bar_span(1920, (0, 1921), 520, 560), (0, 1920));
        // Also within 2% of it: an 896 x 512 stream on 1920 x 1080 and on
        // 2560 x 1440; a little more than 2% short, under the picture. On the
        // ultrawide screen that stream is under the picture too.
        assert_eq!(bar_span(1920, (15, 1890), 520, 560), (0, 1920));
        assert_eq!(bar_span(2560, (20, 2520), 520, 560), (0, 2560));
        assert_eq!(bar_span(1920, (20, 1880), 520, 560), (20, 1880));
        assert_eq!(bar_span(5120, (1300, 2520), 520, 560), (1300, 2520));
        // 4:3 on 1920 x 1080.
        assert_eq!(bar_span(1920, (240, 1440), 520, 560), (240, 1440));
        // A portrait picture narrower than the controls: their natural
        // width, centred under it.
        assert_eq!(bar_span(1920, (656, 608), 520, 700), (610, 700));
        assert_eq!(bar_span(5120, (2155, 810), 520, 900), (2110, 900));
        // A video narrower than their natural width: all of it, but never
        // below their minimum, and then from the left edge as by default.
        assert_eq!(bar_span(650, (21, 608), 520, 700), (0, 650));
        assert_eq!(bar_span(400, (0, 400), 520, 700), (0, 520));
        assert_eq!(bar_span(400, (100, 200), 520, 700), (0, 520));
        // Nonsense in, no panic out.
        assert_eq!(bar_span(0, (0, 0), 0, 0), (0, 0));
        assert_eq!(bar_span(i32::MAX, (i32::MAX, i32::MAX), 0, 0).1, i32::MAX);
        assert_eq!(bar_span(100, (i32::MIN, i32::MIN), 50, 60), (0, 60));
    }

    /// Runs the main loop until `done`, for up to 5 s: a release build under
    /// Xvfb can take a while to map a first window.
    fn pump_until(done: impl Fn() -> bool) {
        let context = glib::MainContext::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            while context.pending() {
                context.iteration(false);
            }
            if done() || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// A blank picture of `width` x `height` pixels.
    fn texture(width: i32, height: i32) -> gdk::MemoryTexture {
        let pixels = vec![255; width as usize * height as usize * 4];
        gdk::MemoryTexture::new(
            width,
            height,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_owned(pixels),
            width as usize * 4,
        )
    }

    /// A picture whose size changes, as the video sink's does with a new
    /// frame size.
    mod frames {
        use gtk4::{gdk, prelude::*, subclass::prelude::*};
        use std::cell::Cell;

        mod imp {
            use super::*;

            #[derive(Default)]
            pub struct Frames {
                pub size: Cell<(i32, i32)>,
            }

            #[glib::object_subclass]
            impl ObjectSubclass for Frames {
                const NAME: &'static str = "ReolingTestFrames";
                type Type = super::Frames;
                type Interfaces = (gdk::Paintable,);
            }

            impl ObjectImpl for Frames {}

            impl PaintableImpl for Frames {
                fn intrinsic_width(&self) -> i32 {
                    self.size.get().0
                }

                fn intrinsic_height(&self) -> i32 {
                    self.size.get().1
                }

                fn snapshot(&self, _: &gdk::Snapshot, _: f64, _: f64) {}
            }
        }

        glib::wrapper! {
            pub struct Frames(ObjectSubclass<imp::Frames>) @implements gdk::Paintable;
        }

        impl Default for Frames {
            fn default() -> Self {
                glib::Object::new()
            }
        }

        impl Frames {
            pub fn set_size(&self, width: i32, height: i32) {
                self.imp().size.set((width, height));
                self.invalidate_size();
            }
        }
    }
    use frames::Frames;

    /// Needs a display: the bar of the fullscreen controls over a video, in
    /// a window of 800 x 450 (`placed` true: under the picture, as in the
    /// app; false: the overlay's own placement, as before), with a message in
    /// the middle and a notice at the top as the app has them.
    fn stage(placed: bool) -> (Window, Overlay, ZoomPicture, Revealer, [Label; 2]) {
        let picture = ZoomPicture::new();
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        let overlay = Overlay::new();
        overlay.set_child(Some(&picture));
        let message = Label::new(Some("Add a device to start"));
        message.set_halign(gtk4::Align::Center);
        message.set_valign(gtk4::Align::Center);
        message.set_wrap(true);
        overlay.add_overlay(&message);
        let notice = Label::new(Some("Snapshot saved"));
        notice.set_halign(gtk4::Align::Center);
        notice.set_valign(gtk4::Align::Start);
        notice.set_margin_top(12);
        overlay.add_overlay(&notice);
        let controls = GtkBox::new(Orientation::Horizontal, 0);
        controls.set_size_request(500, 40);
        let bar = Revealer::new();
        bar.set_transition_type(RevealerTransitionType::SlideUp);
        bar.set_valign(gtk4::Align::End);
        bar.set_child(Some(&controls));
        bar.set_reveal_child(true);
        overlay.add_overlay(&bar);
        if placed {
            place_bar_under_picture(&overlay, &bar, &picture);
        }
        let window = Window::builder().decorated(false).default_width(800).default_height(450).child(&overlay).build();
        window.present();
        pump_until(|| (overlay.width(), overlay.height()) == (800, 450));
        assert_eq!((overlay.width(), overlay.height()), (800, 450));
        (window, overlay, picture, bar, [message, notice])
    }

    /// A widget's place in its overlay: x, y, width, height.
    fn bounds_in(overlay: &Overlay, widget: &impl IsA<gtk4::Widget>) -> (i32, i32, i32, i32) {
        let bounds = widget.compute_bounds(overlay).expect("the widget is in the overlay");
        (bounds.x() as i32, bounds.y() as i32, bounds.width() as i32, bounds.height() as i32)
    }

    /// Needs a display: the overlay puts the bar under the picture as the
    /// window, the stream, its size and the stretch setting change, and
    /// where the picture fills the video exactly where it was before.
    /// `gtk4::test`: on the one thread every GTK test shares.
    #[gtk4::test]
    #[ignore = "opens a window on the display; run explicitly"]
    fn the_fullscreen_bar_follows_the_picture() {
        gtk4::init().expect("a display");
        let (window, overlay, picture, bar, labels) = stage(true);
        let (before_window, before_overlay, before_picture, before_bar, before_labels) = stage(false);
        let both = |width: i32, height: i32| {
            overlay.allocate(width, height, -1, None);
            before_overlay.allocate(width, height, -1, None);
            // The message and the notice stay where the overlay puts them.
            for (label, before) in labels.iter().zip(&before_labels) {
                assert_eq!(bounds_in(&overlay, label), bounds_in(&before_overlay, before), "{}", label.label());
            }
            (bounds_in(&overlay, &bar), bounds_in(&before_overlay, &before_bar))
        };

        // Without a picture, and with one that fills a video of its shape,
        // the bar is where the overlay put it before.
        let (now, before) = both(800, 450);
        assert_eq!(now, (0, 410, 800, 40));
        assert_eq!(now, before);
        for paintable in [&picture, &before_picture] {
            paintable.set_paintable(Some(&texture(160, 90)));
        }
        for (width, height) in [(800, 450), (1920, 1080), (1280, 720), (500, 300)] {
            let (now, before) = both(width, height);
            assert_eq!(now, before, "16:9 on {width} x {height}");
        }
        let (now, _) = both(1920, 1080);
        assert_eq!(now, (0, 1040, 1920, 40));

        // On an ultrawide screen: under the picture and as wide as it, not
        // across the screen as before.
        let (now, before) = both(5120, 1440);
        assert_eq!(now, (1280, 1400, 2560, 40));
        assert_eq!(before, (0, 1400, 5120, 40));

        // The window lays itself out again on its own (800 x 450) when the
        // picture changes: a new stream (4:3), then new frame sizes on the
        // same paintable, as the video sink reports them.
        picture.set_paintable(Some(&texture(160, 120)));
        pump_until(|| bounds_in(&overlay, &bar) == (100, 410, 600, 40));
        assert_eq!(bounds_in(&overlay, &bar), (100, 410, 600, 40));
        let frames = Frames::default();
        frames.set_size(640, 480);
        picture.set_paintable(Some(&frames));
        pump_until(|| bounds_in(&overlay, &bar) == (100, 410, 600, 40));
        assert_eq!(bounds_in(&overlay, &bar), (100, 410, 600, 40));
        frames.set_size(1600, 900);
        pump_until(|| bounds_in(&overlay, &bar) == (0, 410, 800, 40));
        assert_eq!(bounds_in(&overlay, &bar), (0, 410, 800, 40));
        // Portrait: 254 wide at 273, so the controls' 500, centred under it.
        frames.set_size(360, 640);
        pump_until(|| bounds_in(&overlay, &bar) == (150, 410, 500, 40));
        assert_eq!(bounds_in(&overlay, &bar), (150, 410, 500, 40));

        // Stretched, the picture fills the video and so does the bar.
        picture.set_keep_aspect_ratio(false);
        pump_until(|| bounds_in(&overlay, &bar) == (0, 410, 800, 40));
        assert_eq!(bounds_in(&overlay, &bar), (0, 410, 800, 40));
        assert_eq!(both(5120, 1440).0, (0, 1400, 5120, 40));
        picture.set_keep_aspect_ratio(true);

        picture.set_paintable(Some(&texture(160, 90)));
        assert_eq!(both(5120, 1440).0, (1280, 1400, 2560, 40));

        // Hidden, the bar keeps its width with no height at the bottom.
        bar.set_transition_duration(0);
        bar.set_reveal_child(false);
        assert_eq!(both(5120, 1440).0, (1280, 1440, 2560, 0));

        // A new stream clears the picture, then shows a paintable with no
        // frame yet: the bar stays where it was meanwhile, and the first
        // frame places it.
        bar.set_reveal_child(true);
        picture.set_paintable(None::<&gdk::Paintable>);
        assert_eq!(both(5120, 1440).0, (1280, 1400, 2560, 40));
        let frames = Frames::default();
        picture.set_paintable(Some(&frames));
        assert_eq!(both(5120, 1440).0, (1280, 1400, 2560, 40));
        frames.set_size(640, 480);
        assert_eq!(both(5120, 1440).0, (1600, 1400, 1920, 40));

        // Without a picture on a video of another size, the whole width, as
        // before, and the old place is gone when the size comes back.
        picture.set_paintable(None::<&gdk::Paintable>);
        assert_eq!(both(3840, 1080).0, (0, 1040, 3840, 40));
        assert_eq!(both(5120, 1440).0, (0, 1400, 5120, 40));
        // Nor does it outlast fullscreen: the bar leaves the video and comes
        // back.
        picture.set_paintable(Some(&texture(160, 90)));
        assert_eq!(both(5120, 1440).0, (1280, 1400, 2560, 40));
        picture.set_paintable(None::<&gdk::Paintable>);
        bar.set_visible(false);
        bar.set_visible(true);
        assert_eq!(both(5120, 1440).0, (0, 1400, 5120, 40));

        window.close();
        before_window.close();
    }

    #[test]
    fn the_saved_password_goes_only_when_the_device_calls_it_wrong() {
        // The saved password: kept after a login, forgotten after a wrong-password refusal.
        assert_eq!(keyring_step(true, None), KeyringStep::Keep);
        assert_eq!(keyring_step(false, None), KeyringStep::Forget);
        // A typed password: the dialog's choice once it has logged in, nothing at all when it was wrong.
        assert_eq!(keyring_step(true, Some(Remember::Store)), KeyringStep::Store);
        assert_eq!(keyring_step(true, Some(Remember::Forget)), KeyringStep::Forget);
        assert_eq!(keyring_step(false, Some(Remember::Store)), KeyringStep::Keep);
        assert_eq!(keyring_step(false, Some(Remember::Forget)), KeyringStep::Keep);
    }

    #[test]
    fn the_stream_selector_opens_after_a_failed_start_but_not_while_starting() {
        // Starting: no frame and no failure yet.
        assert!(!stream_selectable(true, false, false));
        // Playing, as before.
        assert!(stream_selectable(true, true, false));
        // Refused or broken off: another stream can be picked.
        assert!(stream_selectable(true, false, true));
        // Nothing watched (stopped, on the Playback page, removed or lost): closed, whatever came before.
        for (streaming, refused) in [(false, false), (true, false), (false, true), (true, true)] {
            assert!(!stream_selectable(false, streaming, refused), "streaming {streaming}, refused {refused}");
        }
    }
}
