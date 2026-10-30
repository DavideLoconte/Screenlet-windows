# Screenlet for Windows

Your Android tablet as a second screen for a Windows PC, with touch and pen, over
a USB-C cable or Wi-Fi, with the lowest latency it can get. Works with Screenlet
Companion on the tablet.

A fork of Plus One: see LICENSE.

## Use it

1. Run the installer. It asks for the display driver, and installs what the cable
   needs.
2. Plug the tablet in, or have it on the same Wi-Fi as this PC, and open Screenlet.
3. Click **Connect**. The first time, check that both screens show the same six digits.

**Disconnect** ends it, and the icon by the clock does the same. In the settings,
*Show the desktop when the tablet is plugged in* skips the click for a paired tablet.

The window has the same settings as Screenlet for macOS and for GNOME: Extend or
Mirror (one of this PC's monitors), Resolution, Optimize for, the connection
(automatic, Wi-Fi, cable), and, under Advanced, refresh rate, codec, bitrate,
largest frame, packets and keyframes. The latency is shown live, stage by stage.

## The cable

Windows reaches a USB device only through WinUSB, its own driver. The installer gives the
tablet as an accessory to WinUSB, with an INF signed on this PC by a certificate made for
it: only the public part stays, in this PC's trusted stores. No driver of ours runs: the
INF names Microsoft's.

The rest is the `ScreenletDisplay` service's, with no prompt and no developer options:

- **Starting.** With USB debugging on, the requests that make the tablet an accessory go
  through its debugging interface, and `adb` must not be running (`adb kill-server`:
  WinUSB takes one program at a time). With it off, the service gives the tablet's files
  function to WinUSB for those few seconds. An accessory has no files to show anyway.
- **Ending.** When the session ends, however it does, the service cycles the tablet's USB
  port, as if the cable had been pulled out: Android leaves accessory mode and the tablet
  is back in Explorer, in a few seconds. As after any plugging in, Android then asks on the
  tablet whether to allow access to its files. The app then waits for the cable to be
  really unplugged before plugging it in shows the desktop again.

UsbDk would need none of this, and the code would be Linux's, but it is a filter on every
USB device and its open issues include Windows 11 PCs that no longer start.

## How it is built

- `screenlet.exe` (Rust, `src/`): the engine and the icon by the clock. Desktop
  Duplication, Media Foundation, the protocol (PROTOCOL.md), touch and pen
  injection, WinUSB. Cross-compiled from Linux: `tools/dev.sh cargo build --release`.
  On Windows, with MinGW's `bin` on the path:
  `cargo +stable-x86_64-pc-windows-gnu build --release --target x86_64-pc-windows-gnu`.
- `window\ScreenletWindow.exe` (C#, WinUI 3, `ui/`): the window, a process of its
  own that talks JSON lines with the engine and exists only while open. Built on
  Windows with .NET 10: `dotnet publish ui\Screenlet.csproj -c Release -o dist\window`.
- `installer\screenlet.iss`: the installer, with Inno Setup, from `dist\`.

Extend uses [Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver)
(MIT, signed through SignPath), inside `screenlet.exe`, with its licence in `driver/`.
Only an administrator may make the device it runs on, so the `ScreenletDisplay` service,
running as the system from Program Files, holds that device for as long as the app holds
its pipe open. The monitor goes with the session, however it ends.

## State

Tried on a real Galaxy Tab S6 Lite and an Intel UHD 620 on Windows 11: pairing, Extend,
Mirror, H.264 and HEVC in hardware, Wi-Fi, the cable, the desktop shown on plugging in,
fingers, the pen with pressure, the window, the installer's build, the cable with USB
debugging off, the tablet going back to MTP after a session.

Written, not yet tried: running the installer itself, Wi-Fi Direct, the pointer on
a discrete GPU, plugging in again after Disconnect.

Not there yet: HiDPI on a tablet 2560 or more wide, which the others set to 2×
(Windows has no documented call for it), the fidelity measure of `measure`.

```bash
screenlet --show-cable 20 [options]  # on the tablet, latency logged stage by stage
screenlet --show-wifi 20 [options]   # options: mirror h264 hevc latency clarity 60 120 small large pattern
screenlet --bench 10                 # this PC's stages alone
```
