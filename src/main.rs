//! Screenlet for Windows.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod cable;
mod display;
mod input;
mod link;
mod nearby;
mod protocol;
mod session;
mod settings;
mod sizes;
mod ui;
mod video;

use session::{Path, Status};
use settings::Mode;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

const USAGE: &str = "screenlet                         the window, and the icon by the clock
screenlet --background            only the icon
screenlet --install-driver        what Extend needs (as administrator)
screenlet --install-cable         what the cable needs (as administrator)
screenlet --uninstall-driver      all of these away again
screenlet --tablets               the tablets on the network and on the cable
screenlet --pair ADDRESS          pair with the tablet at ADDRESS, comparing the code
screenlet --pair-cable | --pair-wifi      pair, comparing the code here and on the tablet
screenlet --show ADDRESS SECONDS  the desktop on the tablet, for a while
screenlet --show-cable SECONDS [OPTIONS]  the desktop on the tablet on the cable, for a while,
screenlet --show-wifi SECONDS [OPTIONS]   or on the first paired tablet on the network,
screenlet --show-direct SECONDS [OPTIONS] or on a paired tablet's own Wi-Fi Direct group,
                                  with the latency logged stage by stage every 2 s
    options: mirror, h264 or hevc, latency, balanced or clarity, 30, 60, 90 or 120 (Hz;
             the tablet's otherwise), native, WxH, small or large (packets),
             pattern (a test pattern instead of the desktop), serial=SERIAL (which
             tablet on the cable, as --tablets lists them; the first otherwise)
screenlet --bench SECONDS [hevc]  this PC's part alone: the test pattern encoded, sealed
                                  and sent to itself, its stages measured";

fn address(text: &str) -> link::Result<SocketAddr> {
    let text = if text.contains(':') { text.to_string() } else { format!("{text}:{}", protocol::PORT) };
    text.to_socket_addrs()?.next().ok_or_else(|| "no such address".into())
}

fn main() {
    // Desktop Duplication and pointer injection both in the monitor's real pixels.
    unsafe { let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); }
    let args: Vec<String> = std::env::args().collect();
    let first = args.get(1).map(String::as_str);
    if !matches!(first, None | Some("--background" | "--tablet")) {
        unsafe { let _ = AttachConsole(ATTACH_PARENT_PROCESS); } // a program without a window has no console of its own
    }
    let result = match (first, args.get(2)) {
        (None, _) | (Some("--background"), _) | (Some("--tablet"), _) => {
            // --tablet: for a network that hides the tablet's announcements.
            let manual = (first == Some("--tablet")).then(|| args.get(2).and_then(|a| address(a).ok())).flatten();
            ui::run(first == Some("--background"), manual);
            return;
        }
        (Some("--tablets"), _) => tablets(),
        (Some("--install-driver"), _) => display::logged(display::install()),
        (Some("--install-cable"), _) => display::logged(cable::install()),
        (Some("--display-service"), _) => display::service(),
        (Some("--uninstall-driver"), _) => display::uninstall().and(cable::uninstall()),
        (Some("--pair"), Some(to)) => address(to).and_then(|a| pair(Path::Network(a, None))),
        (Some("--show"), Some(to)) => address(to).and_then(|a| show(Path::Network(a, None), args.get(3).and_then(|s| s.parse().ok()).unwrap_or(20), Mode::load(), false)),
        (Some("--pair-cable"), _) => cable::Cable::open(None).and_then(|c| pair(Path::Cable(c))),
        (Some("--pair-wifi"), _) => any_tablet(false).and_then(pair),
        (Some("--show-cable" | "--show-wifi" | "--show-direct"), _) => measure(&args),
        (Some("--bench"), _) => bench(args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10), args.get(3).is_some_and(|c| c == "hevc")),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("screenlet: {e}");
        std::process::exit(1);
    }
}

fn tablets() -> link::Result<()> {
    let found = nearby::browse();
    std::thread::sleep(Duration::from_secs(5));
    for tablet in found.lock().unwrap().values() {
        println!("{} at {} ({})", tablet.name, tablet.address, if link::paired::by_id(&tablet.id).is_some() { "paired" } else { "not paired" });
    }
    for plug in cable::plugged() {
        println!("{} on the cable, serial={}{}", plug.name, plug.serial, if plug.open { "" } else { " (nothing here can reach it)" });
    }
    Ok(())
}

/// The first tablet on the network, or the first paired one, within 10 s.
fn any_tablet(paired: bool) -> link::Result<Path> {
    let found = nearby::browse();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(t) = found.lock().unwrap().values().find(|t| !paired || link::paired::by_id(&t.id).is_some()) {
            return Ok(Path::Network(t.address, paired.then(|| t.id.clone())));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("no tablet on the network".into())
}

fn pair(path: Path) -> link::Result<()> {
    let ask = |name: &str, code: &str| {
        println!("Pair with {name} only if it shows {} {}. Pair? [y/N]", &code[..3], &code[3..]);
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).is_ok() && answer.trim().eq_ignore_ascii_case("y")
    };
    println!("Paired with {}", session::pair(&path, &ask)?);
    Ok(())
}

fn show(path: Path, seconds: u64, mode: Mode, pattern: bool) -> link::Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let timer = stop.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(seconds));
        timer.store(true, Ordering::Relaxed);
    });
    let status: Arc<Mutex<Status>> = Arc::default();
    session::run(&path, &mode, pattern, None, &status, &stop, &AtomicBool::new(false))
}

/// The Wi-Fi Direct group of the first paired tablet that told this PC about one.
fn direct() -> link::Result<Path> {
    for (fingerprint, entry) in link::paired::all() {
        let (Some(ssid), Some(passphrase)) = (entry["p2p"]["ssid"].as_str(), entry["p2p"]["passphrase"].as_str()) else { continue };
        let address = nearby::reach(ssid, passphrase, entry["p2p"]["at"].as_str().unwrap_or("192.168.49.1"))?;
        return Ok(Path::Network(address, Some(fingerprint.chars().take(16).collect())));
    }
    Err("no paired tablet told this PC its Wi-Fi Direct group yet: connect to it once by Wi-Fi or the cable".into())
}

fn measure(args: &[String]) -> link::Result<()> {
    let mut mode = Mode::load();
    let mut pattern = false;
    let mut serial = None;
    for word in args.iter().skip(3) {
        match word.as_str() {
            on if on.starts_with("serial=") => serial = Some(on["serial=".len()..].to_uppercase()),
            "mirror" => mode.extend = false,
            "h264" => mode.codec = "avc".into(),
            "hevc" => mode.codec = "hevc".into(),
            "latency" | "balanced" | "clarity" => mode.priority = word.clone(),
            "30" | "60" | "90" | "120" => mode.fps = word.parse()?,
            "native" => mode.resolution = "tablet".into(),
            "small" | "large" => mode.packets = word.clone(),
            "pattern" => pattern = true,
            other => mode.resolution = other.into(),
        }
    }
    session::STATS_EVERY.store(2, Ordering::Relaxed);
    let path = match args[1].as_str() {
        "--show-cable" => Path::Cable(cable::Cable::open(serial.as_deref())?),
        "--show-direct" => direct()?,
        _ => any_tablet(true)?,
    };
    show(path, args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20), mode, pattern)
}

/// This PC's stages alone, with no tablet: the test pattern at 1920x1200@60 through the
/// encoder, packetized, sealed and sent to a socket here, where they are opened and put
/// together again. Reports each stage's median.
fn bench(seconds: u64, hevc: bool) -> link::Result<()> {
    use protocol::*;
    let socket = std::net::UdpSocket::bind("127.0.0.1:0")?;
    let to = socket.local_addr()?;
    let out = std::net::UdpSocket::bind("127.0.0.1:0")?;
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    let key = [7u8; 32];
    let converted: Arc<Mutex<Vec<f64>>> = Arc::default();
    let encoded: Arc<Mutex<Vec<f64>>> = Arc::default();
    let sent: Arc<Mutex<Vec<f64>>> = Arc::default();
    let (c, e, s) = (converted.clone(), encoded.clone(), sent.clone());
    let mut sealer = Sealer::new(&key, FROM_PC);
    let mut number = 0u32;
    let mut body = vec![0u8; VIDEO_HEADER + LARGE_PAYLOAD];
    let mut packet = vec![0u8; MAX_PACKET];
    let size = (1920, 1200);
    let codec = video::codecs(hevc).remove(0);
    let video = video::Video::start(video::Source::Pattern(size), &codec, 60, 20000, 4.0, 0, size, video::KEYFRAME_SECONDS, Box::new(|| false),
        Box::new(move |bytes, keyframe, captured, into| {
            c.lock().unwrap().push((into - captured) as f64 / 1e6);
            e.lock().unwrap().push((video::now() - into) as f64 / 1e6);
            number += 1;
            let count = bytes.len().div_ceil(LARGE_PAYLOAD);
            for (i, chunk) in bytes.chunks(LARGE_PAYLOAD).enumerate() {
                body[..VIDEO_HEADER].copy_from_slice(&video_header(keyframe, number, i as u16, count as u16, captured));
                body[VIDEO_HEADER..VIDEO_HEADER + chunk.len()].copy_from_slice(chunk);
                let n = sealer.seal(&body[..VIDEO_HEADER + chunk.len()], &mut packet);
                let _ = out.send_to(&packet[..n], to);
            }
            s.lock().unwrap().push((video::now() - captured) as f64 / 1e6);
        }))?;
    println!("bench: {} ({codec}) at {}x{}@60 for {seconds} s", video.name, video.size.0, video.size.1);
    let mut opener = Opener::new(&key, FROM_PC);
    let (mut buffer, mut plain) = (vec![0u8; 65536], vec![0u8; 65536]);
    let mut whole = Vec::new(); // capture -> last packet opened here
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        let Ok(n) = socket.recv(&mut buffer) else { continue };
        let Some(b) = opener.open(&buffer[..n], &mut plain) else { continue };
        let (index, count) = (u16::from_be_bytes([b[6], b[7]]), u16::from_be_bytes([b[8], b[9]]));
        if index + 1 == count {
            let captured = i64::from_be_bytes(b[10..18].try_into()?);
            whole.push((video::now() - captured) as f64 / 1e6);
        }
    }
    if let Some(error) = video.error() { return Err(error.into()); }
    drop(video);
    let median =|v: &mut Vec<f64>| { v.sort_by(f64::total_cmp); v.get(v.len() / 2).copied().unwrap_or(f64::NAN) };
    let p90 = |v: &Vec<f64>| v.get(v.len() * 9 / 10).copied().unwrap_or(f64::NAN);
    let (mut converted, mut encoded, mut sent) =
        (converted.lock().unwrap().clone(), encoded.lock().unwrap().clone(), sent.lock().unwrap().clone());
    println!("frames: {}", encoded.len());
    println!("capture -> into encoder:   {:.2} ms (90%: {:.2})", median(&mut converted), p90(&converted));
    println!("encoder:                   {:.2} ms (90%: {:.2})", median(&mut encoded), p90(&encoded));
    println!("capture -> sent:           {:.2} ms (90%: {:.2})", median(&mut sent), p90(&sent));
    println!("capture -> opened here:    {:.2} ms (90%: {:.2})", median(&mut whole), p90(&whole));
    Ok(())
}
