# Changelog

All notable changes of Reoling, newest first.

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
