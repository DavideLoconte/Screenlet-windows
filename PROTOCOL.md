# Screenlet protocol, version 1

Normative spec for the link between a **computer** (Screenlet for macOS,
GNOME or Windows) and the **app** (Screenlet Companion for Android). It derives from Plus One's protocol version 3, without its camera,
microphone, speaker and PDF-signing messages. MUST, SHOULD and MAY are used
as in RFC 2119. All integers are big-endian unless marked `le`. `‖` is
concatenation. `hex()` is lowercase hex.

Roles: the app is the server (listens), the computer is the client (connects).

## Constants

| Name | Value |
|---|---|
| Protocol version | `1` |
| Bonjour service | `_screenlet._tcp` |
| Default TCP port | `47236` (the app MAY use another and announce it) |
| Max message, before pairing | 4096 bytes |
| Max message, after pairing | 16 MiB |
| App drops a silent paired computer after | 90 s |
| Computer pings during a display session | every 5 s |
| Max video bytes per packet | 16338 (a 16 KiB datagram); 1426 in small packets |
| Input repeat interval (app) | 0.1 s |
| Input silence timeout (computer) | 0.4 s, then all touches are lifted |
| Cable: app read size | 16 KiB |
| Cable: computer max write | 48 KiB |
| Cable: max frame `length` | 1 MiB |
| Wi-Fi Direct group owner address | `192.168.49.1` |

## 0. Paths

The computer picks the path by itself, in this order:

1. **Cable** (§4), whenever a tablet is plugged in.
2. **Network**: a paired tablet announced with Bonjour.
3. **Wi-Fi Direct**: when neither, the tablet runs a Wi-Fi Direct group of
   its own, and a computer that learned its credentials (§2.1) MAY join it
   as an ordinary Wi-Fi network and connect to the group owner's address.
   The app starts the group only while it is on no Wi-Fi network and has no
   cable: the group's radio time would slow a Wi-Fi network down.

## Implementation map

| Section | Screenlet-macOS | Screenlet-Companion (`app/src/main/java/com/davideloconte/screenlet/`) |
|---|---|---|
| Discovery, TLS, hello, pairing | `Sources/Screenlet/Companion.swift` | `CompanionService.kt`, `Pairing.kt`, `Identity.kt` |
| Display session, packets, bitrate | `Sources/Screenlet/Display.swift` | `DisplaySession.kt`, `Sealed.kt` |
| Encoding, Annex B | `Sources/Screenlet/Video.swift` | (decoding) `DisplaySession.kt` |
| INPUT | `Sources/Screenlet/Input.swift` | `DisplayInput.kt` |
| Cable framing | `Sources/Screenlet/Cable.swift`, `Sources/Native/Accessory.m` | `UsbTunnel.kt` |
| Test vectors | `Tests/ScreenletTests/ScreenletTests.swift` | |

## 1. Discovery

The app MUST announce `_screenlet._tcp` with TXT records:

| Key | Value |
|---|---|
| `id` | first 16 hex digits of the app certificate's fingerprint |
| `name` | human-readable tablet name |

If `id` matches a paired tablet, the computer MUST require that tablet's
certificate during TLS and MUST NOT connect with any other.

## 2. Link

- TLS 1.3. Each side presents a self-signed EC P-256 certificate. No CA.
- **Fingerprint** = `hex(SHA-256(certificate DER))`.
- The computer accepts the app's certificate only if its fingerprint is in
  the paired list, or after a successful pairing (§2.2).
- **Framing**, both directions: `length u32 | JSON object (length bytes, UTF-8)`.
  No message carries a payload: a receiver MUST close the link on a non-zero
  `"size"`.
- A receiver MUST close the link on a message over the size limit.

### 2.1 Hello

The computer sends first; the app replies.

```json
{"type": "hello", "name": "Davide's Mac", "version": 1, "paired": true, "pair": false}
{"type": "hello", "name": "Galaxy Tab S6 Lite", "version": 1, "paired": true, "at": "192.168.0.204"}
```

| Field | From | Type | Meaning |
|---|---|---|---|
| `name` | both | string | display name |
| `version` | both | int | MUST be `1`; on mismatch both sides close |
| `paired` | both | bool | this side knows the other's certificate |
| `pair` | computer | bool | user asked to pair with this tablet |
| `at` | app | string | the app's address as reached by the computer |
| `p2p` | app | object | `ssid`, `passphrase`, `at`: its Wi-Fi Direct group (§0). Only when both sides are paired; the computer stores it with the pairing |

### 2.2 Pairing

Runs if either `paired` is false and the computer sent `"pair": true`.
Otherwise an unpaired link MUST be closed.

| Step | Sender | Message |
|---|---|---|
| 1 | app | `{"type": "pair-commit", "commit": hex(SHA-256(appNonce ‖ appCertDER ‖ computerCertDER))}` |
| 2 | computer | `{"type": "pair-nonce", "nonce": hex(computerNonce)}` |
| 3 | app | `{"type": "pair-nonce", "nonce": hex(appNonce)}` |
| 4 | computer | verifies the commitment from step 1; on mismatch closes |
| 5 | both | display the code (below) |
| 6 | both | `{"type": "pair-ok"}` or `{"type": "pair-no"}` after the user answers |

Nonces are 16 random bytes. The app MUST send step 1 before receiving step 2.

**Code** = `uint32(SHA-256(lo ‖ hi ‖ computerNonce ‖ appNonce)[0..4]) mod 1000000`,
zero-padded to 6 digits, where `lo`/`hi` are the two certificate DERs
ordered bytewise.

If both sides send `pair-ok`, each stores the other's certificate. Otherwise
nothing is stored and the link closes.

### 2.3 Other messages

| Message | Meaning |
|---|---|
| `{"type": "ping"}` | keepalive |
| `{"type": "bye"}` | close the link |

A receiver MUST ignore a message of an unknown `type`.

## 3. Display session

Requested on a paired link.

```json
{"type": "display", "key": "<64 hex: AES-256 key>", "port": 50123, "codecs": ["hevc", "avc"]}
```

Reply, one of:

```json
{"type": "display-ready", "port": 41089, "width": 2000, "height": 1200, "refresh": 60.0, "codec": "hevc",
 "decoders": {"avc": 120, "hevc": 60, "vp9": 60}}
{"type": "refused", "reason": "background" | "codec"}
```

| Field | Rule |
|---|---|
| `key` | fresh random AES-256 key per session |
| `port` | the sender's UDP port; ignored over the cable |
| `width`, `height` | the tablet's screen in pixels, landscape (`width ≥ height`) |
| `refresh` | the fastest refresh rate the tablet's panel runs at during the session, in Hz. The computer SHOULD capture at it, capped at what it sustains |
| `codecs` | request: the codecs the computer encodes in hardware (§3.3), its preferred first. Software H.264 only if it has none |
| `codec` | reply: the codec of the session (below). The computer MUST encode it, and MUST close the link if it is not one it offered |
| `decoders` | reply, optional: the tablet's profile. For each codec it decodes in hardware, the frames per second its decoder sustains at `width`×`height` (as the device measured it, or as its maker declares). The computer MAY use it to choose an automatic resolution the decoder keeps up with |

**Codec negotiation.** The app picks the first codec in `codecs` it decodes
in hardware at its screen size; if there is none, the first it decodes in
software. If it decodes none of them it replies `refused` with reason
`"codec"`. Hardware on both sides comes first because a software encoder or
decoder costs more latency than any codec saves.

After `display-ready` the computer creates a display at the tablet's
proportions and streams it. The link stays open: the computer pings every
5 s; `{"type": "stop"}` from the computer or the link closing ends the session.
`{"type": "show"}` asks a tablet in pause (PING's `shown` is 0) to bring the
desktop back on screen.

### 3.1 Packet envelope

UDP datagrams (or cable DATAGRAM frames), AES-GCM with the session key:

```
nonce[12] = direction u32 ‖ count u64
datagram  = nonce ‖ ciphertext ‖ tag[16]
```

- `direction`: `1` computer → app, `2` app → computer.
- `count` starts at 0 per direction and increases by 1 per packet.
- The receiver MUST drop a packet that fails authentication, whose `count`
  it accepted already, or 64 or more below the highest it accepted (a small
  window, not strictly increasing: Wi-Fi reorders).
- The plaintext body starts with `kind u8`.

### 3.2 Packet kinds

| Kind | Name | Direction | Body after `kind` | Body size |
|---|---|---|---|---|
| 1 | VIDEO | computer → app | `flags u8` (bit 0 = keyframe), `frame u32`, `index u16`, `count u16`, `captured i64`, video bytes | 18 + ≤16338 |
| 2 | KEYFRAME | app → computer | none | 1 |
| 3 | PING | app → computer | `time i64`, `shown u8` | 10 (9 if `shown` is omitted) |
| 4 | PONG | computer → app | `pingTime i64`, `computerTime i64` | 17 |
| 5 | STATS | app → computer | `shown u32`, `lost u32`, then 5 stages × (median, p90, max) as `u16` in 0.1 ms units (§3.5) | 39 |
| 6 | INPUT | app → computer | `fingers u8`, per finger `id u8, x u16, y u16`; `samples u8`, per sample 9 bytes (§3.4) | variable |

The computer MUST ignore unknown kinds and bodies of the wrong size.

### 3.3 Video

- Codecs, by name:

| Name | Codec | Bitstream in VIDEO |
|---|---|---|
| `avc` | H.264, High profile or lower | Annex B; a keyframe carries SPS and PPS |
| `hevc` | HEVC Main | Annex B; a keyframe carries VPS, SPS and PPS |
| `vp8` | VP8 | one frame |
| `vp9` | VP9 profile 0 | one frame (a superframe if it has several) |
| `av1` | AV1 Main | one temporal unit, low-overhead OBU format with its sequence header on keyframes |

- No B-frames or other reordering: each frame decodes as soon as it arrives.
- Each frame is split into `count` packets, `index` from 0, of up to 16338
  video bytes: a 16 KiB datagram. Each packet costs the app a decryption,
  so few large ones are quicker; over UDP, IP fragments them, which loses no
  more frames than small packets (a packet lost loses its frame either way).
  The app MUST take datagrams of up to 64 KiB.
- If the app shows no frame while the computer sends (a network that drops
  IP fragments), the computer SHOULD send small packets for the rest of the
  session: 1426 video bytes, within one 1500-byte Ethernet MTU.
- `captured` is the computer's monotonic clock in ns at capture. The app
  aligns clocks with PING/PONG to measure delay.
- The app decodes a frame once all its packets have arrived. After a loss
  it sends KEYFRAME, repeating until a keyframe arrives. The computer MAY
  rate-limit keyframes (this app: at most one per 0.25 s).
- While PING's `shown` is 0 the computer SHOULD stop capturing. When `shown`
  returns to 1 it MUST send a keyframe.

### 3.4 Input

- Each INPUT is the full current state. The app repeats it every 0.1 s while
  anything touches the screen.
- A finger present in the previous INPUT and absent now has lifted.
- No INPUT for 0.4 s: the computer lifts everything.
- Finger `x`, `y`: 0–65535 across the screen.
- Pen sample (the S Pen HID report without its report ID), all `le`:

| Offset | Field | Type | Range |
|---|---|---|---|
| 0 | bits | u8 | `0x01` tip, `0x02` barrel button, `0x08` eraser, `0x10` in range |
| 1 | x | u16 le | 0–32767 |
| 3 | y | u16 le | 0–32767 |
| 5 | pressure | u16 le | 0–4096 |
| 7 | tilt x | s8 | degrees |
| 8 | tilt y | s8 | degrees |

The computer MUST reject an INPUT that is truncated or lists more than 10 fingers.

### 3.5 Latency, stage by stage

The app aligns the clocks with PING/PONG (the offset from the ping with the
shortest round trip, aged out after 10 s) and sends STATS each second for
the frames shown in it:

| Stage | From → to | What it shows |
|---|---|---|
| 0 | capture → decoded | the whole way (clocks aligned) |
| 1 | capture → first packet | the computer's capture, encode and send, and the network (clocks aligned) |
| 2 | first → last packet | the frame spread over the network |
| 3 | last packet → queued | waiting for a decoder input buffer |
| 4 | queued → decoded | the decoder |

The computer measures its own: capture → encoded → sent. Together they say
where each millisecond goes. A computer SHOULD also accept the 21-byte STATS
of earlier apps (capture→decoded and first packet→decoded, median, p90, max).

## 4. Cable (Android Open Accessory)

USB-C without tethering or developer options. The computer is the USB host.

1. The computer sends AOA control requests 51 (get protocol), 52 (send
   strings), 53 (start) with: manufacturer `Screenlet`, model `Screenlet`,
   description `Screenlet on the PC` (or the Mac), version `1`, URI
   `https://github.com/DavideLoconte/Screenlet-Companion`.
2. The tablet re-enumerates as an accessory (Google VID `0x18D1`, PID
   `0x2D00`), asks its user once, and opens the app.
3. One bulk endpoint each way carries frames:

```
frame = kind u8 ‖ connection u8 ‖ length u32 ‖ data[length]
```

| Kind | Name | Meaning |
|---|---|---|
| 0 | HELLO | computer: reset, all connections closed. App replies HELLO. The computer repeats HELLO until answered |
| 1 | OPEN | new TLS connection number `connection` |
| 2 | DATA | bytes for `connection` |
| 3 | CLOSE | `connection` ended |
| 4 | DATAGRAM | one §3.1 datagram, either direction |

- TLS, pairing and AES-GCM run end to end inside the frames. The cable is
  not trusted.
- The app reads in 16 KiB transfers. The computer writes at most 48 KiB per
  transfer and MUST follow a transfer that is an exact multiple of the
  endpoint's max packet size with a zero-length packet.
- A frame may span several transfers; receivers MUST buffer until `length`
  bytes have arrived.
- A sender MUST NOT send a frame with `length` over 1 MiB; it splits longer
  DATA into several frames. A receiver MAY drop the cable on a longer one.

## Test vectors

`vectors.json`, in every repository, holds the pairing code, the
commitment, a sealed packet, a cable frame and an INPUT body. Each
implementation's tests check them.
