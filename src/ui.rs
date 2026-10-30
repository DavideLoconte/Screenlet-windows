//! The app: an icon by the clock, its menu, and the window (ui/, WinUI), a process of
//! its own while it is open: this one stays small and keeps the session whatever it does.
use crate::display;
use crate::link::paired;
use crate::nearby::{self, Tablet};
use crate::session::{self, Status};
use crate::settings::{start_with_windows, starts_with_windows, Mode};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
const QUIT: usize = 1;
const STOP: usize = 2;
const WINDOW: usize = 3;
const RESUME: usize = 4;
const TABLET: usize = 10; // and the next ones, in the menu's order

struct Running {
    stop: Arc<AtomicBool>,
    resume: Arc<AtomicBool>,
    name: String,
    status: Arc<Mutex<Status>>,
}

struct App {
    window: HWND,
    tablets: Arc<Mutex<HashMap<String, Tablet>>>,
    manual: Option<Tablet>,
    listed: Vec<Tablet>,
    running: Option<Running>,
    ended: Arc<Mutex<Option<String>>>,
    restored: u32, // the taskbar came back: the icon goes in again
    mode: Mode,
    message: String,
    screen: Option<(Child, ChildStdin)>, // the window, when open
    commands: Arc<Mutex<Vec<Value>>>,
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

impl App {
    fn tip(&self) -> String {
        self.running.as_ref().map_or("Screenlet".into(), |r| format!("Screenlet: on {}", r.name))
    }

    fn start(&mut self, tablet: Tablet) {
        if self.running.is_some() { return; }
        let (stop, resume) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let status: Arc<Mutex<Status>> = Arc::default();
        status.lock().unwrap().state = format!("Connecting to {}…", tablet.name);
        self.running = Some(Running { stop: stop.clone(), resume: resume.clone(), name: tablet.name.clone(), status: status.clone() });
        icon(self.window, NIM_MODIFY, &self.tip(), None);
        let (ended, window, mode) = (self.ended.clone(), self.window.0 as isize, self.mode.clone());
        std::thread::spawn(move || {
            let result = (|| {
                if paired::by_id(&tablet.id).is_none() {
                    session::pair(&tablet.address, &ask)?;
                }
                session::run(&tablet.address, &mode, &status, &stop, &resume)
            })();
            *ended.lock().unwrap() = result.err().map(|e| e.to_string());
            unsafe { let _ = PostMessageW(Some(HWND(window as _)), ENDED, WPARAM(0), LPARAM(0)); }
        });
    }

    fn list(&mut self) {
        self.listed = self.tablets.lock().unwrap().values().cloned().chain(self.manual.clone()).collect();
        self.listed.sort_by(|a, b| a.name.cmp(&b.name));
    }

    /// The window's state.
    fn state(&mut self) -> Value {
        self.list();
        let paired = |t: &Tablet| paired::by_id(&t.id).is_some();
        let (state, tint, actions, stats) = if let Some(running) = &self.running {
            let status = running.status.lock().unwrap();
            let mut actions = Vec::new();
            if status.paused {
                actions.push(json!({"id": "resume", "device": running.name, "detail": "Paused: not on the tablet's screen", "short": "Resume", "primary": true}));
            }
            actions.push(json!({"id": "stop", "device": running.name, "detail": "Wi-Fi", "short": "Disconnect", "primary": false}));
            (status.state.clone(), "on", actions, status.stats.clone())
        } else {
            let actions: Vec<Value> = self.listed.iter().enumerate().map(|(i, t)| if paired(t) {
                json!({"id": format!("tablet {i}"), "device": t.name, "detail": "Wi-Fi", "short": "Show the desktop", "primary": true})
            } else {
                json!({"id": format!("tablet {i}"), "device": t.name, "detail": "Wi-Fi", "short": "Pair…", "primary": false})
            }).collect();
            let idle = if actions.is_empty() { "No tablet with Screenlet open" } else { "A tablet is ready" };
            (if self.message.is_empty() { idle.to_string() } else { self.message.clone() },
             if actions.is_empty() { "idle" } else { "ready" }, actions, "Shown while the desktop is on the tablet".to_string())
        };
        let mut settings = self.mode.json();
        settings["login"] = json!(starts_with_windows());
        settings["extendable"] = json!(display::available());
        json!({"state": state, "tint": tint, "actions": actions, "settings": settings, "stats": stats})
    }

    fn publish(&mut self) {
        if self.screen.is_none() { return; }
        let line = self.state().to_string() + "\n";
        let gone = self.screen.as_mut().is_some_and(|(_, input)| input.write_all(line.as_bytes()).is_err());
        if gone { self.screen = None; }
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

    fn command(&mut self, command: Value) {
        if let (Some(key), Some(value)) = (command["set"].as_str(), command["value"].as_str()) {
            if key == "login" { start_with_windows(value == "1"); } else { self.mode.set(key, value); }
        }
        match command["run"].as_str() {
            Some("stop") => if let Some(running) = &self.running { running.stop.store(true, Ordering::Relaxed); },
            Some("resume") => if let Some(running) = &self.running { running.resume.store(true, Ordering::Relaxed); },
            Some("install") => if let Ok(exe) = std::env::current_exe() {
                // Windows asks the user; the window sees the driver at its next update.
                unsafe { ShellExecuteW(None, w!("runas"), &HSTRING::from(exe.as_os_str()), w!("--install-driver"), None, SW_HIDE); }
            },
            Some(run) => if let Some(i) = run.strip_prefix("tablet ").and_then(|n| n.parse::<usize>().ok()) {
                if let Some(tablet) = self.listed.get(i).cloned() { self.message.clear(); self.start(tablet); }
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
                for (i, tablet) in self.listed.iter().enumerate() {
                    let _ = AppendMenuW(menu, MF_STRING, TABLET + i, &text(format!("Show on {}", tablet.name)));
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
                n if n >= TABLET => if let Some(tablet) = self.listed.get(n - TABLET).cloned() { self.start(tablet); },
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
            WM_TIMER => self.publish(),
            COMMAND => {
                let commands = std::mem::take(&mut *self.commands.lock().unwrap());
                for command in commands { self.command(command); }
            }
            ENDED => {
                self.running = None;
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
        // Any paired tablet: TLS proves which.
        let manual = manual.map(|address| Tablet { id: paired::all().keys().next().map_or(String::new(), |f| f[..16].to_string()),
            name: format!("Tablet at {}", address.ip()), address });
        let mut app = App { window, tablets: nearby::browse(), manual, listed: Vec::new(), running: None, ended: Arc::default(),
            restored: RegisterWindowMessageW(w!("TaskbarCreated")), mode: Mode::load(), message: String::new(), screen: None, commands: Arc::default() };
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
