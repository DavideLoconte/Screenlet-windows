//! What the user chose, in settings.json: from the next session.
use crate::link::config;
use serde_json::{json, Value};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::*;

const RUN: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");

#[derive(Clone)]
pub struct Mode {
    pub priority: String,
    pub fps: u32,  // 0: the tablet's
    pub kbps: u32, // 0: from priority
    pub packets: String,
    pub keyframes: u32, // seconds, 0: rarely
    pub extend: bool,   // a monitor of its own, when the driver is there
}

impl Default for Mode {
    fn default() -> Mode {
        Mode { priority: "balanced".into(), fps: 0, kbps: 0, packets: "auto".into(), keyframes: 0, extend: true }
    }
}

impl Mode {
    pub fn load() -> Mode {
        let v: Value = std::fs::read(config().join("settings.json")).ok()
            .and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default();
        let d = Mode::default();
        let number = |key: &str, default| v[key].as_u64().map_or(default, |n| n as u32);
        let word = |key: &str, default: &str| v[key].as_str().unwrap_or(default).to_string();
        Mode { priority: word("priority", &d.priority), fps: number("fps", d.fps), kbps: number("kbps", d.kbps),
            packets: word("packets", &d.packets), keyframes: number("keyframes", d.keyframes),
            extend: v["display"].as_str().is_none_or(|d| d != "mirror") }
    }

    pub fn json(&self) -> Value {
        json!({"priority": self.priority, "fps": self.fps, "kbps": self.kbps, "packets": self.packets, "keyframes": self.keyframes,
            "display": if self.extend { "extend" } else { "mirror" }})
    }

    /// One setting from the window, kept.
    pub fn set(&mut self, key: &str, value: &str) {
        match key {
            "priority" if matches!(value, "latency" | "balanced" | "clarity") => self.priority = value.into(),
            "packets" if matches!(value, "auto" | "large" | "small") => self.packets = value.into(),
            "fps" => self.fps = value.parse().unwrap_or(0),
            "kbps" => self.kbps = value.parse().unwrap_or(0),
            "keyframes" => self.keyframes = value.parse().unwrap_or(0),
            "display" => self.extend = value != "mirror",
            _ => return,
        }
        let _ = std::fs::create_dir_all(config());
        let _ = std::fs::write(config().join("settings.json"), self.json().to_string());
    }
}

pub fn starts_with_windows() -> bool {
    let mut size = 0;
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN, w!("Screenlet"), RRF_RT_REG_SZ, None, None, Some(&mut size)) == ERROR_SUCCESS }
}

pub fn start_with_windows(on: bool) {
    unsafe {
        if !on {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN, w!("Screenlet"));
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let line: Vec<u16> = format!("\"{}\" --background", exe.display()).encode_utf16().chain([0]).collect();
        let _ = RegSetKeyValueW(HKEY_CURRENT_USER, RUN, w!("Screenlet"), REG_SZ.0, Some(line.as_ptr() as _), line.len() as u32 * 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_values_are_taken() {
        let mut m = Mode::default();
        m.fps = 30;
        m.keyframes = 5;
        let v = m.json();
        assert_eq!((v["fps"].as_u64(), v["keyframes"].as_u64()), (Some(30), Some(5)));
    }
}
