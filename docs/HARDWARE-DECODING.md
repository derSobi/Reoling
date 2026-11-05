# Hardware decoding

Both packages, the [.deb](INSTALL-DEB.md) and the
[AppImage](INSTALL-APPIMAGE.md), decode video in hardware where they can,
and in software where they cannot. Hardware decoding needs a driver for
the GPU:

- **NVIDIA:** the NVIDIA driver, whose `libcuda` and `libnvcuvid` the NVDEC
  decoders (`nvh264dec`, `nvh265dec`) use.
- **Intel or AMD:** a VA-API driver, for the VA-API decoders (`vah264dec`,
  `vah265dec`). The .deb recommends one (`va-driver-all`, or the one for
  your GPU); the AppImage uses the system's `libva` and its VA-API driver.

Where no driver is present, the libav software decoders take over.

## How Auto picks a decoder

With Decoding set to **Auto (integrated GPU first)**, the default, Reoling
orders the hardware decoders itself:

1. those on the integrated GPU;
2. then those on a GPU whose kind it cannot tell;
3. then those on a dedicated card;
4. then the libav software decoders.

**Auto (dedicated GPU first)** tries the card first, then a GPU of unknown
kind, then the integrated one, and the libav software decoders last.

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
   - **Software**, software decoders only.

The choice applies to streams started afterwards: start the stream again
to see it.

Hardware on a machine where no hardware decoder is found decodes in
software instead of leaving the picture black, and the Decoding row says
so. Where one is found, Hardware uses the hardware decoders only (with a
device chosen, only that device's): a stream they cannot decode, such as
H.265 on a GPU that decodes only H.264, does not play. Auto or Software
plays it.

## Checking what is in use

System Status (Client Settings, System Settings, tab General Settings,
row System Status, View) lists:

- the hardware decoders found, with the kind of GPU where known;
- the decoding mode in use;
- the decoders the latest live and recorded streams started with.

## Troubleshooting

- **System Status finds no hardware decoder.** Install the driver for the
  GPU, as listed at the top. On the AppImage, a stderr line
  `reoling: this system has no libva; ...` means the system has no
  `libva`, and so no VA-API decoding.
- **The video stays black.** Check in System Status which decoder the
  stream started with. Set Decoding to Software and start the stream
  again, to rule out the hardware decoders.
- **The wrong GPU decodes.** Pick Auto (dedicated GPU first), or Hardware
  and the device, then start the stream again.
