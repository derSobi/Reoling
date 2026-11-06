# Hardware decoding

Every package, the [.deb](INSTALL-DEB.md) and both
[AppImages](INSTALL-APPIMAGE.md), decodes video in hardware where it can,
and in software where it cannot. Hardware decoding needs a driver for
the GPU:

- **NVIDIA:** the NVIDIA driver, whose `libcuda` and `libnvcuvid` the NVDEC
  decoders (`nvh264dec`, `nvh265dec`) use.
- **Intel or AMD:** a VA-API driver, for the VA-API decoders (`vah264dec`,
  `vah265dec`). The .deb recommends one (`va-driver-all`, or the one for
  your GPU); the AppImages use the system's `libva` and its VA-API driver.

Where no driver is present, the software decoders installed take over
(FFmpeg's, through GStreamer's libav plugin; for H.264, OpenH264 as
well). Where a codec has no software decoder either, there is nothing to
fall back on: Reoling says so, see below.

## The system AppImage

The embed AppImage brings the GStreamer plugins that decode: libav in
software, nvcodec (NVDEC) and va (VA-API) for hardware. The system
AppImage brings none: like the .deb, it decodes with the distribution's
GStreamer plugins and the system's drivers.

| Distribution | NVDEC and VA-API plugins | Software decoders |
|---|---|---|
| Debian, Ubuntu | `gstreamer1.0-plugins-bad` | `gstreamer1.0-libav` |
| Fedora | `gstreamer1-plugins-bad-free` | `gstreamer1-plugin-openh264` and `openh264` (H.264 only) |
| openSUSE | `gstreamer-plugins-bad` | from Packman (`opi codecs`) |
| Arch Linux | `gst-plugins-bad-libs` (NVDEC), `gst-plugin-va` (VA-API) | `gst-libav` |

The install command for each is in
[What the system file needs](INSTALL-APPIMAGE.md#what-the-system-file-needs).

- **Ubuntu 26.04** keeps the VA-API plugin in `gstreamer1.0-plugins-extra`,
  which `gstreamer1.0-plugins-bad` depends on, so that command installs
  it as well.
- **Fedora** has no software H.265 decoder in its own repositories; RPM
  Fusion's FFmpeg brings one (`sudo dnf swap ffmpeg-free ffmpeg
  --allowerasing`, with `gstreamer1-plugin-libav`). Fedora's own Mesa
  leaves H.264 and H.265 out of its VA-API driver (Mesa's default build
  option, which Fedora's package keeps), so an AMD GPU decodes neither in
  hardware there. RPM Fusion has a VA-API driver with both,
  `mesa-va-drivers-freeworld` (see
  https://rpmfusion.org/Howto/Multimedia).
- **openSUSE** builds its own Mesa without H.264 and H.265 as well (its
  Mesa package's `video_codecs` setting). Where the GPU's driver lacks a
  codec, Auto takes a software decoder: from Packman, or for H.264 also
  OpenH264.

Where a codec has neither a hardware nor a software decoder, Reoling says
so at start and over the picture, with what to install (see
[When something is missing](INSTALL-APPIMAGE.md#when-something-is-missing)).

## How Auto picks a decoder

With Decoding set to **Auto (integrated GPU first)**, the default, Reoling
orders the hardware decoders itself:

1. those on the integrated GPU;
2. then those on a GPU whose kind it cannot tell;
3. then those on a dedicated card;
4. then the software decoders installed (FFmpeg's libav, OpenH264 for
   H.264), in GStreamer's own order.

**Auto (dedicated GPU first)** tries the card first, then a GPU of unknown
kind, then the integrated one, and the software decoders installed last.
With no software decoder for a codec, there is no fallback after the
hardware ones.

How Reoling tells the kinds of GPU apart:

- **dedicated:** switcheroo-control tags it so, or it is an Intel or
  NVIDIA GPU behind a PCI bridge;
- **integrated:** an Intel GPU at 00:02.0 on the root bus;
- **unknown kind:** any other GPU.

## Choosing the decoding

1. Click the Settings button at the right of the main window's header bar;
   the Client Settings window opens.
2. Go to System Settings, tab Live View & Playback, row Decoding.
3. Pick one:
   - **Auto (integrated GPU first)**, the default;
   - **Auto (dedicated GPU first)**;
   - **Hardware**, with a choice of device when there are several;
   - **Software**, software decoders only, for each codec that has one
     installed (see below for one that has none).

The choice applies to streams started afterwards: start the stream again
to see it.

Hardware on a machine where no hardware decoder is found decodes in
software instead of leaving the picture black, and the Decoding row says
so. Where one is found, Hardware uses the hardware decoders only (with a
device chosen, only that device's): a stream they cannot decode, such as
H.265 on a GPU that decodes only H.264, does not play. Auto or Software
plays it.

Software, for a codec with no software decoder installed (H.265 on
Fedora without RPM Fusion, for example), decodes that codec as Auto
does, and System Status says so: `H.265: no software decoder installed,
decoded as Auto`.

## Checking what is in use

System Status (Client Settings, System Settings, tab General Settings,
row System Status, View) lists:

- the hardware decoders found, with the kind of GPU where known;
- the decoding mode in use, and a codec Software decodes as Auto;
- the decoders the latest live and recorded streams started with;
- the GTK 4 sink that draws the picture (`Display: gtk4paintablesink
  <version> (<file>)`), and what this system lacks (`Missing: nothing`,
  or a line per feature with what installs it).

`--check-requirements` prints whether there are software and hardware
decoders for H.264 and H.265, without a window (see
[From a terminal](INSTALL-APPIMAGE.md#from-a-terminal)).

## Troubleshooting

- **System Status finds no hardware decoder.** Install the driver for the
  GPU, as listed at the top; for the system AppImage, also the
  distribution's VA-API and NVDEC plugins (see
  [The system AppImage](#the-system-appimage)). On the embed AppImage, a
  stderr line `reoling: this system has no libva; ...` means the system
  has no `libva`, and so no VA-API decoding.
- **"No H.265 decoder on this system." (or H.264) over the picture.**
  Neither a hardware nor a software decoder for that codec is installed:
  install what the message names, then start Reoling again. On Fedora and
  openSUSE, see [The system AppImage](#the-system-appimage).
- **The video stays black.** Check in System Status which decoder the
  stream started with. Where a software decoder for the stream's codec is
  installed, set Decoding to Software and start the stream again, to rule
  out the hardware decoders. Where none is, Software decodes that codec as
  Auto does, hardware included, and System Status says so (`decoded as
  Auto`).
- **The wrong GPU decodes.** Pick Auto (dedicated GPU first), or Hardware
  and the device, then start the stream again.
