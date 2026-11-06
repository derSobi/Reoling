# Installing the .deb

For Ubuntu 24.04 LTS or newer, Debian 13 or newer, and the distributions
derived from them that have at least those library versions, on x86_64.
You need `sudo` and the package from the
[latest release](https://github.com/derSobi/Reoling/releases/latest),
`reoling_<version>-1_amd64.deb`. It is built on Ubuntu 24.04 LTS, so it
installs on 24.04 and everything newer. On another distribution, use one
of the [AppImages](INSTALL-APPIMAGE.md).

## Install

1. Download `reoling_<version>-1_amd64.deb` from the
   [latest release](https://github.com/derSobi/Reoling/releases/latest).
2. Open a terminal in the folder it was saved to, for example:

   ```bash
   cd ~/Downloads
   ```

3. Install it with apt, which brings in everything it needs:

   ```bash
   sudo apt install ./reoling_*_amd64.deb
   ```

   If the folder holds more than one Reoling .deb, the pattern matches all
   of them: type the file's full name instead.
4. Start Reoling from the applications menu. To see the installed version:

   ```bash
   reoling --version
   ```

## What it installs

The package itself:

- `/usr/bin/reoling`, the app.
- GStreamer's GTK 4 paintable sink, in `/usr/lib/reoling/gstreamer-1.0`.
  Ubuntu 24.04 has no `gstreamer1.0-gtk4` package, so the .deb carries the
  sink itself; `reoling` adds that directory to its plugin registry when it
  starts.
- The desktop entry and the icons, so Reoling appears in the applications
  menu.
- This documentation, in `/usr/share/doc/reoling`.

apt installs with it:

- the GTK 4 and GStreamer runtime;
- GStreamer's base, good and bad plugin sets: the parsers, the muxers, and
  the nvcodec and va hardware decoders;
- the libav software decoders;
- a PulseAudio, PipeWire or ALSA sound sink.

apt recommends, and so installs unless told not to:

- a VA-API driver (`va-driver-all`, or the one for your GPU) for hardware
  decoding, see [Hardware decoding](HARDWARE-DECODING.md);
- a keyring (GNOME Keyring or KWallet) for the device passwords.

## Updating

Download the newer .deb from the
[latest release](https://github.com/derSobi/Reoling/releases/latest) and
install it the same way; apt replaces the installed version:

```bash
sudo apt install ./reoling_<version>-1_amd64.deb
```

`<version>` stands for the version in the file's name: type the name up to
`reoling_` and press Tab to complete it.

If Reoling runs, quit it and start it again to run the new version. The
devices, the settings and the passwords stay.

## Removing

1. If Run at Startup is on (Client Settings, System Settings, tab General
   Settings), turn it off first, so that no autostart entry is left
   behind.
2. Remove the package:

   ```bash
   sudo apt remove reoling
   ```

This removes what the package installed. What Reoling keeps in your home
folder stays, for the .deb and the AppImages alike; delete it by hand if
you want it gone:

- `~/.config/reoling`: the devices and the settings;
- `~/.cache/reoling`: cached product pictures;
- the device passwords, in the desktop keyring;
- the snapshots, recordings and downloads, in `~/Pictures/Reoling` and
  `~/Videos/Reoling` or the folders set in Client Settings.

## Troubleshooting

- **Reoling is missing from the applications menu.** An AppImage ran on
  this machine before and its file is gone: its per-user menu entry hides
  the .deb's. Quit Reoling if it runs, then start `reoling` once from a
  terminal, and the .deb's entry takes over; see
  [Using it alongside the .deb](INSTALL-APPIMAGE.md#using-it-alongside-the-deb).
- **Black video, or no hardware decoding.** See
  [Hardware decoding](HARDWARE-DECODING.md).
