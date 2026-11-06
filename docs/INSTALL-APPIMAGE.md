# Running the AppImages

For any x86_64 distribution with glibc 2.39 and GLib 2.80 or newer
(Ubuntu 24.04, Fedora 40, Debian 13 and later), above all those the
[.deb](INSTALL-DEB.md) is not for. An AppImage is one file that runs
without being installed. It mounts itself with FUSE, which needs
`/dev/fuse` and the `fusermount` program (on Debian and Ubuntu, package
`fuse3`); without FUSE it can unpack itself and run, see
[Troubleshooting](#troubleshooting). Each release has two, on the
[latest release](https://github.com/derSobi/Reoling/releases/latest).

## Which file

| File | Size | GTK 4, GStreamer and the decoders |
|---|---|---|
| `Reoling-<version>-embed-x86_64.AppImage` | about 100 MB | inside the file |
| `Reoling-<version>-system-x86_64.AppImage` | about 6 MB | the distribution's |

- **The embed file** runs on the desktop install of any such
  distribution as it comes; a minimal Debian 13 install, a container for
  example, may lack two libraries it takes from the system (see
  [Troubleshooting](#troubleshooting)). Take it when unsure. Up to 1.0.4
  it was the only AppImage, named `Reoling-<version>-x86_64.AppImage`.
- **The system file** carries the app alone, with its GTK 4 sink. It needs
  GTK 4.14 and GStreamer 1.24 or newer from the distribution, with the
  base, good and bad plugins and a decoder for H.264 and one for H.265,
  which then come with the distribution's updates. One command installs
  them, see [What the system file needs](#what-the-system-file-needs);
  where something is missing, it says what, and how to install it. It is
  tested in containers of Ubuntu 24.04 and 26.04, Debian 13, Fedora 44,
  openSUSE Tumbleweed and Arch Linux.

Neither runs on Ubuntu 22.04, Debian 12 or other older systems: the
embed file needs glibc 2.39, the system file GTK 4.14 and GStreamer 1.24.

## Run it

In the commands below, `<version>` stands for the version in the file's
name: type the name up to `Reoling-` and press Tab to complete it. They
show the embed file; for the system file, write `system` in place of
`embed`.

1. Download the file from the
   [latest release](https://github.com/derSobi/Reoling/releases/latest).
   Move it to where it is to stay: the menu entry it writes starts that
   file.
2. Open a terminal in that folder and make the file executable:

   ```bash
   chmod +x Reoling-*-x86_64.AppImage
   ```

3. Start it, with its full name (a pattern would match the other file
   and older copies too):

   ```bash
   ./Reoling-<version>-embed-x86_64.AppImage
   ```

From then on, Reoling is in the applications menu (see
[What it writes](#what-it-writes)). `--version` prints the version.

The system file may stop at this point and say what the system lacks: see
[When something is missing](#when-something-is-missing).

## Which GTK and GStreamer it uses

The embed file:

- Inside the file are GTK 4, GStreamer with the parsers, muxers, decoders
  (libav in software; the nvcodec and va plugins for hardware) and sound
  sinks the app uses, its GTK 4 sink, librsvg and the icons it draws.
- It uses the system's GTK 4 and GStreamer where all of these hold:
  - they are at least as new as its own (GTK 4.14, GStreamer 1.24);
  - the dynamic loader finds, in the system's libraries, every library
    and symbol the app and its bundled plugins need (a failure its own
    copies share does not count);
  - the app starts on them: it answers `--version`.

  It then looks like the desktop's other applications, with the
  desktop's own theme.
- Otherwise it uses its bundled copies.
- A line on stderr says which: `reoling: GTK and GStreamer core: host` or
  `bundled`, and for `bundled` why.
- `REOLING_GTK_STACK=bundled` or `REOLING_GTK_STACK=host` forces the
  choice, for example:

  ```bash
  REOLING_GTK_STACK=bundled ./Reoling-<version>-embed-x86_64.AppImage
  ```

The system file always uses the system's GTK 4 and GStreamer, with the
system's plugins and decoders. Its line on stderr says `host`, with the
system's GTK and GStreamer versions and `app-only AppImage, the system's
plugins`. It ignores `REOLING_GTK_STACK`, and says that too.

Both follow the desktop's GTK theme, and with it the accent colour. Both
bring GStreamer's GTK 4 sink (gtk4paintablesink), which draws the
picture, and use it ahead of one the distribution may have
(`gstreamer1.0-gtk4` and the like).

## What the system file needs

- GTK 4.14 or newer;
- GStreamer 1.24 or newer, with its base, good and bad plugins (the bad
  ones have the H.264 and H.265 parsers);
- a decoder for H.264 and one for H.265: FFmpeg's (GStreamer's libav
  plugin) or OpenH264 in software, or a hardware decoder (see
  [Hardware decoding](HARDWARE-DECODING.md));
- a sound output for GStreamer, which a desktop has (PipeWire's,
  PulseAudio's or ALSA's plugin).

The system file reads the distribution from `/etc/os-release` and prints
its command when it lacks GTK 4 or GStreamer. The commands are these.

**Debian, Ubuntu and the distributions derived from them** (the .deb's
dependencies, the sound output left to the desktop):

```bash
sudo apt install libgtk-4-1 libgstreamer-gl1.0-0 gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav
```

A system without a sound output for GStreamer also needs one of
`gstreamer1.0-pipewire`, `gstreamer1.0-pulseaudio` or
`gstreamer1.0-alsa`.

**Fedora:**

```bash
sudo dnf install gtk4 gstreamer1-plugins-base gstreamer1-plugins-good gstreamer1-plugins-bad-free \
    gstreamer1-plugin-libav gstreamer1-plugin-openh264 openh264
```

Fedora's own FFmpeg (`ffmpeg-free`) decodes neither H.264 nor H.265:

- **H.264:** OpenH264 decodes it, the plugin
  `gstreamer1-plugin-openh264` with the library `openh264`. openh264
  comes from Fedora's fedora-cisco-openh264 repository (enabled by
  default) and replaces noopenh264, a stand-in that decodes nothing,
  which a Fedora desktop has. Reoling tries the OpenH264 decoder once at
  start and counts it only if it starts, so on the stand-in it reports
  no H.264 decoder, and names the library `openh264` alone: the plugin
  is there already.
- **H.265:** it needs the full FFmpeg from RPM Fusion: enable it
  (https://rpmfusion.org/Configuration), then:

  ```bash
  sudo dnf swap ffmpeg-free ffmpeg --allowerasing
  ```

  The swap does not bring `gstreamer1-plugin-libav`, the GStreamer
  plugin that uses FFmpeg; the command above installs it. Reoling names
  it only where that plugin is missing; with the plugin there, as on a
  Fedora desktop, its message gives the swap and no package. With RPM
  Fusion's FFmpeg, H.264 needs no OpenH264 either.
- If the decoder still does not show up, remove the plugin registry
  Reoling names in its message and start Reoling again. The system
  file's registries are in `~/.cache/reoling`:

  ```bash
  rm -f ~/.cache/reoling/gstreamer-registry-system-*.bin
  ```

  (The .deb's is `~/.cache/reoling/gstreamer-registry-deb-*.bin`; a
  source build uses GStreamer's own,
  `~/.cache/gstreamer-1.0/registry.*.bin`.)

**openSUSE:**

```bash
sudo zypper install libgtk-4-1 libgstgl-1_0-0 gstreamer-plugins-base gstreamer-plugins-good \
    gstreamer-plugins-bad gstreamer-plugins-libav
```

openSUSE's own FFmpeg decodes neither H.264 nor H.265; the decoders come
from Packman. `opi` adds the Packman repository and its codecs, and asks
before each step:

```bash
sudo zypper install opi
opi codecs
```

In a Tumbleweed container, the command above already gave an H.264
decoder (OpenH264, from openSUSE's openh264 repository), and only H.265
was missing.

**Arch Linux and the distributions derived from it:**

```bash
sudo pacman -S --needed gtk4 gst-plugins-base gst-plugins-good gst-plugins-bad-libs \
    gst-libav gst-plugin-va
```

**Other distributions:** the system file names what is missing (GTK 4,
GStreamer, or a GStreamer plugin) and leaves the command to the
distribution's package manager. Or take the embed file.

## When something is missing

### At the start of the system file

Before the app starts, the system file asks the dynamic loader whether
the system has the libraries the app and its GTK 4 sink need, checks for
GTK 4.14 and GStreamer 1.24 or newer, and asks the app for `--version`.
Where that fails, the app is not started, the start ends with status
127, and stderr says, a line each behind `reoling: `:

- `cannot start`, followed by `(missing: ...)` with the library files not
  found where some are missing; where nothing is missing (only too old,
  or another failure, below), the line is `cannot start` alone;
- that this AppImage takes GTK 4 and GStreamer, with its plugins and
  decoders, from the system;
- where libraries are missing, `This system lacks GTK 4, GStreamer.
  Install them with:` (or the like) and the command for this
  distribution from
  [What the system file needs](#what-the-system-file-needs), with
  Fedora's and openSUSE's codec line after it;
- where GTK or GStreamer is too old, `Too old on this system: GTK 4.6.9
  (Reoling needs 4.14)` (or GStreamer), with no command: the
  distribution's own GTK or GStreamer is too old;
- where nothing is missing or too old but the app still cannot load or
  start, `The dynamic loader reports: ...` or `The app does not start:
  ...`, with no command;
- `Or use Reoling-<version>-embed-x86_64.AppImage, which brings its own
  GTK 4 and GStreamer`, with the address of the releases.

Started from the desktop, where stderr goes unseen, it shows the same
text in a dialog, "Reoling cannot start" (zenity, or kdialog on KDE),
or, where no dialog program starts (zenity 4 needs GTK 4, which may be
what is missing), as a desktop notification.

`REOLING_SKIP_SYSTEM_CHECK=1` starts the app all the same, and stderr
says so. Where a library really is missing, the dynamic loader then
stops the app instead:

```bash
REOLING_SKIP_SYSTEM_CHECK=1 ./Reoling-<version>-system-x86_64.AppImage
```

### Once Reoling runs

At every start, Reoling checks the GStreamer parts each feature is built
from: the picture, H.264 video, H.265 video, sound, talk, recording and
downloads. Where one is missing:

- **A window, "Missing parts",** lists them, a line per feature, with
  the command that installs them on this distribution, where one is
  needed, and any steps; from the system file it ends with `Or use
  Reoling-<version>-embed-x86_64.AppImage, which brings these parts
  along.` Tick **Don't show this again** before OK, and it stays away
  until the missing parts change.
- **One line on stderr:** `reoling: missing on this system: ...`, with
  the command, where one is needed, after `install:`.
- **System Status** (Client Settings, System Settings, tab General
  Settings, row System Status, View) has, after its decoder lines, where
  the GStreamer plugins come from, the GTK 4 sink in use and what is
  missing. With every part there, from the system file:

  ```
  Plugins: the system's, under the AppImage
  Display: gtk4paintablesink <version> (<file>)
  Missing: nothing
  ```

  `Plugins:` reads `bundled with the AppImage` in the embed file and
  `the system's` in the .deb. With something missing, the same two lines
  come first (`Display: gtk4paintablesink missing` where the sink is
  what is missing), then `Missing for <feature>: ...`, a line per
  feature, and the same advice.
- **Over the picture,** a stream that cannot be shown says why, and what
  installs it:
  - an element missing, for example "Cannot show this stream: h265parse
    is missing.", or a pipeline that does not start: a live stream stops
    with its first frame, and the device stays connected, so another
    stream or camera can be picked;
  - no decoder that takes the stream, "No H.265 decoder on this system."
    (or H.264), which shows once the stream has started: a live stream
    runs on without its picture, its sound and a recording go on, and
    another stream or camera can be picked;
  - in Playback, either way, playback pauses, and Play tries again;
    picking the other stream takes the text away, and Play then tries
    that one.

Install what it names and start Reoling again: GStreamer reads its
plugins at start.

It names no decoder package whose GStreamer plugin is there already,
as installing it would change nothing: FFmpeg's plugin (libav) has
decoders only for the codecs the system's FFmpeg decodes, and Fedora's
OpenH264 plugin is there on the noopenh264 stand-in too. On Fedora,
H.265 then gets the swap to RPM Fusion's FFmpeg, and H.264 the library
`openh264` (see
[What the system file needs](#what-the-system-file-needs)); on
openSUSE, the decoders get Packman. Where FFmpeg's plugin is there but
lacks a decoder Reoling needs, and no such step brings one, it names no
package, only the plugin registry to remove: `If the decoder still does
not show up, remove ... and start Reoling again.`

The embed file brings every part. One missing there is a fault of the
package: the texts then name no command and ask you to report it at
https://github.com/derSobi/Reoling/issues. On a minimal Debian 13
install, see [Troubleshooting](#troubleshooting) first.

### From a terminal

`--check-requirements` prints the same report and exits, with no window
and no display. It works for both files, and for `reoling` from the .deb:

```bash
./Reoling-<version>-system-x86_64.AppImage --check-requirements
```

In an Ubuntu 24.04 container with the command above, which has no GPU
and so no hardware decoder, the system file prints:

```
Reoling <version>
GTK 4.14.5
GStreamer 1.24.2
Plugins: the system's, under the AppImage
Display: gtk4paintablesink 0.15.2-RELEASE (/tmp/.mount_Reolin.../usr/lib/gstreamer-1.0/libgstgtk4.so)
H.264 decoders: software
H.265 decoders: software
Missing: nothing
```

- `Plugins:` says where the GStreamer plugins come from: `bundled with
  the AppImage` (the embed file), `the system's, under the AppImage` (the
  system file) or `the system's` (the .deb, a source build).
- `H.264 decoders:` and `H.265 decoders:` say `software`, `hardware`,
  `software and hardware` or `none`.
- With something missing, `Missing:` lists it in short (for example
  `h265:decoder-h265`), and the window's text follows.

It exits with:

| Status | Meaning |
|---|---|
| 0 | nothing is missing |
| 1 | H.264 or H.265 video, or any video at all, cannot be shown |
| 2 | only sound, talk, recording or downloads miss something |
| 127 | the system file's check stopped it before the app ran (above) |

## What it takes from the system

Both files always take these from the system:

- glibc;
- GLib, whose GIO modules read the desktop's settings;
- Mesa (OpenGL/EGL) or the NVIDIA driver's GL;
- the core X11 client libraries (`libX11`, `libX11-xcb`, `libxcb`) and
  `libwayland-client`. GTK links `libX11` and `libwayland-client`, and
  Reoling `libwayland-client` too, so a session needs them on X11 and on
  Wayland alike;
- fontconfig and the fonts;
- PulseAudio, PipeWire (`pipewire-pulse`) or ALSA, for sound;
- `libva`. In the embed file, the FFmpeg libraries behind the software
  decoders need it too, with `libva-drm` and `libva-x11`; any of them the
  system lacks comes from the AppImage;
- `libgpg-error` and `libcom_err` (on Debian, `libgpg-error0` and
  `libcom-err2`): in the embed file, FFmpeg's plugin needs both and
  PulseAudio's `libgpg-error`. A desktop has both; a minimal install may
  not (see [Troubleshooting](#troubleshooting));
- the hardware decoding driver: NVIDIA's `libcuda`/`libnvcuvid` for
  NVDEC, or a VA-API driver for the system's `libva` (see
  [Hardware decoding](HARDWARE-DECODING.md));
- a Secret Service keyring (GNOME Keyring, KWallet), for the device
  passwords.

The system file takes everything else from the system as well: GTK 4 and
the libraries under it, GStreamer with all its plugins and decoders, and
FFmpeg. It carries only the app, its GTK 4 sink (with the sink's
licence, MPL-2.0), the icons and the desktop entry.

## What it writes

- Its own GStreamer registry, under `~/.cache/reoling`:
  `gstreamer-registry-<stack>-<GStreamer version>.bin`, the stack being
  `host` or `bundled` for the embed file and `system` for the system
  file.
- **An icon for its own file.** GNOME Files otherwise shows the bare
  "executable" icon for an AppImage. The first time it runs, Reoling:
  - copies the Reoling icon into your icon theme, under
    `~/.local/share/icons/hicolor`, for the sizes the system theme lacks;
  - sets it as the file's custom icon: the GIO attribute
    `metadata::custom-icon`, which Files shows at every zoom level and
    gvfs keeps per user and per path. By hand that is
    `gio set <file> metadata::custom-icon file://<icon>`;
  - writes the file's thumbnail-cache entries under `~/.cache/thumbnails`,
    for the file managers and dialogs that read the cache but no custom
    icon.

  Other file managers (Dolphin among them) may still show the generic
  icon.
- **A menu entry**, where the .deb is not installed: the per-user desktop
  entry `~/.local/share/applications/de.dersobi.reoling.desktop`, which
  starts the AppImage file. Reoling then appears in the applications menu,
  and the dock shows its icon. The entry names the copy started last
  (either file, and a source build writes the same entry for itself), and
  the menu hides it once that file is gone.
- **The autostart entry**, only while Run at Startup is on (Client
  Settings, System Settings, tab General Settings):
  `~/.config/autostart/de.dersobi.reoling.desktop`, which starts the
  AppImage file. At every start, Reoling points an entry whose program is
  gone (an earlier AppImage's, or a file moved or deleted since) at the
  copy that runs; only its `Exec` changes, and no entry is created.

The icon and the thumbnails are checked at every start, so a replaced file
(a new download) or a moved one gets its icon back the next time it runs.

For the desktop, that is all it touches, and only for the AppImage file it
was started from. It installs no thumbnailer and no file type association,
so nothing of Reoling's runs on other files. Its devices, settings and
passwords are kept as the .deb's are, see
[Removing](#removing).

Reoling runs once, whichever file or package it was started from: started
while it already runs, it only brings the running window forward, and
that start gives the new file no icon and no menu entry. So quit it
(close its window) before you start another copy.

## Using it alongside the .deb

A per-user desktop entry hides the .deb's of the same name. Once the .deb
is installed, the next start of either package, with Reoling not running,
removes the entry Reoling wrote, and the .deb's takes over. Until then,
the menu still starts the AppImage.

If the AppImage file is deleted before that, Reoling is missing from the
menu until `reoling` is started once from a terminal, with Reoling not
running.

With Run at Startup on, the autostart entry keeps starting the AppImage
while its file is there; to have it start the .deb's `reoling`, turn Run
at Startup off and on again in the .deb.

## Updating

1. Download the new file from the
   [latest release](https://github.com/derSobi/Reoling/releases/latest):
   `Reoling-<version>-embed-x86_64.AppImage` or
   `Reoling-<version>-system-x86_64.AppImage`. In place of 1.0.4's
   `Reoling-1.0.4-x86_64.AppImage`, the embed file is the same kind.
2. Quit Reoling (close its window), so that the new file starts on its
   own.
3. Make it executable and start it once, by its full name (here the embed
   file):

   ```bash
   chmod +x Reoling-<version>-embed-x86_64.AppImage
   ./Reoling-<version>-embed-x86_64.AppImage
   ```

   The menu entry (where the .deb is not installed) now starts this copy,
   and the new file gets its icon. If Run at Startup is on, turn it off
   and on again in this copy (Client Settings, System Settings, tab
   General Settings): the autostart entry still starts the old file.
4. Delete the old file.

The devices, the settings and the passwords stay. To change from one file
to the other, follow the same steps with the other file.

## Removing

1. If Run at Startup is on (Client Settings, System Settings, tab General
   Settings), turn it off first, so that no autostart entry is left
   behind.
2. Delete the AppImage file. The menu hides its entry once the file is
   gone.
3. If you want, remove what it wrote for the desktop and its cache:

   ```bash
   rm -f ~/.local/share/applications/de.dersobi.reoling.desktop
   rm -f ~/.local/share/icons/hicolor/*/apps/de.dersobi.reoling.png
   rm -rf ~/.cache/reoling
   ```

   The thumbnail-cache entries can stay.

What Reoling keeps in your home folder stays, for the AppImages and the
.deb alike; delete it by hand if you want it gone:

- `~/.config/reoling`: the devices and the settings;
- the device passwords, in the desktop keyring;
- the snapshots, recordings and downloads, in `~/Pictures/Reoling` and
  `~/Videos/Reoling` or the folders set in Client Settings.

## Troubleshooting

- **It does not start: "Cannot mount AppImage, please check your FUSE
  setup."** FUSE is missing or cannot be used, and the lines above the
  message say why: `No suitable fusermount binary found` means the
  `fusermount` program is missing (on Debian and Ubuntu, package
  `fuse3`), `fuse: device not found` that there is no `/dev/fuse`, and
  `mount failed: Permission denied` alone that the system does not allow
  the mount. Without FUSE, the AppImage can unpack itself instead (here
  the embed file):
  - `./Reoling-<version>-embed-x86_64.AppImage --appimage-extract-and-run`
    unpacks it into `$TMPDIR` (`/tmp` if that is not set) and runs it
    from there;
  - `./Reoling-<version>-embed-x86_64.AppImage --appimage-extract` unpacks
    it for good, into `squashfs-root/` in the current folder; start that
    copy with `./squashfs-root/AppRun`.

  `--appimage-extract-and-run` leaves its copy, a few hundred MB for the
  embed file, in `$TMPDIR` (or `/tmp`) as
  `appimage_extracted_<checksum>`: the next run of the same file uses it
  again, and a new version unpacks one of its own beside it. To free the
  space, with Reoling not running, delete the copies that hold Reoling
  (other AppImages may leave theirs there too):

  ```bash
  for f in "${TMPDIR:-/tmp}"/appimage_extracted_*/usr/bin/reoling; do
      rm -rf "${f%/usr/bin/reoling}"
  done
  ```

  Started with `--appimage-extract-and-run`, Reoling still writes its
  menu entry and, with Run at Startup on, its autostart entry; both start
  the AppImage file without the option, so without FUSE neither starts
  Reoling: start it from a terminal as above.
- **The system file does not start: `reoling: cannot start (missing:
  ...)` on stderr, or a dialog or notification "Reoling cannot start".**
  The system lacks GTK 4 or GStreamer: run the command it names (see
  [What the system file needs](#what-the-system-file-needs)), or take the
  embed file. A bare `reoling: cannot start`, with no `(missing: ...)`,
  comes with what is too old or what failed instead, and no command (see
  [At the start of the system file](#at-the-start-of-the-system-file)).
- **`Too old on this system: GTK ... (Reoling needs 4.14)`**, or
  GStreamer (1.24). The distribution's own GTK or GStreamer is too old
  for the system file. Take the embed file, where glibc is 2.39 or newer.
- **A window "Missing parts" at start, or `reoling: missing on this
  system: ...` on stderr.** Install what it names and start Reoling
  again, see [When something is missing](#when-something-is-missing).
- **"No H.265 decoder on this system." on Fedora or openSUSE,** also
  after installing. See the Fedora and openSUSE notes in
  [What the system file needs](#what-the-system-file-needs), the plugin
  registry among them.
- **The embed file on a minimal Debian 13 install (a container, for
  example): `reoling: missing on this system: an H.264 decoder (H.264
  video), an H.265 decoder (H.265 video); This AppImage should bring
  these parts along: please report this at ...`** on stderr, or the
  window "Missing parts" with the same two decoders and a request for a
  report. The first start may also print GStreamer warnings that
  `libgstlibav.so` and `libgstpulseaudio.so` failed to load:
  `libgpg-error.so.0: cannot open shared object file`. With
  `libgpg-error.so.0` there, only `libgstlibav.so` fails, on
  `libcom_err.so.2`. The embed file takes these two libraries from the
  system; a desktop install has both, a minimal one may not. Install
  them and start Reoling again:

  ```bash
  sudo apt install libgpg-error0 libcom-err2
  ```

  If the message is still there, quit Reoling, remove its plugin
  registries, and start it again. A registry keeps a plugin that failed
  to load as failed while the plugin's path stays the same, as it does
  with `--appimage-extract-and-run`:

  ```bash
  rm -f ~/.cache/reoling/gstreamer-registry-*.bin
  ```
- **A few "Theme parser error" lines on stderr, and the theme looks a
  little off.** On the embed file's bundled GTK, a theme written for a
  newer GTK may print them and lose some of its looks. The stderr line
  `reoling: GTK and GStreamer core: ...` says which GTK is in use.
- **Reoling is missing from the applications menu.** Quit Reoling if it
  runs, then start the AppImage once from where it is now: the entry
  names the copy started last, and the menu hides it once that file is
  gone. If the .deb is installed too, start `reoling` once from a
  terminal instead (see
  [Using it alongside the .deb](#using-it-alongside-the-deb)).
- **The file manager shows a generic icon.** GNOME Files shows the
  Reoling icon once the file has run; other file managers (Dolphin among
  them) may not.
- **`reoling: this system has no libva; using the AppImage's own copy (no
  VA-API decoding)` on stderr** (the embed file). The system has no
  `libva`, so there is no VA-API decoding; software decoding still works.
  See [Hardware decoding](HARDWARE-DECODING.md).
- **Black video, or no hardware decoding.** See
  [Hardware decoding](HARDWARE-DECODING.md).
