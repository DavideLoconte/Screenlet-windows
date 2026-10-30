//! What the user chose, in settings.json: from the next session.
use crate::link::config;
use serde_json::{json, Value};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::*;

const RUN: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");

#[derive(Clone)]
pub struct Mode {
    pub priority: String,
    pub fps: u32,  // 0: the tablet's
    pub kbps: u32, // 0: from priority
    pub packets: String,
    pub keyframes: u32, // seconds, 0: rarely
    pub extend: bool,   // a monitor of its own, when the driver is there
    pub resolution: String, // "auto", "tablet" (native) or "WxH"
    pub mirror: String,     // the monitor to mirror (\.\DISPLAY1), "" for the first
    pub codec: String,      // "auto", "avc", "hevc"
    pub vbv_frames: f64,    // the largest frame in average ones, 0: from priority
    pub connection: String, // "auto", "wifi", "cable"
    pub auto_start: bool,   // show the desktop when a paired tablet is plugged in
    pub tablet_screen: Option<(i32, i32)>, // the last tablet's, for the resolutions offered
}

impl Default for Mode {
    fn default() -> Mode {
        Mode { priority: "balanced".into(), fps: 0, kbps: 0, packets: "auto".into(), keyframes: 0, extend: true, resolution: "auto".into(),
            mirror: String::new(), codec: "auto".into(), vbv_frames: 0.0, connection: "auto".into(), auto_start: false, tablet_screen: None }
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
            extend: v["display"].as_str().is_none_or(|d| d != "mirror"), resolution: word("resolution", &d.resolution),
            mirror: word("mirror", &d.mirror), codec: word("codec", &d.codec), vbv_frames: v["vbv_frames"].as_f64().unwrap_or(d.vbv_frames),
            connection: word("connection", &d.connection), auto_start: v["auto_start"].as_bool().unwrap_or(d.auto_start),
            tablet_screen: v["tablet_screen"].as_array().and_then(|a| Some((a.first()?.as_i64()? as i32, a.get(1)?.as_i64()? as i32))) }
    }

    pub fn json(&self) -> Value {
        json!({"priority": self.priority, "fps": self.fps, "kbps": self.kbps, "packets": self.packets, "keyframes": self.keyframes,
            "display": if self.extend { "extend" } else { "mirror" }, "resolution": self.resolution, "mirror": self.mirror,
            "codec": self.codec, "vbv_frames": self.vbv_frames, "connection": self.connection, "auto_start": self.auto_start,
            "tablet_screen": self.tablet_screen.map(|s| [s.0, s.1])})
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(config());
        let _ = std::fs::write(config().join("settings.json"), self.json().to_string());
    }

    /// One setting from the window, kept.
    pub fn set(&mut self, key: &str, value: &str) {
        match key {
            "priority" if matches!(value, "latency" | "balanced" | "clarity") => self.priority = value.into(),
            "packets" if matches!(value, "auto" | "large" | "small") => self.packets = value.into(),
            "codec" if matches!(value, "auto" | "avc" | "hevc") => self.codec = value.into(),
            "connection" if matches!(value, "auto" | "wifi" | "cable") => self.connection = value.into(),
            "fps" => self.fps = value.parse().unwrap_or(0),
            "kbps" => self.kbps = value.parse().unwrap_or(0),
            "keyframes" => self.keyframes = value.parse().unwrap_or(0),
            "vbv_frames" => self.vbv_frames = value.parse().unwrap_or(0.0),
            "display" => self.extend = value != "mirror",
            "resolution" => self.resolution = value.into(),
            "mirror" => self.mirror = value.into(),
            "auto_start" => self.auto_start = value == "1",
            _ => return,
        }
        self.save();
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
