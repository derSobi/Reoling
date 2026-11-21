# The Diagnostic Log

When a device does not connect, refuses live view or shows no picture, I usually cannot reproduce it: I only have my own devices. The Diagnostic Log lets Reoling write down what happened on your machine, in a file you can attach to an issue. It is available since Reoling 1.0.6.

The log is off unless you turn it on, and it turns itself off when Reoling is closed.

## Turning it on

There are two ways. Which one to take depends on when the problem happens.

In the app: click the Settings button in the header bar, and in the Client Settings window open System Settings, General. Turn on "Diagnostic Log". From that moment Reoling writes the log, until you turn the switch off or close Reoling. This is enough for a problem you can repeat while the app runs, such as a refused live view or a black picture.

From the start: run Reoling with `--diagnostics`. Take this way when a device does not connect, because the devices connect right at start, before you could reach the switch.

```bash
reoling --diagnostics
```

With an AppImage, add the option to the file you start:

```bash
./Reoling-1.0.6-embed-x86_64.AppImage --diagnostics
```

Started this way, the switch in the settings is on already. If Reoling is running, close it first: a second start only brings the running window to the front.

## Repeating the problem

With the log on, do what goes wrong, once, and wait a few seconds after it.

- A device stays on "Not connected": start with `--diagnostics`, wait until its card says "Not connected", then click the card once so that it tries again.
- A device connects but refuses live view: click it so that the refusal happens. Then open the device's menu in the device list and choose "Try other live view requests…". Reoling tries the other forms of the request it knows, one after the other. Wait until the picture appears or the message over it says that the camera accepted none of them. The entry is there only while the log is on and after a refusal.
- A device connects but the picture stays black, freezes or stutters: start the stream with the log on and let it run for at least ten seconds. The video part of the log follows a stream from its start, so for a stream that was running before you turned the switch on, press Stop and then Play.

## Saving the file

Under the switch, the row "Diagnostic Log File" shows where the file is. "Save As…" saves a copy wherever you like, and the folder button opens the folder that holds it. The row stays while a log file exists, also after the switch is off again.

The file is `~/.local/state/reoling/reoling-diagnostic.log`, or `reoling/reoling-diagnostic.log` under `$XDG_STATE_HOME` where that variable is set. Each time the log is turned on, the log that was there is kept beside it as `reoling-diagnostic.1.log` and a new one starts. So save your copy before you turn the log on again or start with `--diagnostics` a second time. A log that grows past 5 MB is moved aside the same way.

## What the log contains

Every line starts with the time in seconds since Reoling started. `[dev 2]` is the device's place in the device list, and `[conn 7]` or `[race 3]` number one connection or one connection attempt, so the lines that belong together can be found.

- How each device connects: the paths tried (the P2P session, the local network), which one won, how long it took, and the reason when none did.
- The login: which steps were made and the device's answer code. Never the user name or the password.
- What the device says about itself: its model, the streams it offers and what its cameras can do.
- Live view: each request Reoling sent, and the device's answer code. During "Try other live view requests…", each form tried and its result.
- Replies Reoling could not read, in a shortened and masked form.
- Video: the distribution, the versions of GTK and GStreamer, the decoders installed, the decoder chosen for the stream, the picture's format and size, GStreamer's errors and warnings, and how many frames arrived, were decoded and reached the display.

A few lines from a camera that connects on the local network and shows its picture:

```text
+0.238 [dev 4] NEGOTIATION device: added by UID
+0.279 [dev 4] [race 1 +40 ms] NEGOTIATION race: a device answered the LAN discovery, its legs join the race: LanUdp, LanTcp
+0.353 [dev 4] [race 1 +114 ms] NEGOTIATION race: won by LanTcp ([conn 7])
+0.393 [dev 4] [conn 7 +39 ms] DEBUG identity model: RLC-520A
+0.425 [dev 4] [conn 7 +71 ms] NEGOTIATION preview reply: response_code=200 class=0x0 msg_id=3 body_len=138
+0.613 [app] VIDEO decoder: vah264dec (Codec/Decoder/Video/Hardware)
+0.618 [app] VIDEO decoded picture: NV12 640x480
+5.591 [app] VIDEO after 5 s: 51 frame(s) received, 42 decoded, 42 reached the display sink
```

## What is masked, and what is never written

Reoling masks a line before it writes it. In place of a value you find a word in angle brackets:

| In the log | What stood there |
|---|---|
| `<uid>` | a camera's or device's UID |
| `<mac>` | a MAC address |
| `<ipv4 private>`, `<ipv4 public>`, `<ipv6 …>` | an IP address, with its kind only |
| `<email>` | an e-mail address |
| `<name>` | the name of a device or camera, or a user name, that Reoling knows |

Text that comes from a device enters a line only in forms known to be safe: names of fields, numbers and fixed words, version numbers, and the model as a short word. Anything else is written as a placeholder or as its length, such as `<5 bytes>`. A line that Reoling cannot make safe is replaced by `NEGOTIATION line withheld: it could not be masked safely`.

Passwords, login nonces and encryption keys are never written, masked or not. The log holds no picture and no sound.

In the video lines, your home directory is written as `~` and your user name in other paths as `<user>`.

The mask recognises the names Reoling knows. A name it has never seen, for example one that only appears inside a device's text, cannot be recognised as a name. So please read through the file before you post it. It is plain text, and a search for your camera names and your public IP address takes a minute.

## Attaching it to an issue

Open an issue at <https://github.com/derSobi/Reoling/issues>, or add to the one that describes your problem, and drag the saved file into the text box. GitHub accepts `.log` files.

Beside the file, these help me most:

- The Reoling version and the package: the .deb, the embed AppImage or the system AppImage.
- The device: model and firmware version, whether it is a camera on its own or behind an NVR or Home Hub, and whether you added it by UID or by IP address.
- Whether the device is on the same network as the computer.
- What you did and what you saw, in a sentence or two, and whether the official Reolink app shows the same device from the same computer or phone.

For a missing or black picture, the output of this command also helps. It lists what your system has for video and needs no window:

```bash
reoling --check-requirements
```
