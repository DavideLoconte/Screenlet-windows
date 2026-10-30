//! Screenlet for Windows.
#![cfg_attr(not(test), windows_subsystem = "windows")]

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

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

const USAGE: &str = "screenlet                         the window, and the icon by the clock
screenlet --background            only the icon
screenlet --install-driver        what Extend needs (as administrator)
screenlet --tablets               the tablets on the network
screenlet --pair ADDRESS          pair with the tablet at ADDRESS, comparing the code
screenlet --show ADDRESS SECONDS  the desktop on the tablet, for a while";

fn address(text: &str) -> link::Result<SocketAddr> {
    let text = if text.contains(':') { text.to_string() } else { format!("{text}:{}", protocol::PORT) };
    text.to_socket_addrs()?.next().ok_or_else(|| "no such address".into())
}

fn main() {
    // Desktop Duplication and pointer injection both in the monitor's real pixels.
    unsafe { let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); }
    let args: Vec<String> = std::env::args().collect();
    let result = match (args.get(1).map(String::as_str), args.get(2)) {
        (None, _) | (Some("--background"), _) | (Some("--tablet"), _) => {
            // --tablet: for a network that hides the tablet's announcements.
            let manual = (args.get(1).map(String::as_str) == Some("--tablet")).then(|| args.get(2).and_then(|a| address(a).ok())).flatten();
            ui::run(args.get(1).map(String::as_str) == Some("--background"), manual);
            return;
        }
        (Some("--tablets"), _) => tablets(),
        (Some("--install-driver"), _) => display::install(),
        (Some("--pair"), Some(to)) => address(to).and_then(pair),
        (Some("--show"), Some(to)) => address(to).and_then(|a| show(a, args.get(3).and_then(|s| s.parse().ok()).unwrap_or(20))),
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
    Ok(())
}

fn pair(to: SocketAddr) -> link::Result<()> {
    let ask = |name: &str, code: &str| {
        println!("Pair with {name} only if it shows {} {}. Pair? [y/N]", &code[..3], &code[3..]);
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).is_ok() && answer.trim().eq_ignore_ascii_case("y")
    };
    println!("Paired with {}", session::pair(&to, &ask)?);
    Ok(())
}

fn show(to: SocketAddr, seconds: u64) -> link::Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let timer = stop.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(seconds));
        timer.store(true, Ordering::Relaxed);
    });
    let status = Arc::default();
    session::run(&to, &settings::Mode::load(), &status, &stop, &AtomicBool::new(false))
}
