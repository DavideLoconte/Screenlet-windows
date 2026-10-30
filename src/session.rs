//! A display session (PROTOCOL.md §3): the desktop to the tablet, its reports back.
use crate::display::{self, Monitor};
use crate::input::Input;
use crate::link::{Link, Result, Stream};
use crate::protocol::{self, Opener, Sealer, Stats, FROM_APP, FROM_PC, LARGE_PAYLOAD, PAYLOAD};
use crate::settings::Mode;
use crate::sizes::{self, Rate};
use crate::video::{self, Video};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::json;
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn stream(address: &SocketAddr) -> Result<Box<dyn Stream>> {
    let tcp = TcpStream::connect_timeout(address, Duration::from_secs(5))?;
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(Duration::from_secs(1)))?;
    Ok(Box::new(tcp))
}

/// Pairs with the app (ask shows the code, returns the user's answer): its name.
pub fn pair(address: &SocketAddr, ask: &dyn Fn(&str, &str) -> bool) -> Result<String> {
    let mut link = Link::open(stream(address)?, None, Some(ask))?;
    let _ = link.send(&json!({"type": "bye"}));
    Ok(link.name)
}

/// What the window shows of a session.
#[derive(Default)]
pub struct Status {
    pub state: String,
    pub paused: bool,
    pub stats: String,
}

pub fn run(address: &SocketAddr, mode: &Mode, status: &Arc<Mutex<Status>>, stop: &Arc<AtomicBool>, resume: &AtomicBool) -> Result<()> {
    let mut link = Link::open(stream(address)?, None, None)?;
    let tablet = link.name.clone();
    status.lock().unwrap().state = format!("Connecting to {tablet}…");
    let mut key = [0u8; 32];
    SystemRandom::new().fill(&mut key).map_err(|_| "no randomness")?;
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    let offered = ["avc"];
    let reply = link.request(&json!({"type": "display", "key": protocol::hex(&key), "port": socket.local_addr()?.port(), "codecs": offered}))?;
    if reply["type"] == "refused" {
        return Err(format!("{tablet} refused: {}", reply["reason"].as_str().unwrap_or("?")).into());
    }
    if reply["type"] != "display-ready" {
        return Err(format!("the app answered {}", reply["type"]).into());
    }
    let screen = (reply["width"].as_i64().unwrap_or(1920) as i32, reply["height"].as_i64().unwrap_or(1200) as i32);
    if !sizes::valid(screen) {
        return Err(format!("{tablet} reports a screen of {}x{}", screen.0, screen.1).into());
    }
    eprintln!("app display: {tablet} shows {}x{} at {} Hz", screen.0, screen.1, reply["refresh"]);
    let decoder = reply["decoders"]["avc"].as_u64().map(|f| f as u32);
    let fps = if mode.fps > 0 { mode.fps } else { sizes::fps(reply["refresh"].as_f64(), decoder, false) };
    let (preset_kbps, _) = sizes::priority(&mode.priority);
    let to = SocketAddr::new(address.ip(), reply["port"].as_u64().unwrap_or(0) as u16);
    let sealer = Arc::new(Mutex::new(Sealer::new(&key, FROM_PC)));
    let waiting = Arc::new(AtomicUsize::new(0));
    let (frames_tx, frames_rx): (SyncSender<(Vec<u8>, bool, i64, i64)>, Receiver<_>) = mpsc::sync_channel(8);
    let w = waiting.clone();
    let skip = Box::new(move || w.load(Ordering::Relaxed) >= 2);
    let w = waiting.clone();
    let on_frame = Box::new(move |bytes: &[u8], keyframe: bool, captured: i64| {
        w.fetch_add(1, Ordering::Relaxed);
        let _ = frames_tx.send((bytes.to_vec(), keyframe, captured, video::now()));
    });
    let target = if mode.kbps > 0 { mode.kbps } else { sizes::bitrate(preset_kbps, screen) };
    let mut rate = Rate::new(target, true);
    // Extend: a monitor of the tablet's own, with nothing scaled. Without the driver, a mirror.
    let monitor = if mode.extend && display::available() { Some(Monitor::create(sizes::choose("auto", screen, true), fps)?) } else { None };
    let output = monitor.as_ref().map(|m| m.name.clone());
    let video = Arc::new(Video::start(fps, rate.kbps, screen, mode.keyframes, output, skip, on_frame)?);
    eprintln!("app display: {}x{}@{fps} H.264 ({}) {} kbit/s", video.size.0, video.size.1, video.name, rate.kbps);

    status.lock().unwrap().state = format!("On {tablet}, {}x{}", video.size.0, video.size.1);
    let payload = Arc::new(AtomicUsize::new(if mode.packets == "small" { PAYLOAD } else { LARGE_PAYLOAD }));
    let fallback = mode.packets == "auto";
    let kbps_now = Arc::new(AtomicU32::new(rate.kbps));
    let timings: Arc<Mutex<(Vec<f64>, Vec<f64>, usize)>> = Arc::default();
    let sender = {
        let (socket, sealer, payload, timings, waiting, stop) =
            (socket.try_clone()?, sealer.clone(), payload.clone(), timings.clone(), waiting.clone(), stop.clone());
        std::thread::Builder::new().name("screenlet-send".into()).spawn(move || {
            let mut number = 0u32;
            let mut body = vec![0u8; protocol::VIDEO_HEADER + LARGE_PAYLOAD];
            let mut sealed = vec![0u8; protocol::MAX_PACKET];
            while !stop.load(Ordering::Relaxed) {
                let Ok((bytes, keyframe, captured, encoded_at)) = frames_rx.recv_timeout(Duration::from_millis(200)) else { continue };
                waiting.fetch_sub(1, Ordering::Relaxed);
                number = number.wrapping_add(1);
                let unit = payload.load(Ordering::Relaxed);
                let count = bytes.len().div_ceil(unit).max(1);
                for (index, chunk) in bytes.chunks(unit).enumerate() {
                    let header = protocol::video_header(keyframe, number, index as u16, count as u16, captured);
                    body[..protocol::VIDEO_HEADER].copy_from_slice(&header);
                    body[protocol::VIDEO_HEADER..protocol::VIDEO_HEADER + chunk.len()].copy_from_slice(chunk);
                    let n = sealer.lock().unwrap().seal(&body[..protocol::VIDEO_HEADER + chunk.len()], &mut sealed);
                    let _ = socket.send_to(&sealed[..n], to);
                }
                let now = video::now();
                let mut t = timings.lock().unwrap();
                t.0.push((encoded_at - captured) as f64 / 1e6);
                t.1.push((now - captured) as f64 / 1e6);
                t.2 += bytes.len();
            }
        })?
    };

    let (from_app_tx, from_app) = mpsc::channel::<Vec<u8>>();
    {
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

    let receiver = {
        let (video, socket, stop, payload, timings, status) = (video.clone(), socket.try_clone()?, stop.clone(), payload, timings, status.clone());
        let tablet = tablet.clone();
        std::thread::Builder::new().name("screenlet-feedback".into()).spawn(move || {
            let mut input = Input::new(video.size, video.origin);
            let mut opener = Opener::new(&key, FROM_APP);
            let mut plain = [0u8; 2048];
            let mut last_keyframe = Instant::now() - Duration::from_secs(1);
            let mut shown = true;
            let mut keyframe = |video: &Video| {
                if last_keyframe.elapsed() >= Duration::from_millis(250) {
                    last_keyframe = Instant::now();
                    video.keyframe();
                }
            };
            while !stop.load(Ordering::Relaxed) {
                let wait = if input.busy() { 5 } else { 250 };
                let packet = match from_app.recv_timeout(Duration::from_millis(wait)) {
                    Ok(p) => Some(p),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                input.check();
                let Some(packet) = packet else { continue };
                let Some(body) = opener.open(&packet, &mut plain) else { continue };
                match body[0] {
                    protocol::INPUT => {
                        if !input.packet(body) { eprintln!("app display: a malformed input packet"); }
                    }
                    protocol::KEYFRAME => keyframe(&video),
                    protocol::PING if body.len() == 9 || body.len() == 10 => {
                        let mut pong = [0u8; 17];
                        pong[0] = protocol::PONG;
                        pong[1..9].copy_from_slice(&body[1..9]);
                        pong[9..17].copy_from_slice(&video::now().to_be_bytes());
                        let mut out = [0u8; 64];
                        let n = sealer.lock().unwrap().seal(&pong, &mut out);
                        let _ = socket.send_to(&out[..n], to);
                        if body.len() == 10 && (body[9] != 0) != shown {
                            shown = body[9] != 0;
                            video.pause(!shown);
                            if shown { keyframe(&video); } else { input.close(); }
                            let mut s = status.lock().unwrap();
                            s.paused = !shown;
                            s.state = if shown { format!("On {tablet}") } else { format!("On {tablet}, paused") };
                        }
                    }
                    protocol::STATS => {
                        let Some(stats) = Stats::parse(body) else { continue };
                        let (encoded, sent, bytes) = std::mem::take(&mut *timings.lock().unwrap());
                        let median = |mut v: Vec<f64>| { v.sort_by(f64::total_cmp); v.get(v.len() / 2).copied().unwrap_or(f64::NAN) };
                        let whole = stats.stages[0].unwrap_or_default();
                        if fallback && stats.shown == 0 && !sent.is_empty() && payload.load(Ordering::Relaxed) != PAYLOAD {
                            payload.store(PAYLOAD, Ordering::Relaxed);
                            eprintln!("app display: no frame arrives in large packets: small ones");
                            keyframe(&video);
                        }
                        if stats.shown > 0 {
                            if let Some(next) = rate.report(whole[0], stats.lost) {
                                video.rate(next);
                                kbps_now.store(next, Ordering::Relaxed);
                            }
                        }
                        let text = format!("{} fps, {} lost\nCapture to decoded {:.1} ms (90%: {:.1})\nEncoded {:.1} ms, sent {:.1} ms\n{} kB/s",
                                           stats.shown, stats.lost, whole[0], whole[1], median(encoded), median(sent), bytes / 1024);
                        eprintln!("app display: {}", text.replace('\n', "; "));
                        status.lock().unwrap().stats = text;
                    }
                    _ => {}
                }
            }
            input.close();
        })?
    };

    let failure: Mutex<Option<String>> = Mutex::new(None);
    let result = link.hold(resume, || {
        if let Some(error) = video.error() {
            *failure.lock().unwrap() = Some(error);
            return true;
        }
        stop.load(Ordering::Relaxed)
    });
    stop.store(true, Ordering::Relaxed);
    let _ = receiver.join();
    let _ = sender.join();
    drop(video);
    drop(monitor);
    result?;
    match failure.into_inner().unwrap() {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}
