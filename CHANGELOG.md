# Changelog

All notable changes of Reoling, newest first.

## 1.0.5 — 2026-11-07

`reoling_1.0.5-1_amd64.deb` and two AppImages, all built on Ubuntu
24.04 by the release workflow: `Reoling-1.0.5-embed-x86_64.AppImage`,
built as 1.0.4's AppImage was, under a new name, and the new
`Reoling-1.0.5-system-x86_64.AppImage`, which takes GTK 4, GStreamer and
the decoders from the distribution. The .deb is built as 1.0.4's was,
and now draws the picture with its own GTK 4 sink also where the
distribution has one. Where a part a feature needs is missing, Reoling
says what and how to install it, instead of a panic or a black picture.
Beside that, Playback's timeline takes clicks right from its left edge,
and a device's error text no longer contains its UID.

- Two AppImages. The embed one carries GTK 4, GStreamer, its plugins and
  the decoders, as 1.0.4's did, and runs on any distribution with glibc
  2.39 and GLib 2.80 or newer; it is packed as 1.0.4's was, and its hook
  also tells the app that its plugins are its own. The system one, about
  6 MB, carries the app, its GTK 4 sink (with the sink's licence,
  MPL-2.0), the icons and the desktop entry; it needs GTK 4.14 and
  GStreamer 1.24 or newer with the base, good and bad plugins and a
  decoder for H.264 and H.265, and is tested in containers of Ubuntu
  24.04 and 26.04, Debian 13, Fedora 44, openSUSE Tumbleweed and Arch
  Linux. Both give the file its icon and, where the .deb is not
  installed, a menu entry that starts the copy started last; with the
  .deb installed, its entry takes over, and the one Reoling wrote is
  removed. The tag sits between the version and the architecture, so
  both names end in `-x86_64.AppImage`; the version itself has no tag.
- The system AppImage checks the system before the app starts: the
  libraries the app and its sink need, GTK 4.14 and GStreamer 1.24.
  Where that fails, it does not start the app (status 127) and says
  why, with the embed AppImage as the other way: on stderr and, started
  from the desktop, in a dialog or a desktop notification that needs no
  GTK. Where libraries are missing, it names them and the command that
  installs them on this distribution (apt, dnf, zypper or pacman, read
  from `/etc/os-release`); where GTK or GStreamer is only too old, it
  says which, with no command. `REOLING_SKIP_SYSTEM_CHECK=1` starts it
  all the same.
- Every package checks at start the GStreamer parts each feature is
  built from (the picture, H.264 and H.265 video, sound, talk, recording,
  downloads). Where one is missing, a window "Missing parts" names them,
  with the command that installs them and the steps for the codecs
  (OpenH264 or RPM Fusion's FFmpeg on Fedora, Packman on openSUSE); it
  names no decoder package whose GStreamer plugin is there already, so
  on a Fedora desktop H.265 gets the swap to RPM Fusion's FFmpeg and no
  package. "Don't show this again" keeps it away until the missing
  parts change. One line on stderr says the same. Before, a missing
  element panicked, and a missing decoder left the picture black.
- A stream that cannot be shown says why over the picture, with what
  installs the missing part. A live stream whose pipeline cannot be
  built ("Cannot show this stream: h265parse is missing.") stops, and
  the device stays connected; one that no decoder takes ("No H.265
  decoder on this system.") runs on without its picture, its sound and
  a recording going on. Playback pauses, and Play tries again with a
  new pipeline; picking the other stream there takes the text away, as
  a stream switch does in live view, and Play then tries that stream.
  A seek after the end of the last recording, where nothing opens,
  keeps the text. On a system with every part nothing of this shows,
  and live video, PTZ, pushes, Playback and downloads run as before.
- System Status says where the GStreamer plugins come from ("Plugins:
  the system's", "the system's, under the AppImage" or "bundled with the
  AppImage"), names the GTK 4 sink that draws the picture ("Display:
  gtk4paintablesink", its version and file) and what is missing, or
  "Missing: nothing".
- `reoling --check-requirements` prints that report without a window or
  a display and exits with 0 (nothing missing), 1 (H.264 or H.265 video,
  or any video, cannot be shown) or 2 (only sound, talk, recording or
  downloads miss something). The release's smoke tests run it for every
  package.
- On Fedora, OpenH264 on noopenh264, the stand-in a Fedora desktop has,
  decodes nothing; Reoling tries the decoder once and does not count it
  then.
- Decoding set to Software, for a codec with no software decoder
  installed, decodes that codec as Auto does, and System Status says
  so, instead of showing no picture.
- The .deb draws the picture with its own GTK 4 sink, the one Reoling
  is built and tested with (0.15.2, in
  `/usr/lib/reoling/gstreamer-1.0`), also where the distribution has
  one (`gstreamer1.0-gtk4` on Debian 13 and Ubuntu 26.04: 0.13.5 and
  0.14.4) or `GST_PLUGIN_PATH` names one. In 1.0.4, the distribution's
  was used instead. Only Reoling changes: while GStreamer starts, it
  puts its sink's directory first on the plugin path and uses a plugin
  registry of its own,
  `~/.cache/reoling/gstreamer-registry-deb-<GStreamer version>.bin`, as
  the AppImages do, then puts the environment back, so the programs it
  starts get yours. The registry other GStreamer programs share is not
  written, and they go on using the distribution's sink. Should the
  .deb's file fail to load, GStreamer says why on stderr, and the
  distribution's sink, where there is one, is used. System Status names
  the sink in use ("Display:").
- Playback's day timeline takes a click or a drag right from its left
  edge, where the day starts, while the device list is shown. Before,
  the device list's divider took presses up to about 15 px into the
  page, over the timeline's first pixels.
- When a device cannot be found through any P2P relay, the reason
  Reoling gives, such as the tooltip of its card's "Not connected",
  reads "could not resolve the device's UID against any P2P relay".
  Before, that text contained the UID itself.
- A connect that fails, and a login that fails other than by the
  device's refusal (which had its line already), print one line on
  stderr, such as "connecting to the device failed: P2P negotiation
  timed out", with the kind of failure only: no UID, address or text
  from the device.
- The release tests the system AppImage in containers of six
  distributions, each from its bare image: it must refuse with exactly
  that distribution's install command, then start once that command
  ran; on Fedora and openSUSE, its report must also not name the
  package of a plugin that is loaded there. The release also installs
  the .deb in containers of Debian 13 and Ubuntu 26.04 beside their
  `gstreamer1.0-gtk4`, where it must start with its own sink. A
  critical warning from GTK, GLib or GStreamer fails a smoke test. The
  release notes carry the SHA-256 of all three files.
- README: Installation names the three packages, each with what it
  needs, and links one guide for both AppImages. Docs: running the
  AppImages (INSTALL-APPIMAGE.md) explains which file to take, the
  install command per distribution and the codec notes for Fedora and
  openSUSE, what the warnings look like, and the two libraries the
  embed AppImage needs on a minimal Debian 13 install; installing the
  .deb says which GTK 4 sink it uses; building the packages and
  hardware decoding cover the system AppImage, and building the
  packages the new container tests.

## 1.0.4 — 2026-11-05

`reoling_1.0.4-1_amd64.deb` and `Reoling-1.0.4-x86_64.AppImage`,
built as 1.0.3's were; the .deb also installs the new guides. Tests on
a second NVR brought two fixes, for live video after a switch to the sub
stream and for the login, and Run at Startup now works from the
AppImage. The stream selector, the message and the debug output come
from [issue #3](https://github.com/derSobi/Reoling/issues/3), where a
Reolink Go (1st generation) refuses the live view request for the sub
stream with code 400. The README still lists the Go as not working yet;
with 1.0.4 another stream can be tried on it right in the stream
selector, the message says which, and the debug output tells more.

- After a switch from the main to the sub stream, live video no longer
  stays black. An NVR reached over the relay went on sending the main
  stream for a moment after the sub stream's reply (in our captures up
  to 0.2 s, and up to 0.7 s between two cameras), under the main
  stream's request number; live video took it for the new stream, picked
  the decoder from an old H.265 frame for an H.264 stream, and the
  picture stayed black. Live video now leaves out every video message
  that does not come under its own request's number, so going from one
  camera to another no longer shows a moment of the camera left behind
  either. Pushes, pictures, settings and control replies are read as
  before, and what the app sends is unchanged.
- A device that pushes a message, such as an alarm event, while a login
  waits for its reply no longer breaks that login. An NVR reached over
  the relay did so in one run of 14: the login took the push for the
  reply, failed, and the device's card said "Not connected". Each login
  step now reads past other messages until its own reply, for at most
  64 messages and 15 s; a channel list that comes ahead of the login
  reply is kept. The requests are unchanged, and a wrong password still
  clears the saved one.
- Run at Startup, turned on in the AppImage, starts the AppImage file at
  the next login. Before, the autostart entry named the program inside
  the AppImage's mount (`/tmp/.mount_…`), which is gone once Reoling
  exits, so it started nothing. The path in the entry is now quoted, so
  a folder with a space in its name works too, for every package. At
  start, Reoling points an autostart entry whose program is gone, such
  as one an earlier AppImage wrote, at the running copy; only its
  `Exec` changes, and Run at Startup is never turned on by it.
- The stream selector stays usable when a live stream failed to start
  or broke off, so another stream can be picked there, which starts it.
  Before, the selector was greyed out until video played, so a stream
  the camera refused could only be left by picking another one while
  another camera played. It stays greyed out, as before, while a stream
  starts ("Starting…"), while live video waits for a download on the
  device, and with nothing watched (stopped, on the Playback page, or
  with the device removed or lost). Only a failure of the channel being
  watched opens it.
- When a camera refuses a live stream, the message names the stream and
  the one to try instead: "The camera refused the sub stream (code
  400). Try the main stream in the stream selector." For the main
  stream it names the sub stream, for the extern stream the main or the
  sub stream. Before, it read "protocol error: camera rejected the video
  stream request with code 400". Only the message changed: the request
  stays as it was, and the app does not try another stream by itself.
- README: Installation is short, where to download and each package in
  a few lines, and links four new guides in docs/: installing the .deb
  (INSTALL-DEB.md), running the AppImage (INSTALL-APPIMAGE.md), hardware
  decoding (HARDWARE-DECODING.md) and building the packages
  (BUILDING-PACKAGES.md). The .deb installs them in
  `/usr/share/doc/reoling/docs`, next to FEATURES.md. They correct the
  old text: the AppImage needs `/dev/fuse` and the `fusermount` program
  (`fuse3` on Debian and Ubuntu), not `libfuse2`, as its runtime has
  libfuse built in.
- README: a new section, Tested devices, lists a Home Hub Pro and two
  RLN36 NVRs and the cameras on them, the second NVR's being a Reolink
  Duo 2 PoE, an RLC-1224A, five RLC-820A and a P330, with their hardware
  and firmware and what is tested on which; on the second NVR, Playback
  does not list the recordings yet. It also lists the models users
  reported: the RLC-823S1 works, the Reolink Go (1st generation) does
  not yet.
- For those who report a problem: with `REOLING_DEBUG_NEGOTIATION` set,
  stderr also gives, right after login, the P2P path that won (local,
  NAT-mapped or relay, by name only, never an address; a device added by
  IP prints none) and the streams the login reply offers (per group of
  channels, each stream's type, size and default frame rate), and says
  how the request for the device's name and model ended: the reply's
  code, the time it took and what its body gave, or that no reply came.
  A login step names each message it reads past by its id and number
  (`DEBUG login: read past message 33 under 0`), and live video says
  how much of an earlier stream it left out (`NEGOTIATION live: skipped
  23 messages (129073 bytes) of an earlier stream`). These lines, and
  those about the live stream and the device's name and model, start
  with the connection's number and, after login, the time since login,
  as `[conn 2 +812 ms]`, so that the lines of devices connecting side by
  side can be told apart; the number names no device. Without the
  variable nothing changes.
- `REOLING_PROBE_PUSH`, a diagnostic that prints the login reply and the
  replies after it, now also hides the contents of `serialNumber`,
  `userName`, `validpassword`, `password`, `mac`, `token`, `secretCode`
  and `bootSecret`. Before, it hid only the elements that can hold a
  UID, so a login reply's `secretCode` and `bootSecret` were printed as
  they came.

## 1.0.3 — 2026-11-01

`reoling_1.0.3-1_amd64.deb` and `Reoling-1.0.3-x86_64.AppImage`,
built as 1.0.2's were.

- Fullscreen: the controls lie under the picture and are as wide as it,
  not across the whole screen. On a 5120 × 1440 monitor a 16:9 picture
  shows 2560 px wide in the middle, and the buttons sat about 1280 px
  beside it. Where the picture is narrower than the controls (a portrait
  stream, or 4:3 on a small screen) they keep their own width, centred
  under it. A picture as wide as the screen or within 2% of it, and
  Stretch Mode, keep the full-width bar. Beside a narrower bar the
  pointer is over the video: the bar hides after 2.5 s there, a
  double-click leaves fullscreen and a middle click shows the whole
  picture again. While a new stream starts, the bar stays where the
  last picture was; when a stream of another shape shows, it narrows or
  widens about its centre.
- On X11 every Reoling window has the `WM_CLASS` `de.dersobi.reoling`,
  the `StartupWMClass` of its desktop entries, in place of `reoling`, so
  a dock or panel that matches windows on it can show Reoling's icon and
  group the windows with the launcher. On Wayland the main window had
  this app id already; Camera control, the settings windows and the
  dialogs now have it too. The accessible application name is
  `de.dersobi.reoling` as well (it was `reoling`), and GLib and GTK
  warnings in the terminal start with `(de.dersobi.reoling:PID)`.
- A connection that stays up for days keeps reading what the device
  sends. Every request takes the next of 65535 numbers; with a device
  idle and a keep-alive every 6 s, the count came round after about 4.5
  days, to 0 and to the subscription's number, under which the device
  sends channel lists and alarm events of its own: a live stream started
  under one of them would have left those unread. The count now skips
  them and the last live stream's number, and a number given out again
  starts clean: its answer is decrypted, and a preset's or Monitor
  Point's picture is made of its own bytes only. Tests run the count
  round; no device has been connected that long.
- The .deb also installs docs/FEATURES.md, in
  `/usr/share/doc/reoling/docs`, so the README's link to it works in the
  installed copy.
- The GTK 4 sink plugin in the .deb and in the AppImage is stripped,
  about a third smaller; lintian no longer reports it as unstripped.
  `scripts/build-gst-gtk4-plugin.sh` strips what it builds.
- Building from source: `Cargo.toml` names Rust 1.88 as the minimum
  (the locked dependencies have needed it since 1.0.0; it said 1.75),
  and `scripts/build-gst-gtk4-plugin.sh` needs Rust 1.92 or newer.
- README: it names the libraries the AppImage takes from the system,
  as 1.0.2's AppImage did too: `libX11`, `libX11-xcb`, `libxcb` and
  `libwayland-client`, on X11 and on Wayland alike.

## 1.0.2 — 2026-11-01

`reoling_1.0.2-1_amd64.deb` and `Reoling-1.0.2-x86_64.AppImage`,
built as 1.0.1's were.

- Digital video zoom in live view and playback, contributed by Simon
  Bechard (@simbesh): the wheel over the picture zooms in at the
  pointer, up to 16×, and out again; a left drag pans while zoomed in,
  and a middle click shows the whole picture again. The zoom stays when
  the window is resized or goes fullscreen; in live view it starts over
  with each new stream (another camera, channel or stream, Play after
  Stop), in playback it stays across seeks, speed changes and one day's
  recordings. The zoomed picture fills the area the whole picture
  takes, out to its edges, with no empty strip or blended border; the
  letterbox around it stays unless Stretch Mode is on. A double-click
  still toggles fullscreen when the mouse moves a pixel or two during
  it, and snapshots and recordings still hold the whole frame.
- On Wayland the wheel zooms at the pointer also right after the window
  went fullscreen, was maximized or moved while the mouse stood still,
  where the compositor tells the pointer's position, as GNOME Shell 50.1
  was measured to do; on other compositors it may zoom about the
  pointer's last known position. On X11 it zooms where the pointer is.
- Camera settings: on a Home Hub's cameras a few rows can be changed,
  with Save and Revert on their tab. On an E1 Outdoor PoE these are
  whether the date is shown, the watermark and the camera name's
  position on Display, brightness, flip and day and night mode on
  Image, and the status LED, the infrared lights and the volume on
  Light & sound; on an RLC-1224A, noise reduction and its level. Save
  writes one row at a time, as the official Reolink app was captured
  doing, each built from a fresh read of the tab; a Display Save also
  sends the flow-data overlay switched off, as that app does. Every
  other row outside Encoding stays read-only, and on an NVR's cameras or
  a camera added on its own only Encoding can be changed, as before.
  [docs/FEATURES.md](docs/FEATURES.md) has the details.
- A save that goes on with the settings window closed is given up when
  the camera's live video stops (Stop, the Playback page) or the
  connection is lost; when the window is opened again for that camera,
  the tab says so and shows what the camera has.
- Fullscreen: the controls hide after 2.5 s also when the mouse never
  moves, no longer keep coming back by themselves on Wayland, and stay
  up while their volume slider or stream list is open. When the window
  manager ends fullscreen by itself, the controls go back below the
  video, and the header bar and, if it was shown, the sidebar return.
- Escape leaves fullscreen with one press, also right after going
  fullscreen and after the controls were used. An open volume slider or
  stream list closes first, and the next Escape leaves fullscreen.
- A Home Hub or NVR with nothing playing stays connected: while no live
  video, playback or download runs on it, Reoling sends the keep-alive
  the official Reolink app sends, after every 6 s without a message
  from the device. Before, a Home Hub Pro dropped to "Not connected"
  after about a minute with nothing playing.
- The version history moved from the README to CHANGELOG.md, which the
  .deb installs beside the README in `/usr/share/doc/reoling`.
- Building from source also needs GTK 4 with its Wayland backend and
  `libwayland-client` 1.20 or newer with its headers (`libwayland-dev`,
  which `libgtk-4-dev` pulls in).

## 1.0.1 — 2026-10-25

`reoling_1.0.1-1_amd64.deb` and `Reoling-1.0.1-x86_64.AppImage`,
built as 1.0.0's were.

- AppImage: it runs on the system's GTK 4 and GStreamer where they are
  at least as new as its own (GTK 4.14, GStreamer 1.24), so a desktop
  theme written for a newer GTK renders in full (on newer Ubuntu
  releases this brings back the Playback calendar's marks for the days
  with recordings), and on its own copies otherwise; a line on stderr
  names the choice, and `REOLING_GTK_STACK=host` or `bundled` forces
  it. On a system with libva but without libva-x11 the software decoders
  and AAC sound work too: the AppImage brings the libva libraries the
  system lacks.
- Decoding: Auto prefers the integrated GPU's hardware decoder over a
  dedicated card's (1.0.0 took the one GStreamer ranked first, NVDEC
  over VA-API), then software; a second Auto mode, "Auto (dedicated GPU
  first)", prefers the card; an Auto stored by 1.0.0 is the first. System
  Status shows the kind of GPU each hardware decoder runs on, where it
  is known, and the decoders the latest live and recorded streams
  started with.
- Starting Reoling while it runs brings its open window to the front
  instead of logging in to every device a second time.
- A device refusing a login for another reason than a wrong password
  no longer makes Reoling forget the saved password, and neither does
  a mistyped password or user name in the login dialog.
- With Theme set to Auto, the theme is loaded once at start instead of
  three times.

## 1.0.0 — 2026-10-24

the first published packages: `reoling_1.0.0-1_amd64.deb` for
Ubuntu 24.04 LTS and later (Debian 13 and later too) and
`Reoling-1.0.0-x86_64.AppImage` for other distributions, both built on
Ubuntu 24.04 by the release workflow and attached to the release on
GitHub (see [Installation](README.md#installation)); `reoling --version` prints
the version.

- Settings pages, read-only, with the values as the device reports
  them: the camera settings window gets Info, Image, Display,
  Detection, Light & sound, Schedules and Network tabs beside Encoding,
  and "Device settings…" in a Home Hub's or NVR's card menu opens the
  device's own window — Info, Storage, Network, Time, Light & sound
  (Hub) or Alerts (NVR), Users, Maintenance. Rows a model does not have
  are left out, after the camera's own ability table. Only Encoding can
  be changed.
- Playback downloads: a range whose first recording is silent still
  gets the sound of a later recording it covers; two downloads started
  together never write into one file; a download the device does not
  answer in time is ended on the device and the connection stays
  usable; when the disk cannot keep up, the download fails instead of
  holding the recording in memory; and a damaged frame header in
  recorded footage no longer makes playback or a download wait (and
  grow in memory) for a frame that never completes — the stream resyncs
  at the next frame.
- Requests that arrive while a download holds the connection, the
  settings reads among them, wait and go out once it is over.
- Decoding: the hardware decoder — NVDEC or VA-API — is picked
  automatically, with the software decoders as the fallback. Hardware,
  set on a machine without a usable hardware decoder, decodes in
  software instead of leaving the picture black; the setting is shown
  as it is stored, the Decoding row says that no hardware decoder was
  found, and System Status names the mode in use.
- AppImage: it follows the desktop's theme and accent colour, bundles
  the NVIDIA and VA-API GStreamer plugins for hardware decoding, takes
  GLib and libva from the system (so the desktop's settings are read and
  the VA-API driver matches its library), and gives its own file the
  Reoling icon in GNOME Files from the first run on — the file's custom
  icon, set per user; other file managers may still show the generic
  icon. Where the .deb is not installed it also adds Reoling to the
  applications menu (a per-user desktop entry that starts the file), so
  the dock shows the Reoling icon for its window; once the .deb is
  installed, the next start of either package removes that entry and the
  .deb's takes over. It does this only for the AppImage file it was
  started from, and its SVG icons load on systems without librsvg too.

## 0.1.27 — 2026-10-23

Playback: a Download button saves a chosen time range (a start,
a length from 30 s to 15 min or the whole recording, main or sub stream)
as an MP4 with the recording's sound into the recordings folder, as one
file when the range crosses into the next recording; progress under the
controls, Cancel, "Open folder", and the range marked on the timeline.
A folder picked in the dialog is remembered as the recordings folder.

## 0.1.26 — 2026-10-20

Playback: sound, at 1× and in sync with the picture (the other
speeds are silent); the camera's detections as event bars on the day
timeline, a detection filter (any motion, person, vehicle, animal,
package, line crossing / zone) and previous/next-event buttons to jump
between them.

## 0.1.25 — 2026-10-20

Playback: recorded video per camera of a Home Hub or NVR (or of
a camera on its own), over the same UDP P2P connection as live video. A
calendar marks the days with recordings; the day timeline seeks, zooms
and pans with the mouse or the keyboard; play/pause, speeds from 0.25×
to 16×, main or sub stream, and it goes on into the next recording.
Playback has no sound yet.

## 0.1.24 — 2026-10-11

Camera settings: a malformed encoding reply from a device can no
longer crash the app, and an implausible I-frame range is not offered.

## 0.1.23 — 2026-10-11

Camera settings: the camera's picture, model and firmware, and
its encoding per stream (resolution, frame rate, bitrate, I-frame
interval), changeable within the values the camera allows — for example
raising the sub stream from 10 to 15 fps.

## 0.1.22 — 2026-10-11

live video and camera control:

- Pan/tilt keeps moving for as long as an arrow is held. A GTK timer used
  to send a stop about 0.4 s into every hold, so the camera moved in
  bursts.
- The picture stays at the chosen latency. Before, a burst or a network
  hiccup added delay that never went away (measured: 2.6 s instead of 1 s
  on Balanced).
- Sub and extern streams show every frame. On sub only about 1 frame in
  10 reached the screen: frames the decoder released late were discarded
  on the way to the display. Low frame rates are also paced evenly now.
- Switching streams no longer fails with "unsupported bcmedia magic" when
  the end of the previous stream is still arriving.
- A locked or busy device (too many users, device occupied…) now says so
  instead of "Incorrect password", and the saved password is kept.
- The list of P2P servers asked about a UID is corrected.
- New debug switches: `REOLING_DEBUG_LATENCY`, `REOLING_DEBUG_PTZ`,
  `REOLING_DUMP_VIDEO`, and `examples/replay_dump.rs` to replay a dump.

## 0.1.21 — 2026-10-02

the Download Settings screenshot is replaced by one without the
background behind the window.

## 0.1.20 — 2026-10-02

the official website, <https://reoling.eu>, is linked from the
README (with its screenshots, now kept in `docs/screenshots`), the About
window and the package metadata.

## 0.1.19 — 2026-09-23

System Status: the text no longer takes the focus (it opened with
everything selected); the Confirm button has it instead.

## 0.1.18 — 2026-09-23

the dock shows "Reoling" with its icon when run from a source
checkout (a per-user desktop entry is written at start unless the system
has one; `data/de.dersobi.reoling.desktop` is the one to install);
settings window taller (no scrolling); System Status no longer opens with
its text selected; "Support the project" points to GitHub Sponsors.

## 0.1.17 — 2026-09-23

the settings window is rebuilt (sidebar with System / Download / Local
Record / About, tabs under System Settings); new working settings: Run at
Startup, Auto Live View, Stretch Mode, and where screenshots and recordings
are saved; System Status and About windows.

## 0.1.16 — 2026-09-23

presets are listed in the camera's own order again (an id sort and
a locally remembered order were tried and dropped).

## 0.1.14 — 2026-09-23

"New Preset Point" now takes the camera's current picture as it
opens (click it to retake) and Confirm saves the preset with that picture,
which shows in the list at once.

## 0.1.13 — 2026-09-23

Add and Edit now follow a decrypted capture: Add takes a live picture
(`Snap`, 109), shrinks it to 169×95, uploads it (420) and saves the preset
at the current position with it; clicking the picture in Edit takes a new
one the same way; Confirm renames (a preset message with no `command`) and
attaches the new picture. Uploads go out with the JPEG in the clear, like
Talk.

## 0.1.12 — 2026-09-23

`REOLING_DEBUG_PRESETS=1` prints the preset-picture queue's steps
(to chase a "refresh all" that did nothing on real hardware).

## 0.1.11 — 2026-09-23

Preset Points redesigned (thumbnail/list toggle, refresh all, add, Edit
dialog, whole-card click moves the camera); pictures re-read one at a time
on every opening; the local thumbnail cache from 0.1.10 is gone again;
narrower remote window. Renaming a preset is not wired yet — the wire
message for it is unconfirmed.

## 0.1.10 — 2026-09-23

Monitor Point's and every preset's thumbnail is now cached
locally (`~/.cache/reoling/thumbnails`) and shown immediately from there
on the next start or page open, instead of staying blank until the
camera answers.

## 0.1.9 — 2026-09-23

found the real root cause with a second capture (Monitor Point
worked fine twice, but asking for a *preset that never got a thumbnail*
— e.g. one Reoling itself saved — always broke the connection, even as
a single, fully-serialized request): the camera's "no file by that
name" reply for that case declares a 0-byte body but still puts 4 extra
bytes on the wire that its own header doesn't account for, desyncing
every message read after it. The wire parser (`read_bc`) now recognizes
and skips those bytes, with a unit test pinning the exact captured byte
pattern; the 0.1.8 fix (refuse an overlapping image request) stays too,
since it's real and correct on its own, just wasn't what was disconnecting here.

## 0.1.8 — 2026-09-23

the 0.1.7 fix wasn't the whole story: a real capture (Monitor
Point refresh clicked twice, then Preset Points opened) showed the
camera stays confused by a 4th image-file request sent before the
earlier ones had each fully finished — even back-to-back, not truly
concurrent — replying with a malformed header that desynced the
connection. The client now refuses a new image request outright while
one is still outstanding (for Monitor Point or any preset), with a 10s
safety timeout in case the camera never replies at all.

## 0.1.7 — 2026-09-22

fixed a real-hardware bug where opening Preset Points
disconnected the camera (`bad magic header`): it was fetching every
preset's thumbnail at once, which the camera's image-file transfer
can't handle concurrently; each preset's thumbnail is now click-to-
refresh instead, matching Monitor Point's own thumbnail.

## 0.1.6 — 2026-09-22

Preset Points now show a thumbnail per preset, same as Monitor
Point, fetched from the camera under each preset's own name
(`preset_{id}`, confirmed from a capture of the camera's own
preset list) each time the Preset Points page opens.

## 0.1.5 — 2026-09-22

the remote is now paged (Calibration/Monitor Point/Preset Points reached
from the main page and left with "back") instead of showing everything at
once — much shorter; Monitor Point's thumbnail is now click-to-refresh;
calibration disables the panel with a spinner while it runs, with a 20 s
safety fallback in case the camera's reply never arrives.

## 0.1.4 — 2026-09-22

Monitor Point's saved thumbnail, shown in the remote; its timeout
now steps by exactly 1 second; calibration and Monitor Point confirmed
working on real hardware; control-reply messages (e.g. Monitor Point
actions were showing "Spotlight command sent") fixed to name the right
command.

## 0.1.3 — 2026-09-22

PTZ Monitor Point (Auto Return, timeout, return, reset) and
calibration, read from a capture decrypted with a local, standalone tool
(see `examples/decrypt_bc_stream.rs`).

## 0.1.2 — 2026-09-22

camera remote in its own window: zoom, focus (both read back after
a change), PTZ presets (save / go to / delete); a latency setting.

## 0.1.1 — 2026-09-21

camera controls (siren, spotlight, pan/tilt, talk), snapshot,
recording, audio, settings, per-channel streams, stop/play, fullscreen bar.

## 0.1.0 — 2026-09-20

protocol, P2P discovery, login, live video, first UI.
