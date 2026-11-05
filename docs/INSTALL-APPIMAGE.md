# Running the AppImage

For any x86_64 distribution with glibc 2.39 and GLib 2.80 or newer
(Ubuntu 24.04, Fedora 40, Debian 13 and later), above all those the
[.deb](INSTALL-DEB.md) is not for. The AppImage is one file that runs
without being installed. It mounts itself with FUSE, which needs
`/dev/fuse` and the `fusermount` program (on Debian and Ubuntu, package
`fuse3`); without FUSE it can unpack itself and run, see
[Troubleshooting](#troubleshooting). The file is
`Reoling-<version>-x86_64.AppImage`, from the
[latest release](https://github.com/derSobi/Reoling/releases/latest).

## Run it

In the commands below, `<version>` stands for the version in the file's
name: type the name up to `Reoling-` and press Tab to complete it.

1. Download `Reoling-<version>-x86_64.AppImage` from the
   [latest release](https://github.com/derSobi/Reoling/releases/latest).
   Move it to where it is to stay: the menu entry it writes starts that
   file.
2. Open a terminal in that folder and make the file executable:

   ```bash
   chmod +x Reoling-*-x86_64.AppImage
   ```

3. Start it, with its full name (a pattern would match older copies
   too):

   ```bash
   ./Reoling-<version>-x86_64.AppImage
   ```

From then on, Reoling is in the applications menu (see
[What it writes](#what-it-writes)). `--version` prints the version.

## Which GTK and GStreamer it uses

- Inside the file are GTK 4, GStreamer with the parsers, muxers, decoders
  (libav in software; the nvcodec and va plugins for hardware) and sound
  sinks the app uses, its GTK 4 sink, librsvg and the icons it draws.
- Where the system's GTK 4 and GStreamer are at least as new as its own
  (GTK 4.14, GStreamer 1.24), it uses the system's. It then looks like
  the desktop's other applications, with the desktop's own theme.
- Otherwise it uses its bundled copies.
- A line on stderr says which: `reoling: GTK and GStreamer core: host` or
  `bundled`.
- `REOLING_GTK_STACK=bundled` or `REOLING_GTK_STACK=host` forces the
  choice, for example:

  ```bash
  REOLING_GTK_STACK=bundled ./Reoling-<version>-x86_64.AppImage
  ```

- It follows the desktop's GTK theme, and with it the accent colour.

## What it takes from the system

It always takes these from the system:

- glibc;
- GLib, whose GIO modules read the desktop's settings;
- Mesa (OpenGL/EGL) or the NVIDIA driver's GL;
- the core X11 client libraries (`libX11`, `libX11-xcb`, `libxcb`) and
  `libwayland-client`. GTK links `libX11` and `libwayland-client`, and
  Reoling `libwayland-client` too, so a session needs them on X11 and on
  Wayland alike;
- fontconfig and the fonts;
- PulseAudio, PipeWire (`pipewire-pulse`) or ALSA, for sound;
- `libva`. The FFmpeg libraries behind the software decoders need it too,
  with `libva-drm` and `libva-x11`; any of them the system lacks comes
  from the AppImage;
- the hardware decoding driver: NVIDIA's `libcuda`/`libnvcuvid` for
  NVDEC, or a VA-API driver for the system's `libva` (see
  [Hardware decoding](HARDWARE-DECODING.md));
- a Secret Service keyring (GNOME Keyring, KWallet), for the device
  passwords.

## What it writes

- Its own GStreamer registry, under `~/.cache/reoling`.
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
  and the dock shows its icon. The entry names the copy started last (a
  source build writes the same entry for itself), and the menu hides it
  once that file is gone.
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

Reoling runs once: started while it already runs, it only brings the
running window forward, and that start gives the new file no icon and no
menu entry. So quit it (close its window) before you start another copy.

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

1. Download the new `Reoling-<version>-x86_64.AppImage` from the
   [latest release](https://github.com/derSobi/Reoling/releases/latest).
2. Quit Reoling (close its window), so that the new file starts on its
   own.
3. Make it executable and start it once, by its full name:

   ```bash
   chmod +x Reoling-<version>-x86_64.AppImage
   ./Reoling-<version>-x86_64.AppImage
   ```

   The menu entry (where the .deb is not installed) now starts this copy,
   and the new file gets its icon. If Run at Startup is on, turn it off
   and on again in this copy (Client Settings, System Settings, tab
   General Settings): the autostart entry still starts the old file.
4. Delete the old file.

The devices, the settings and the passwords stay.

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

What Reoling keeps in your home folder stays, for the AppImage and the .deb
alike; delete it by hand if you want it gone:

- `~/.config/reoling`: the devices and the settings;
- the device passwords, in the desktop keyring;
- the snapshots, recordings and downloads, in `~/Pictures/Reoling` and
  `~/Videos/Reoling` or the folders set in Client Settings.

## Troubleshooting

- **It does not start: "Cannot mount AppImage, please check your FUSE
  setup."** FUSE is missing: install the `fusermount` program (on Debian
  and Ubuntu, package `fuse3`). Without FUSE, the AppImage can unpack
  itself instead:
  - `./Reoling-<version>-x86_64.AppImage --appimage-extract-and-run`
    unpacks it to a temporary directory, for that run only;
  - `./Reoling-<version>-x86_64.AppImage --appimage-extract` unpacks it
    for good, into `squashfs-root/` in the current folder; start that
    copy with `./squashfs-root/AppRun`.
- **A few "Theme parser error" lines on stderr, and the theme looks a
  little off.** On the bundled GTK, a theme written for a newer GTK may
  print them and lose some of its looks. The stderr line
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
  VA-API decoding)` on stderr.** The system has no `libva`, so there is no
  VA-API decoding; software decoding still works. See
  [Hardware decoding](HARDWARE-DECODING.md).
- **Black video, or no hardware decoding.** See
  [Hardware decoding](HARDWARE-DECODING.md).
