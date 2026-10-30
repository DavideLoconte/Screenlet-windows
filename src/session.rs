//! A display session (PROTOCOL.md §3): the desktop to the tablet, its reports back.
use crate::cable::Cable;
use crate::display::{self, Monitor};
use crate::input::Input;
use crate::link::{Link, Result, Stream};
use crate::protocol::{self, Opener, Sealer, Stats, FROM_APP, FROM_PC, LARGE_PAYLOAD, PAYLOAD};
use crate::settings::Mode;
use crate::sizes::{self, Rate};
use crate::video::{self, Source, Video};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::json;
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::os::windows::io::AsRawSocket;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Networking::WinSock::{setsockopt, SOCKET, SOL_SOCKET, SO_SNDBUF};

/// Windows asks the user for the display driver once per run of the app, not per session.
static ASKED: AtomicBool = AtomicBool::new(false);
pub static STATS_EVERY: AtomicU32 = AtomicU32::new(30); // log one report in this many, and any with losses

#[derive(Clone)]
pub enum Path {
    /// A tablet on the network (or its Wi-Fi Direct group), and the id it announced.
    Network(SocketAddr, Option<String>),
    Cable(Arc<Cable>),
}

impl Path {
    pub fn stream(&self) -> Result<Box<dyn Stream>> {
        Ok(match self {
            Path::Network(address, _) => {
                let tcp = TcpStream::connect_timeout(address, Duration::from_secs(5))?;
                tcp.set_nodelay(true)?;
                tcp.set_read_timeout(Some(Duration::from_secs(1)))?;
                Box::new(tcp)
            }
            Path::Cable(cable) => Box::new(cable.connect()?) as Box<dyn Stream>,
        })
    }

    fn expect(&self) -> Option<&str> {
        match self {
            Path::Network(_, id) => id.as_deref(),
            Path::Cable(_) => None, // any paired tablet: TLS proves which
        }
    }
}

/// The display's datagrams, over UDP or the cable.
enum Datagrams {
    Udp(UdpSocket, SocketAddr),
    Cable(Arc<Cable>),
}

impl Datagrams {
    fn send(&self, packets: &[&[u8]], batch: &mut Vec<u8>) {
        match self {
            Datagrams::Udp(socket, to) => for packet in packets { let _ = socket.send_to(packet, to); },
            Datagrams::Cable(cable) => { let _ = cable.send_datagrams(packets.iter().copied(), batch); }
        }
    }
}

/// Pairs with the app on path (ask shows the code, returns the user's answer): its name.
pub fn pair(path: &Path, ask: &dyn Fn(&str, &str) -> bool) -> Result<String> {
    let mut link = Link::open(path.stream()?, None, Some(ask))?;
    let _ = link.send(&json!({"type": "bye"}));
    Ok(link.name)
}

/// What the window shows of a session.
#[derive(Default)]
pub struct Status {
    pub state: String,
    pub paused: bool,
    pub stats: String,
    pub screen: Option<(i32, i32)>, // the tablet's, for the window's resolutions
}

/// pattern: a moving test pattern instead of the desktop, with no input. ask: pairs first
/// with a tablet not paired yet, asking the user; without it, such a tablet ends here.
pub fn run(path: &Path, mode: &Mode, pattern: bool, ask: Option<&dyn Fn(&str, &str) -> bool>, status: &Arc<Mutex<Status>>, stop: &Arc<AtomicBool>, resume: &AtomicBool) -> Result<()> {
    let wifi = matches!(path, Path::Network(..));
    let mut link = Link::open(path.stream()?, path.expect(), ask)?;
    let tablet = link.name.clone();
    status.lock().unwrap().state = format!("Connecting to {tablet}…");
    let mut key = [0u8; 32];
    SystemRandom::new().fill(&mut key).map_err(|_| "no randomness")?;
    let socket = match path {
        Path::Network(..) => {
            let socket = UdpSocket::bind("0.0.0.0:0")?;
            // Room for a keyframe at the highest bitrate, no more: a deeper
            // queue only makes the next frames later when the network slows.
            unsafe { setsockopt(SOCKET(socket.as_raw_socket() as usize), SOL_SOCKET, SO_SNDBUF, Some(&(1i32 << 20).to_ne_bytes())); }
            Some(socket)
        }
        Path::Cable(_) => None,
    };
    // What this PC encodes in hardware, the preferred first.
    let mut offered = video::codecs(mode.priority == "clarity");
    if mode.codec != "auto" && offered.contains(&mode.codec) {
        offered.sort_by_key(|c| *c != mode.codec);
    }
    let port = socket.as_ref().map_or(9, |s| s.local_addr().map_or(9, |a| a.port()));
    let reply = link.request(&json!({"type": "display", "key": protocol::hex(&key), "port": port, "codecs": offered}))?;
    if reply["type"] == "refused" {
        return Err(format!("{tablet} refused: {}", reply["reason"].as_str().unwrap_or("?")).into());
    }
    if reply["type"] != "display-ready" {
        return Err(format!("the app answered {}", reply["type"]).into());
    }
    let codec = reply["codec"].as_str().filter(|c| offered.iter().any(|o| o == c))
        .ok_or_else(|| format!("the app chose {}, not one of {offered:?}", reply["codec"]))?.to_string();
    let screen = (reply["width"].as_i64().unwrap_or(1920) as i32, reply["height"].as_i64().unwrap_or(1200) as i32);
    if !sizes::valid(screen) {
        return Err(format!("{tablet} reports a screen of {}x{}", screen.0, screen.1).into());
    }
    status.lock().unwrap().screen = Some(screen);
    eprintln!("app display: {tablet} shows {}x{} at {} Hz, decodes {}", screen.0, screen.1, reply["refresh"], reply["decoders"]);
    let decoder = reply["decoders"][&codec].as_u64().map(|f| f as u32);
    let fps = if mode.fps > 0 { mode.fps } else { sizes::fps(reply["refresh"].as_f64(), decoder, !wifi) };
    let mut size = sizes::choose(&mode.resolution, screen, wifi);
    // Automatic: no more pixels a second than the tablet's decoder takes.
    if mode.resolution == "auto" {
        if let Some(d) = decoder.filter(|&d| d < fps) {
            let rate = screen.0 as i64 * screen.1 as i64 * d as i64;
            size = sizes::all(screen).into_iter().find(|s| s.0 as i64 * s.1 as i64 * fps as i64 <= rate).unwrap_or(size);
        }
    }
    let (preset_kbps, preset_vbv) = sizes::priority(&mode.priority);
    let target = if mode.kbps > 0 { mode.kbps } else { sizes::bitrate(preset_kbps, size) };
    let vbv = if mode.vbv_frames > 0.0 { mode.vbv_frames } else { preset_vbv };
    let mut rate = Rate::new(target, wifi);
    let datagrams = Arc::new(match (path, socket) {
        (Path::Network(address, _), Some(socket)) => Datagrams::Udp(socket, SocketAddr::new(address.ip(), reply["port"].as_u64().unwrap_or(0) as u16)),
        (Path::Cable(cable), _) => Datagrams::Cable(cable.clone()),
        _ => unreachable!(),
    });
    let sealer = Arc::new(Mutex::new(Sealer::new(&key, FROM_PC)));
    let waiting = Arc::new(AtomicUsize::new(0));
    let (frames_tx, frames_rx): (SyncSender<(Vec<u8>, bool, i64, i64)>, Receiver<_>) = mpsc::sync_channel(8);
    let w = waiting.clone();
    let skip = Box::new(move || w.load(Ordering::Relaxed) >= 2);
    let w = waiting.clone();
    let on_frame = Box::new(move |bytes: &[u8], keyframe: bool, captured: i64, _into: i64| {
        w.fetch_add(1, Ordering::Relaxed);
        let _ = frames_tx.send((bytes.to_vec(), keyframe, captured, video::now()));
    });
    // Extend: a monitor of the tablet's own, with nothing scaled. Without the driver, a mirror.
    if mode.extend && !pattern && !display::available() && !ASKED.swap(true, Ordering::Relaxed) {
        status.lock().unwrap().state = "Installing what Extend needs…".to_string();
        if let Err(e) = display::elevated("--install-driver") { eprintln!("app display: no driver: {e}"); }
    }
    let monitor = if mode.extend && !pattern && display::available() {
        Monitor::create(size, fps).inspect_err(|e| eprintln!("app display: a mirror instead: {e}")).ok()
    } else { None };
    let source = if pattern { Source::Pattern(size) } else {
        let mirrored = Some(mode.mirror.clone()).filter(|m| display::outputs().contains(m));
        Source::Screen(monitor.as_ref().map(|m| m.name.clone()).or(mirrored))
    };
    let fit = if monitor.is_some() { size } else { screen };
    let video = Arc::new(Video::start(source, &codec, fps, rate.kbps, vbv, fit, mode.keyframes, skip, on_frame)?);
    eprintln!("app display: {}x{}@{fps} {codec} ({}) {} kbit/s{}", video.size.0, video.size.1, video.name, rate.kbps,
              if wifi { " over Wi-Fi" } else { " over the cable" });

    status.lock().unwrap().state = format!("On {tablet}, {}x{}", video.size.0, video.size.1);
    let payload = Arc::new(AtomicUsize::new(if mode.packets == "small" { PAYLOAD } else { LARGE_PAYLOAD }));
    let fallback = wifi && mode.packets == "auto";
    let timings: Arc<Mutex<(Vec<f64>, Vec<f64>, usize)>> = Arc::default();
    let sender = {
        let (datagrams, sealer, payload, timings, waiting, stop) =
            (datagrams.clone(), sealer.clone(), payload.clone(), timings.clone(), waiting.clone(), stop.clone());
        std::thread::Builder::new().name("screenlet-send".into()).spawn(move || {
            let mut number = 0u32;
            let mut body = vec![0u8; protocol::VIDEO_HEADER + LARGE_PAYLOAD];
            let mut sealed = vec![0u8; protocol::MAX_PACKET];
            let mut all: Vec<u8> = Vec::new();
            let mut batch = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let Ok((bytes, keyframe, captured, encoded_at)) = frames_rx.recv_timeout(Duration::from_millis(200)) else { continue };
                waiting.fetch_sub(1, Ordering::Relaxed);
                number = number.wrapping_add(1);
                let unit = payload.load(Ordering::Relaxed);
                let count = bytes.len().div_ceil(unit).max(1);
                all.clear();
                let mut ends = Vec::with_capacity(count);
                {
                    let mut sealer = sealer.lock().unwrap();
                    for (index, chunk) in bytes.chunks(unit).enumerate() {
                        let header = protocol::video_header(keyframe, number, index as u16, count as u16, captured);
                        body[..protocol::VIDEO_HEADER].copy_from_slice(&header);
                        body[protocol::VIDEO_HEADER..protocol::VIDEO_HEADER + chunk.len()].copy_from_slice(chunk);
                        let n = sealer.seal(&body[..protocol::VIDEO_HEADER + chunk.len()], &mut sealed);
                        all.extend_from_slice(&sealed[..n]);
                        ends.push(all.len());
                    }
                }
                let mut from = 0;
                let packets: Vec<&[u8]> = ends.iter().map(|&end| { let packet = &all[from..end]; from = end; packet }).collect();
                datagrams.send(&packets, &mut batch);
                let now = video::now();
                let mut t = timings.lock().unwrap();
                t.0.push((encoded_at - captured) as f64 / 1e6);
                t.1.push((now - captured) as f64 / 1e6);
                t.2 += bytes.len();
            }
        })?
    };

    // The app's packets: from the UDP socket, or the cable's reading thread.
    let (from_app_tx, from_app) = mpsc::channel::<Vec<u8>>();
    match &*datagrams {
        Datagrams::Cable(cable) => {
            let tx = Mutex::new(from_app_tx);
            *cable.on_datagram.lock().unwrap() = Some(Box::new(move |d: &[u8]| { let _ = tx.lock().unwrap().send(d.to_vec()); }));
        }
        Datagrams::Udp(socket, to) => {
            let (socket, app, stop) = (socket.try_clone()?, to.ip(), stop.clone());
            socket.set_read_timeout(Some(Duration::from_millis(200)))?;
            std::thread::Builder::new().name("screenlet-udp-in".into()).spawn(move || {
                let mut buffer = [0u8; 2048];
                while !stop.load(Ordering::Relaxed) {
                    if let Ok((n, from)) = socket.recv_from(&mut buffer) {
                        if from.ip() == app && from_app_tx.send(buffer[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            })?;
        }
    }

    let receiver = {
        let (video, datagrams, stop, payload, timings, status) = (video.clone(), datagrams.clone(), stop.clone(), payload, timings, status.clone());
        let tablet = tablet.clone();
        std::thread::Builder::new().name("screenlet-feedback".into()).spawn(move || {
            // The monitor's own size: what is encoded may be smaller, and a touch lands on the monitor.
            let mut input = (!pattern).then(|| Input::new(video.screen, video.origin));
            let mut opener = Opener::new(&key, FROM_APP);
            let mut plain = [0u8; 2048];
            let mut last_keyframe = Instant::now() - Duration::from_secs(1);
            let mut shown = true;
            let mut reports = 0u32;
            let mut reported_at = Instant::now();
            let mut batch = Vec::new();
            let mut keyframe = |video: &Video| {
                if last_keyframe.elapsed() >= Duration::from_millis(250) {
                    last_keyframe = Instant::now();
                    video.keyframe();
                }
            };
            while !stop.load(Ordering::Relaxed) {
                let wait = if input.as_ref().is_some_and(|i| i.busy()) { 5 } else { 250 };
                let packet = match from_app.recv_timeout(Duration::from_millis(wait)) {
                    Ok(p) => Some(p),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                if let Some(input) = input.as_mut() { input.check(); }
                let Some(packet) = packet else { continue };
                let Some(body) = opener.open(&packet, &mut plain) else { continue };
                match body[0] {
                    protocol::INPUT => {
                        if let Some(input) = input.as_mut() {
                            if !input.packet(body) { eprintln!("app display: a malformed input packet"); }
                        }
                    }
                    protocol::KEYFRAME => keyframe(&video),
                    protocol::PING if body.len() == 9 || body.len() == 10 => {
                        let mut pong = [0u8; 17];
                        pong[0] = protocol::PONG;
                        pong[1..9].copy_from_slice(&body[1..9]);
                        pong[9..17].copy_from_slice(&video::now().to_be_bytes());
                        let mut out = [0u8; 64];
                        let n = sealer.lock().unwrap().seal(&pong, &mut out);
                        datagrams.send(&[&out[..n]], &mut batch);
                        if body.len() == 10 && (body[9] != 0) != shown {
                            shown = body[9] != 0;
                            video.pause(!shown);
                            if shown { keyframe(&video); } else if let Some(input) = input.as_mut() { input.close(); }
                            let mut s = status.lock().unwrap();
                            s.paused = !shown;
                            s.state = if shown { format!("On {tablet}") } else { format!("On {tablet}, paused") };
                        }
                    }
                    protocol::STATS => {
                        let Some(stats) = Stats::parse(body) else { continue };
                        let (encoded, sent, bytes) = std::mem::take(&mut *timings.lock().unwrap());
                        let median = |v: &[f64]| { let mut v = v.to_vec(); v.sort_by(f64::total_cmp); v.get(v.len() / 2).copied() };
                        let kbps = (bytes as f64 * 8.0 / reported_at.elapsed().as_secs_f64() / 1000.0) as u32;
                        reported_at = Instant::now();
                        let whole = stats.stages[0].unwrap_or_default();
                        // Frames went and none came out: a network that drops IP fragments. Small packets from now on.
                        if fallback && stats.shown == 0 && !sent.is_empty() && payload.load(Ordering::Relaxed) != PAYLOAD {
                            payload.store(PAYLOAD, Ordering::Relaxed);
                            eprintln!("app display: no frame arrives in large packets: small ones");
                            keyframe(&video);
                        }
                        if stats.shown > 0 {
                            if let Some(next) = rate.report(whole[0], stats.lost) {
                                video.rate(next);
                                eprintln!("app display: frames late ({:.0} ms): {next} kbit/s", whole[0]);
                            }
                        }
                        let ms = |v: Option<f64>| v.map_or("–".to_string(), |v| format!("{v:.1} ms"));
                        let stage = |i: usize| stats.stages[i].map(|s| s[0]);
                        let skipped = video.skipped.swap(0, Ordering::Relaxed);
                        let text = format!("Capture to decoded: {:.1} ms (90%: {:.1})\nEncoded at {}, sent at {}\nFirst packet {}, spread {}, decoder wait {}, decode {}\n{} fps, {} lost{}\n{:.1} of {:.1} Mbit/s",
                            whole[0], whole[1], ms(median(&encoded)), ms(median(&sent)), ms(stage(1)), ms(stage(2)), ms(stage(3)), ms(stage(4)), stats.shown, stats.lost,
                            if skipped > 0 { format!(", {skipped} not encoded") } else { String::new() }, kbps as f64 / 1000.0, rate.kbps as f64 / 1000.0);
                        reports += 1;
                        if stats.lost > 0 || reports % STATS_EVERY.load(Ordering::Relaxed) == 1 {
                            eprintln!("app display: {}", text.replace('\n', "; "));
                        }
                        status.lock().unwrap().stats = text;
                    }
                    _ => {}
                }
            }
            if let Some(input) = input.as_mut() { input.close(); }
        })?
    };

    let failure: Mutex<Option<String>> = Mutex::new(None);
    let result = link.hold(resume, || {
        if let Some(error) = video.error() {
            *failure.lock().unwrap() = Some(error);
            return true;
        }
        stop.load(Ordering::Relaxed) || matches!(&*datagrams, Datagrams::Cable(c) if !c.alive())
    });
    stop.store(true, Ordering::Relaxed);
    let _ = receiver.join();
    let _ = sender.join();
    if let Datagrams::Cable(cable) = &*datagrams {
        *cable.on_datagram.lock().unwrap() = None;
    }
    drop(video);
    drop(monitor);
    result?;
    match failure.into_inner().unwrap() {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}
