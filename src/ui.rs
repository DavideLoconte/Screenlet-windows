//! The app: an icon by the clock, its menu, and the window (ui/, WinUI), a process of
//! its own while it is open: this one stays small and keeps the session whatever it does.
use crate::cable::{self, Cable, Plugged};
use crate::display;
use crate::link::{paired, Result};
use crate::nearby::{self, Tablet};
use crate::protocol::PORT;
use crate::session::{self, Path, Status};
use crate::settings::{start_with_windows, starts_with_windows, Mode};
use crate::sizes;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const ICON: u32 = WM_APP + 1;
const ENDED: u32 = WM_APP + 2;
const OPEN: u32 = WM_APP + 3;     // another start of the app: the window
const COMMAND: u32 = WM_APP + 4;  // the window said something
const NOTICE: u32 = WM_APP + 5;   // the driver's install ended
const QUIT: usize = 1;
const STOP: usize = 2;
const WINDOW: usize = 3;
const RESUME: usize = 4;
const TABLET: usize = 10; // and the next ones, in the menu's order
const REPLUG_SECONDS: u32 = 5; // longer than a cycled port takes

struct Running {
    stop: Arc<AtomicBool>,
    resume: Arc<AtomicBool>,
    name: String,
    detail: &'static str,
    status: Arc<Mutex<Status>>,
}

/// How to reach a tablet.
#[derive(Clone)]
enum Way {
    /// On the network; the id to insist on, when it is known to be paired.
    Network(SocketAddr, Option<String>),
    Cable,
    /// Its own Wi-Fi Direct group, and its id.
    Direct { ssid: String, passphrase: String, at: String, id: String },
}

/// What can be done with a tablet: a button in the window, a line in the menu.
#[derive(Clone)]
struct Action {
    device: String,
    detail: &'static str,
    short: &'static str,
    way: Option<Way>, // none: a line to read, with no button
    primary: bool,
}

struct App {
    window: HWND,
    tablets: Arc<Mutex<HashMap<String, Tablet>>>,
    manual: Option<Tablet>,
    listed: Vec<Action>,
    running: Option<Running>,
    ended: Arc<Mutex<Option<String>>>,
    restored: u32, // the taskbar came back: the icon goes in again
    mode: Mode,
    message: String,
    screen: Option<(Child, ChildStdin)>, // the window, when open
    commands: Arc<Mutex<Vec<Value>>>,
    cable: Arc<Mutex<Option<Arc<Cable>>>>, // open across sessions: the tablet stays an accessory
    plugged: Plugged,
    rested: bool, // the tablet was really unplugged: a session that ended cycles its port, and it comes back
    gone: u32,    // seconds with no tablet on the cable
    notice: Arc<Mutex<String>>,
    installing: bool,
    monitors: (Instant, Vec<(String, String)>),
}

thread_local! { static APP: RefCell<Option<App>> = const { RefCell::new(None) }; }

fn fill(into: &mut [u16], text: &str) {
    let room = into.len() - 1;
    for (slot, unit) in into.iter_mut().zip(text.encode_utf16().take(room).chain([0])) {
        *slot = unit;
    }
}

fn icon(window: HWND, change: NOTIFY_ICON_MESSAGE, tip: &str, say: Option<(&str, &str)>) {
    let mut data = NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: window, uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP, uCallbackMessage: ICON, ..Default::default() };
    unsafe {
        let module = GetModuleHandleW(None).unwrap_or_default();
        data.hIcon = LoadIconW(Some(module.into()), PCWSTR(1 as _)).unwrap_or_default();
    }
    fill(&mut data.szTip, tip);
    if let Some((title, text)) = say {
        data.uFlags |= NIF_INFO;
        fill(&mut data.szInfoTitle, title);
        fill(&mut data.szInfo, text);
    }
    unsafe { let _ = Shell_NotifyIconW(change, &data); }
}

fn ask(name: &str, code: &str) -> bool {
    let text = HSTRING::from(format!("Pair with {name} only if it shows {} {}", &code[..3], &code[3..]));
    unsafe { MessageBoxW(None, &text, w!("Screenlet"), MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND | MB_TOPMOST) == IDYES }
}

/// The way as a path to the tablet: the cable opened, or the Wi-Fi Direct group joined.
fn path(way: &Way, cable: &Mutex<Option<Arc<Cable>>>, status: &Mutex<Status>) -> Result<Path> {
    match way {
        Way::Network(address, id) => Ok(Path::Network(*address, id.clone())),
        Way::Cable => {
            let mut open = cable.lock().unwrap();
            if let Some(cable) = open.as_ref().filter(|c| c.alive()) { return Ok(Path::Cable(cable.clone())); }
            status.lock().unwrap().state = "Opening the link over the cable… (allow Screenlet on the tablet)".into();
            let opened = Cable::open()?;
            *open = Some(opened.clone());
            Ok(Path::Cable(opened))
        }
        Way::Direct { ssid, passphrase, at, id } => {
            status.lock().unwrap().state = format!("Joining {ssid}…");
            nearby::join(ssid, passphrase)?;
            let address: SocketAddr = format!("{at}:{PORT}").parse()?;
            // The network comes up some seconds after joining it.
            let deadline = Instant::now() + Duration::from_secs(15);
            while std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_err() {
                if Instant::now() > deadline { return Err("the tablet's Wi-Fi Direct network did not answer".into()); }
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok(Path::Network(address, Some(id.clone())))
        }
    }
}

impl App {
    fn tip(&self) -> String {
        self.running.as_ref().map_or("Screenlet".into(), |r| format!("Screenlet: on {}", r.name))
    }

    /// chosen: by the user, who may then be asked to pair. Never for a tablet merely plugged in.
    fn start(&mut self, action: Action, chosen: bool) {
        let Some(way) = action.way else { return };
        if self.running.is_some() { return; }
        let (stop, resume) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let status: Arc<Mutex<Status>> = Arc::default();
        status.lock().unwrap().state = format!("Connecting to {}…", action.device);
        self.message.clear();
        self.running = Some(Running { stop: stop.clone(), resume: resume.clone(), name: action.device, detail: action.detail, status: status.clone() });
        icon(self.window, NIM_MODIFY, &self.tip(), None);
        let (ended, window, mode, cable) = (self.ended.clone(), self.window.0 as isize, self.mode.clone(), self.cable.clone());
        std::thread::spawn(move || {
            let result = (|| {
                let path = path(&way, &cable, &status)?;
                let asking: Option<&dyn Fn(&str, &str) -> bool> = if chosen { Some(&ask) } else { None };
                session::run(&path, &mode, false, asking, &status, &stop, &resume)
            })();
            *ended.lock().unwrap() = result.err().map(|e| e.to_string());
            unsafe { let _ = PostMessageW(Some(HWND(window as _)), ENDED, WPARAM(0), LPARAM(0)); }
        });
    }

    /// What can be done now, by the connection chosen: as Screenlet for GNOME's window.
    fn list(&mut self) {
        let mut tablets: Vec<Tablet> = self.tablets.lock().unwrap().values().cloned().collect();
        tablets.sort_by_key(|t| t.name.to_lowercase());
        let plugged = self.plugged != Plugged::None;
        let connection = self.mode.connection.as_str();
        let by_cable = plugged && connection != "wifi";
        let by_wifi = connection == "wifi" || (connection == "auto" && !plugged);
        let all = paired::all();
        let mut seen = Vec::new();
        let mut found = Vec::new();
        for t in tablets {
            let known = paired::by_id(&t.id).is_some();
            if known { seen.push(t.id.clone()); }
            if known && by_wifi {
                found.push(Action { device: t.name, detail: "Wi-Fi", short: "Connect", way: Some(Way::Network(t.address, Some(t.id))), primary: true });
            } else if !known && by_wifi {
                found.push(Action { device: t.name, detail: "Wi-Fi", short: "Connect", way: Some(Way::Network(t.address, None)), primary: true });
            }
        }
        if let Some(t) = &self.manual {
            found.push(Action { device: t.name.clone(), detail: "Wi-Fi", short: "Connect", way: Some(Way::Network(t.address, None)), primary: true });
        }
        if by_cable {
            let device = "Tablet on the cable".to_string();
            if self.plugged == Plugged::Closed {
                found.push(Action { device, detail: "Install Extend, or turn on USB debugging on it, to use the cable", short: "", way: None, primary: false });
            } else {
                found.push(Action { device, detail: "USB-C cable", short: "Connect", way: Some(Way::Cable), primary: true });
            }
        }
        // A paired tablet nowhere to be seen, whose own Wi-Fi Direct group this PC knows.
        if by_wifi {
            for (fingerprint, entry) in &all {
                let id: String = fingerprint.chars().take(16).collect();
                let (Some(ssid), Some(passphrase)) = (entry["p2p"]["ssid"].as_str(), entry["p2p"]["passphrase"].as_str()) else { continue };
                if seen.contains(&id) { continue; }
                found.push(Action { device: entry["name"].as_str().unwrap_or("Tablet").to_string(), detail: "Wi-Fi Direct", short: "Connect",
                    way: Some(Way::Direct { ssid: ssid.into(), passphrase: passphrase.into(), at: entry["p2p"]["at"].as_str().unwrap_or("192.168.49.1").into(), id }), primary: true });
            }
        }
        self.listed = found;
    }

    /// The resolutions for the tablet last seen: Automatic, native, then smaller at its proportions.
    fn resolutions(&self) -> Value {
        let screen = self.mode.tablet_screen;
        let automatic = screen.and_then(sizes::models).map_or("Automatic".to_string(), |m| format!("Automatic ({m})"));
        let mut rows = vec![json!({"tag": "auto", "name": automatic})];
        match screen {
            Some(screen) => for (i, size) in sizes::all(screen).into_iter().enumerate() {
                rows.push(if i == 0 { json!({"tag": "tablet", "name": format!("{} × {} (native)", size.0, size.1)}) }
                          else { json!({"tag": format!("{}x{}", size.0, size.1), "name": format!("{} × {}", size.0, size.1)}) });
            },
            None => rows.push(json!({"tag": "tablet", "name": "Native"})),
        }
        Value::Array(rows)
    }

    /// The window's state.
    fn state(&mut self) -> Value {
        self.list();
        if self.running.is_none() && self.monitors.0.elapsed() >= Duration::from_secs(5) {
            self.monitors = (Instant::now(), display::monitors());
        }
        let (state, tint, actions, stats) = if let Some(running) = &self.running {
            let status = running.status.lock().unwrap();
            let mut actions = Vec::new();
            if status.paused {
                actions.push(json!({"id": "resume", "device": running.name, "detail": "Paused: not on the tablet's screen", "short": "Resume", "primary": true}));
            }
            actions.push(json!({"id": "stop", "device": running.name, "detail": running.detail, "short": "Disconnect", "primary": false}));
            (status.state.clone(), "on", actions, status.stats.clone())
        } else {
            let actions: Vec<Value> = self.listed.iter().enumerate().map(|(i, a)|
                json!({"id": format!("tablet {i}"), "device": a.device, "detail": a.detail, "short": a.short, "primary": a.primary})).collect();
            let idle = if actions.is_empty() { "No tablet with Screenlet open" } else { "A tablet is ready" };
            (if self.message.is_empty() { idle.to_string() } else { self.message.clone() },
             if actions.is_empty() { "idle" } else { "ready" }, actions, "Shown while the desktop is on the tablet".to_string())
        };
        let mut settings = self.mode.json();
        settings["login"] = json!(starts_with_windows());
        settings["extendable"] = json!(display::available());
        settings["installing"] = json!(self.installing);
        settings["resolutions"] = self.resolutions();
        settings["monitors"] = self.monitors.1.iter().map(|(tag, name)| json!({"tag": tag, "name": name})).collect();
        json!({"state": state, "tint": tint, "actions": actions, "settings": settings, "stats": stats})
    }

    fn publish(&mut self) {
        if self.screen.is_none() { return; }
        let line = self.state().to_string() + "\n";
        let gone = self.screen.as_mut().is_some_and(|(_, input)| input.write_all(line.as_bytes()).is_err());
        if gone { self.screen = None; }
    }

    /// Each second: the cable, and what a session learned of its tablet.
    fn tick(&mut self) {
        let alive = self.cable.lock().unwrap().as_ref().is_some_and(|c| c.alive());
        let now = if alive { Plugged::Ready } else { cable::plugged() };
        let was = std::mem::replace(&mut self.plugged, now);
        self.gone = if now == Plugged::None { self.gone + 1 } else { 0 };
        if self.gone >= REPLUG_SECONDS { self.rested = true; }
        if now == Plugged::Ready && was != Plugged::Ready && self.rested && self.mode.auto_start && self.mode.connection != "wifi"
            && self.running.is_none() && !paired::all().is_empty() {
            self.list();
            // Plugged in: the desktop goes to it by itself.
            if let Some(action) = self.listed.iter().find(|a| matches!(a.way, Some(Way::Cable))).cloned() { self.start(action, false); }
        }
        let screen = self.running.as_ref().and_then(|r| r.status.lock().unwrap().screen);
        if screen.is_some() && screen != self.mode.tablet_screen {
            self.mode.tablet_screen = screen;
            self.mode.save();
        }
        self.publish();
    }

    fn open(&mut self) {
        if self.screen.as_mut().is_some_and(|(child, _)| matches!(child.try_wait(), Ok(None))) { return; }
        let Ok(exe) = std::env::current_exe() else { return };
        let path = exe.with_file_name("window").join("ScreenletWindow.exe");
        let started = Command::new(&path).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn();
        let mut child = match started {
            Ok(child) => child,
            Err(e) => {
                icon(self.window, NIM_MODIFY, &self.tip(), Some(("Screenlet", &format!("No window: {e}"))));
                return;
            }
        };
        let (Some(input), Some(output)) = (child.stdin.take(), child.stdout.take()) else { return };
        let (commands, window) = (self.commands.clone(), self.window.0 as isize);
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(std::result::Result::ok) {
                if let Ok(command) = serde_json::from_str(&line) {
                    commands.lock().unwrap().push(command);
                    unsafe { let _ = PostMessageW(Some(HWND(window as _)), COMMAND, WPARAM(0), LPARAM(0)); }
                }
            }
        });
        self.screen = Some((child, input));
        self.publish();
    }

    /// What Extend needs: Windows asks the user, on a thread that waits for the answer.
    fn install(&mut self, parameters: &'static str) {
        if self.installing { return; }
        self.installing = true;
        let (notice, window) = (self.notice.clone(), self.window.0 as isize);
        std::thread::spawn(move || {
            if let Err(e) = display::elevated(parameters) { *notice.lock().unwrap() = format!("Not installed: {e}"); }
            unsafe { let _ = PostMessageW(Some(HWND(window as _)), NOTICE, WPARAM(0), LPARAM(0)); }
        });
    }

    fn command(&mut self, command: Value) {
        if let (Some(key), Some(value)) = (command["set"].as_str(), command["value"].as_str()) {
            if key == "login" { start_with_windows(value == "1"); } else { self.mode.set(key, value); }
        }
        match command["run"].as_str() {
            Some("stop") => if let Some(running) = &self.running { running.stop.store(true, Ordering::Relaxed); },
            Some("resume") => if let Some(running) = &self.running { running.resume.store(true, Ordering::Relaxed); },
            Some("install") => self.install("--install-driver"),
            Some("defaults") => {
                let defaults = Mode::default();
                self.mode.fps = defaults.fps;
                self.mode.codec = defaults.codec;
                self.mode.kbps = defaults.kbps;
                self.mode.vbv_frames = defaults.vbv_frames;
                self.mode.packets = defaults.packets;
                self.mode.keyframes = defaults.keyframes;
                self.mode.save();
            }
            Some(run) => if let Some(i) = run.strip_prefix("tablet ").and_then(|n| n.parse::<usize>().ok()) {
                if let Some(action) = self.listed.get(i).cloned() { self.start(action, true); }
            },
            None => {}
        }
        self.publish();
    }

    fn menu(&mut self) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let text = |t: String| HSTRING::from(t);
            let _ = AppendMenuW(menu, MF_STRING | MF_DEFAULT, WINDOW, w!("Open Screenlet"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            if let Some(running) = &self.running {
                let _ = AppendMenuW(menu, MF_GRAYED, 0, &text(format!("On {}", running.name)));
                if running.status.lock().unwrap().paused {
                    let _ = AppendMenuW(menu, MF_STRING, RESUME, w!("Resume"));
                }
                let _ = AppendMenuW(menu, MF_STRING, STOP, w!("Disconnect"));
            } else {
                self.list();
                if self.listed.is_empty() {
                    let _ = AppendMenuW(menu, MF_GRAYED, 0, w!("Looking for your tablet…"));
                }
                for (i, action) in self.listed.iter().enumerate() {
                    let flags = if action.way.is_some() { MF_STRING } else { MF_GRAYED };
                    let line = if action.way.is_some() { format!("Connect to {}", action.device) } else { format!("{}: {}", action.device, action.detail) };
                    let _ = AppendMenuW(menu, flags, TABLET + i, &text(line));
                }
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, QUIT, w!("Quit"));
            let mut at = POINT::default();
            let _ = GetCursorPos(&mut at);
            let _ = SetForegroundWindow(self.window); // or the menu stays when clicking elsewhere
            let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_BOTTOMALIGN, at.x, at.y, None, self.window, None).0 as usize;
            let _ = DestroyMenu(menu);
            match chosen {
                QUIT => self.quit(),
                WINDOW => self.open(),
                STOP => if let Some(running) = &self.running { running.stop.store(true, Ordering::Relaxed); },
                RESUME => if let Some(running) = &self.running { running.resume.store(true, Ordering::Relaxed); },
                n if n >= TABLET => if let Some(action) = self.listed.get(n - TABLET).cloned() { self.start(action, true); },
                _ => {}
            }
        }
    }

    fn quit(&mut self) {
        if let Some(running) = &self.running { running.stop.store(true, Ordering::Relaxed); }
        self.screen = None; // the window ends with its input
        icon(self.window, NIM_DELETE, "", None);
        unsafe { PostQuitMessage(0); }
    }

    fn message(&mut self, id: u32, lparam: LPARAM) {
        match id {
            ICON => match lparam.0 as u32 & 0xffff {
                WM_LBUTTONUP => self.open(),
                WM_RBUTTONUP => self.menu(),
                _ => {}
            },
            OPEN => self.open(),
            WM_TIMER => self.tick(),
            COMMAND => {
                let commands = std::mem::take(&mut *self.commands.lock().unwrap());
                for command in commands { self.command(command); }
            }
            NOTICE => {
                self.installing = false;
                self.message = std::mem::take(&mut *self.notice.lock().unwrap());
                self.publish();
            }
            ENDED => {
                if self.running.take().is_some_and(|r| r.detail == "USB-C cable") {
                    if let Some(cable) = self.cable.lock().unwrap().take() { cable.close(); }
                    self.rested = false; // Disconnect: it stays so until the cable is really pulled out
                }
                let failure = self.ended.lock().unwrap().take();
                self.message = failure.clone().unwrap_or_default();
                icon(self.window, NIM_MODIFY, &self.tip(), failure.as_deref().map(|f| ("Screenlet", f)));
                self.publish();
            }
            id if id == self.restored => icon(self.window, NIM_ADD, &self.tip(), None),
            _ => {}
        }
    }
}

unsafe extern "system" fn procedure(window: HWND, id: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // A menu runs its own loop: messages meanwhile wait for it to end, the app is in use.
    APP.with(|app| if let Ok(mut app) = app.try_borrow_mut() {
        if let Some(app) = app.as_mut() { app.message(id, lparam); }
    });
    unsafe { DefWindowProcW(window, id, wparam, lparam) }
}

pub fn run(background: bool, manual: Option<SocketAddr>) {
    unsafe {
        // One at a time: a second start only opens the first one's window.
        let _once = CreateMutexW(None, true, w!("Local\\Screenlet"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(first) = FindWindowW(w!("Screenlet"), None) { let _ = PostMessageW(Some(first), OPEN, WPARAM(0), LPARAM(0)); }
            return;
        }
        let module = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW { lpfnWndProc: Some(procedure), hInstance: module.into(), lpszClassName: w!("Screenlet"), ..Default::default() };
        RegisterClassW(&class);
        let Ok(window) = CreateWindowExW(WINDOW_EX_STYLE(0), w!("Screenlet"), w!("Screenlet"), WINDOW_STYLE(0), 0, 0, 0, 0,
            None, None, Some(module.into()), None) else { return };
        let manual = manual.map(|address| Tablet { id: String::new(), name: format!("Tablet at {}", address.ip()), address });
        let mut app = App { window, tablets: nearby::browse(), manual, listed: Vec::new(), running: None, ended: Arc::default(),
            restored: RegisterWindowMessageW(w!("TaskbarCreated")), mode: Mode::load(), message: String::new(), screen: None, commands: Arc::default(),
            cable: Arc::default(), plugged: Plugged::None, rested: true, gone: 0, notice: Arc::default(), installing: false, monitors: (Instant::now(), display::monitors()) };
        icon(window, NIM_ADD, &app.tip(), None);
        if !background { app.open(); }
        SetTimer(Some(window), 1, 1000, None);
        APP.with(|a| *a.borrow_mut() = Some(app));
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}
