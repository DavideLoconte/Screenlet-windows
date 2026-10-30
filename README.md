# Screenlet for Windows

Your Android tablet as a second screen for a Windows PC, with the lowest latency
it can get. Works with Screenlet Companion on the tablet.

A fork of Plus One: see LICENSE.

## How it is built

- `screenlet.exe` (Rust, `src/`): the engine and the icon by the clock. Desktop
  Duplication, Media Foundation, the protocol (PROTOCOL.md), touch and pen
  injection. Cross-compiled from Linux: `tools/dev.sh cargo build --release`.
- `window\ScreenletWindow.exe` (C#, WinUI 3, `ui/`): the window, a process of its
  own that talks JSON lines with the engine and exists only while open. Built on
  Windows: `dotnet publish ui\Screenlet.csproj -c Release -o dist\window`.
- `installer\screenlet.iss`: the installer, with Inno Setup.

## State

Works, tried on a real tablet from a virtual machine: pairing, mirror over Wi-Fi,
touch, the window, the icon, Resume, software H.264.

Written, not yet tried, because a virtual machine has no GPU: hardware H.264, GPU
conversion and scaling, the pointer drawn into the frames. Each falls back to the
software path. Discovery on the network too: the machine's NAT hides it.

Extend uses [Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver)
(MIT, signed through SignPath), included in `driver/` with its licence.

Not there yet: the USB cable, HEVC, the choice of monitor in Mirror.
