<p align="center"><img src="ui/Assets/icon.png" width="128" alt="Screenlet icon"></p>

<h1 align="center">Screenlet for Windows</h1>

<p align="center">Your Android tablet as a second screen for a Windows PC,<br>
with touch and pen, over a USB-C cable or Wi-Fi.</p>

---

This is the Windows half of Screenlet. The tablet runs
[Screenlet Companion](https://github.com/DavideLoconte/Screenlet-companion). There is also
[Screenlet for macOS](https://github.com/DavideLoconte/Screenlet-macos) and
[Screenlet for GNOME](https://github.com/DavideLoconte/Screenlet-linux). They all speak the
[Screenlet protocol](PROTOCOL.md).

## Use it

1. Download the installer from [Releases](../../releases) and run it. Windows asks once
   about the display driver.
2. Plug the tablet in, or have it on the same Wi-Fi as the PC, and open Screenlet.
3. Click **Connect**. The first time, check that both screens show the same six digits.

**Disconnect** ends it. The tablet then asks to allow access to its files, as it does
whenever it is plugged in.

The window sets Extend or Mirror (and which monitor), the resolution, what to optimize
for and the connection. Under Advanced are refresh rate, codec, bitrate, largest frame,
packets, keyframes and the latency, live, stage by stage. Showing the desktop as soon as a
paired tablet is plugged in is an option.

## How it works

| Part | Implementation |
|---|---|
| Second monitor | [Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver) (MIT, signed), carried inside `screenlet.exe` |
| Capture and encoding | Desktop Duplication, Media Foundation: H.264 or HEVC on the GPU, software H.264 otherwise |
| Touch and pen | Windows' own synthetic pointers, with pressure, tilt and hover |
| Cable | Android Open Accessory over WinUSB |
| App | `screenlet.exe` (Rust), the icon by the clock and the engine; `ScreenletWindow.exe` (C#, WinUI 3), the window, a process of its own that exists only while open |

Only an administrator can create a virtual monitor or change a USB driver, so a small
service, `ScreenletDisplay`, does both on the app's behalf. The monitor exists for as long
as the app holds the service's pipe open, so it goes with the session however that ends.

### The cable

Windows lets a program reach a USB device only through WinUSB, and a tablet as an
accessory has no driver at all. The installer gives it WinUSB with an INF signed on the
PC by a certificate made for it; only its public part stays, and the driver the INF
names is Microsoft's.

With USB debugging on, the requests that make the tablet an accessory go through its
debugging interface, so `adb` must not be running (`adb kill-server`). With it off, the
service gives the tablet's files to WinUSB for those few seconds. An accessory has no
files to show anyway. When the session ends the service cycles the USB port, as if the
cable had been pulled out, and the tablet is back in Explorer.

UsbDk would avoid all of this, but it filters every USB device and its open issues
include PCs that no longer start.

## Latency

Measured on an Intel UHD 620, Windows 11, with a Galaxy Tab S6 Lite on the cable at
2000×1200 and 60 Hz: 24 ms from capture to decoded. This PC's part alone, at 1920×1200:

| Encoder | Into the encoder | Encoder | Capture to sent |
|---|---|---|---|
| Quick Sync H.264 | 1.8 ms | 11 ms | 13 ms |
| Quick Sync HEVC | 1.3 ms | 12–17 ms | 14–18 ms |

The encoder runs at its fastest setting except for Sharpest, where it takes the slowest
one that still keeps 60 Hz here. Intel's HEVC at its default misses 60 Hz.

```bash
screenlet --bench 10 [hevc]           # this PC's stages alone
screenlet --show-cable 20 [options]   # on the tablet, latency logged stage by stage
screenlet --show-wifi 20 [options]    # options: mirror h264 hevc latency clarity 60 120 small large pattern
screenlet --show-direct 20 [options]  # over the tablet's Wi-Fi Direct group
```

## Build

The engine needs Rust's `x86_64-pc-windows-gnu` target and MinGW, the window .NET 10, the
installer Inno Setup.

```bash
cargo +stable-x86_64-pc-windows-gnu test --target x86_64-pc-windows-gnu
cargo +stable-x86_64-pc-windows-gnu build --release --target x86_64-pc-windows-gnu
dotnet publish ui/Screenlet.csproj -c Release -o dist/window
```

Copy `screenlet.exe` into `dist`, then `iscc installer/screenlet.iss`. `tools/dev.sh cargo
build --release` cross-compiles the engine from Linux.

CI tests and builds every push. A `v*` tag publishes the installer in a release.

## State

Tried on a Galaxy Tab S6 Lite and an Intel UHD 620: pairing, Extend, Mirror, both codecs,
Wi-Fi, the cable with USB debugging on and off, fingers, the pen, the window.

Not tried yet: running the installer, the tablet's side of Wi-Fi Direct, a discrete GPU.

Not there yet: 2× scaling on tablets 2560 pixels wide or more, as the other two do it
(Windows has no documented call for it).

## License

MIT. Screenlet started as a fork of
[Plus One](https://github.com/GabrieleSantangelo/PlusOne) by Gabriele Santangelo.

## AI disclosure

Much of this code was written by Claude, Anthropic's AI model, through Claude Code, under
the author's direction. The author chose what to build and tested it on real hardware.
