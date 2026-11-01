# Reoling features

The full list behind the [README's Status](../README.md#status).

## What the app does today

- Devices: add by UID or IP; every saved device connects at start and shows
  *Connected* / *Not connected*; one password per device, kept in the desktop
  keyring; the device's own name, channels (with their names) and the streams
  each channel offers (main / extern / sub) are read from the device.
- Live view: click a device or one of its channels; the lightest stream is
  the default; Stop freezes the picture and becomes Play; the last stream
  restarts on the next start; fullscreen with a control bar that slides in
  on entering it and whenever the pointer moves, and out again after 2.5 s
  without movement (it stays while the pointer is on it and while its
  volume slider or stream list is open); the bar lies under the picture
  and is as wide as it (its own width, centred, under a narrower picture;
  the whole width in Stretch Mode or when the picture is within 2% of the
  screen's width), and beside it the pointer is over the video; a latency
  setting (Low / Balanced / Smooth) trades reaction time for smoothness
  over a poor connection.
- Digital video zoom in live view and playback: scroll up over the picture
  to zoom in at the pointer, and down to zoom out, up to 16×. Left-click and
  drag to pan while zoomed in; middle-click restores the whole picture.
  Zoom stays when resizing or entering fullscreen. In live view it resets
  whenever the stream starts again (another camera, channel or stream, Play
  after Stop); in playback it stays across seeks, speed changes and the
  recordings of one day, and resets on another day, camera or channel, once
  a lost connection is back, on leaving Playback and whenever the video
  codec changes (a main/sub switch can change it). It respects Stretch Mode;
  snapshots and recordings still contain the full frame.
- Sound: the stream's audio with a volume control; Talk (microphone to the
  camera, ADPCM).
- Playback (the tab beside Live View): recorded video per camera of a Home
  Hub or NVR, or of a camera added on its own, picked in the sidebar. A
  month calendar marks the days with recordings; the day timeline shows
  them as bands and seeks with a click or the arrow keys (10 s, 60 s with
  Shift), zooms with the wheel or `+`/`-` (Home shows the whole day again)
  and pans with a drag. Play/pause, speeds from 0.25× to 16×, main or sub
  stream, and playback goes on into the next recording by itself. At 1×
  the recording's sound plays in sync with the picture, at the volume set
  for live view; the other speeds are silent. The camera's detections
  are drawn as event bars over the recordings on the day timeline (the
  tooltip names them), a detection filter narrows them to any motion,
  person, vehicle, animal, package or line crossing / zone, and
  previous/next-event buttons jump between them. It runs over the same
  UDP P2P connection as live video, which pauses while the tab is open
  and comes back as it was. A Download button saves a chosen time range —
  a start, a length from 30 s to 15 min or the whole recording, main or
  sub stream — as an MP4 with the recording's sound into the recordings
  folder (another folder can be picked there and is remembered); a range
  that crosses into the next recording comes out as one file. The file is
  named after the device, camera, start and length, its range is marked
  on the timeline, and progress shows under the controls with Cancel and
  "Open folder"; the download runs on if the page moves to another day or
  device. While a device downloads, its playback and live video wait.
- Camera controls, shown per channel from what the camera reports: siren,
  spotlight, and — in a separate "Camera control" window, independent of the
  main window — pan/tilt (hold a direction to move) and zoom/focus on its
  main page (read back from the camera after every change, since zooming
  makes it autofocus); Monitor Point and Preset Points are their own pages,
  reached from there and left with "back". Monitor Point: a saved home
  position with its own thumbnail (click it to refresh — a point saved
  from Reoling has none until this is pressed once), Auto Return on/off
  and its timeout in 1-second steps, "Return to Monitor Point", "Reset
  Monitor Point" to save the current position. Presets: a
  thumbnail card per preset (or a plain list, via the toggle), a click
  anywhere on it moves the camera there, Edit (name and picture; click the
  picture to refresh it) and Delete beside it, "refresh all" and "add" on
  top. Every opening of the page re-reads all pictures from the camera,
  one at a time (it breaks on overlapping image requests). Calibration
  disables the panel with a spinner while it
  runs — all confirmed working against real hardware.
- Camera settings (the gear button beside Camera control): the camera's
  product picture, model and firmware, and its video encoding per stream.
  Clear (main) and Fluent (sub) can be changed: resolution, frame rate,
  bitrate and I-frame interval, offered only as the camera itself allows;
  the live picture restarts with the new settings. Balanced (extern) is
  shown as it is. Beside Encoding, further tabs show what else the
  camera tells about itself, with the values as the camera reports them:
  Info (model and firmware), Image (brightness, contrast, saturation,
  sharpness, hue, corridor mode, day and night switching, the advanced
  picture settings), Display (date, time and camera name on the picture,
  the watermark), Detection (motion sensitivity and alarm delay, smart
  detection types and object sizes, auto tracking), Light & sound
  (status LED, infrared lights, the spotlight with its schedule and
  triggers, sound), Schedules (the weekly recording, email, push, FTP
  and siren schedules, drawn as grids) and, except on an NVR's cameras,
  Network (address mode, IP, mask, gateway, MAC, DNS). Rows a model does
  not have — binning, auto tracking, a status LED, "Pet" instead of
  "Animal" — are left out, as the camera's own ability table says. On a
  Home Hub's cameras a few rows can be changed, each written with the
  message body the official Reolink app was captured sending; a row is
  offered only on the camera model its write was captured on, and only
  while the camera's reply has the captured shape and the row's current
  value is one of those offered. On an E1 Outdoor PoE these are, on
  Display, the date shown or hidden, the watermark and the camera name's
  position (top left or bottom right, offered when the date sits at
  neither); on Image, brightness (0 to 255), flip and the day and night
  mode (Auto or Black & white); on Light & sound, the status LED, the
  infrared lights (Auto or Off) and the volume (0 to 100). On an
  RLC-1224A it is noise reduction on Light & sound, with its level
  (1 to 5, locked while noise reduction is off). Each of these rows was
  confirmed working against a Home Hub Pro. Such a tab has Save and
  Revert. Save writes the edited rows one at a time, as the official app
  was captured changing one value per write, in a fixed order: on Display
  the name's position, the date, the watermark; on Image brightness, flip,
  day and night; on Light & sound the status LED, the infrared lights, the
  volume, noise reduction and its level. Before each write it reads the
  tab again and builds that row's write from that read: only that row's
  value changes, every other value the read returned goes back as it was,
  so a value changed elsewhere in the meantime, or by a row written just
  before, is kept; the parts no read returns go out as the official app
  sends them (on Display, the flow-data overlay switched off). A row whose
  value the camera already has when its turn comes is skipped. When the
  read finds a row that can no longer be changed, or gets no answer for
  the next row, nothing more is written and the footer says so. A row goes
  once the write before it was answered 200 (an Image change is followed
  at once by the day and night threshold, sent back unchanged as the
  official app does; the change counts as saved once its own write is
  answered 200); a refusal, or no answer within 10 s, stops the rest, and
  the changes not saved stay for another Save. The tab is then read again,
  and the footer names a saved value the camera did not keep or that could
  not be read back.
  Revert drops the changes not saved yet, and so do Reload and the window
  opening anew (the gear button pressed again, another camera or channel,
  Play after Stop). Every other row outside Encoding stays read-only, and
  on an NVR's cameras or a camera added on its own only Encoding can be
  changed. Reload reads a tab again; the tabs are read and saved while
  the camera's live video runs (press Play first if it is stopped) and
  wait while a download holds the connection. Product pictures come from
  Reolink's own site and are cached; until one loads, or for a model
  without one, a drawing of the device kind (camera, NVR, Home Hub)
  stands in.
- Device settings ("Device settings…" in the card menu of a connected Home
  Hub or NVR): the device's own window, read-only, one tab each. Info
  (model, firmware, UID), Storage (each drive's size and fill level, what
  happens when it is full, recording encryption), Network (address, the
  service ports — HTTP, HTTPS, RTSP, RTMP, ONVIF, UPnP —, DDNS, NTP and,
  on a Hub, the Wi-Fi it offers its cameras), Time (time zone, clock,
  date and time format, daylight saving, NTP), Light & sound on a Hub
  (status LED, notification and alarm volume, alarm pause) or Alerts on
  an NVR (recording, email, push and buzzer per camera), Users (the
  accounts, shared access, login lockout) and Maintenance (auto reboot).
  A tab is read when it is first shown and again on Reload; while a
  download holds the connection it says so and reads once that is over.
- Snapshot (PNG in `~/Pictures/Reoling`) and recording (MKV in
  `~/Videos/Reoling`, where Playback's downloads go too).
- Settings (the "Client Settings" window):
  System Settings (General, Live View & Playback, Alarm), Download Settings,
  Local Record and About. Working: Run at Startup (an XDG autostart entry),
  Theme, System Status (versions, the hardware decoders and the decoders in
  use), Auto Live View, Stretch Mode, decoding (Auto with the integrated or
  the dedicated GPU first / Hardware — with the decoder to use if there are
  several — / Software), latency, and the folders for screenshots and
  recordings. The window's rows Reoling cannot act on yet (client
  update, add devices automatically, lockscreen password, date format,
  language, detection overlays, scrollview time, alarm beep, temporary and
  download folders, local record) are shown greyed out. Colours are the
  active theme's.

## Not done yet

- Changing the other settings on the new pages: beyond Encoding and the
  few camera rows above, the camera and device pages are read-only.
- Playback of several cameras at once (one camera per page today), and
  thumbnails: the device list and the recordings show names and times,
  no pictures.
- Split View, and the camera's own zoom (Camera control) on cameras
  behind an NVR's sub-channel numbering.
- Playback: one download at a time per device, and the device's playback
  and live video wait while it runs; no download of several cameras or of
  a whole day at once.
- PTZ calibration has no separate "finished" signal from the camera — the
  app only knows the request was accepted.
- Talk has been built from this project's packet captures but is the least
  tested part.
- Presets saved by an older Reoling (before 0.1.13) have no picture until
  one is taken in their Edit dialog.
