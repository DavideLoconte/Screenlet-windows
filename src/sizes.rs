//! Display sizes at the tablet's proportions, the presets, and the bitrate
//! that follows the network: as Screenlet for macOS chooses them.

pub type Size = (i32, i32);

const WIDTHS: [i32; 7] = [3840, 2560, 2304, 1920, 1680, 1440, 1280];
const REFERENCE: i64 = 1920 * 1200; // the size the bitrates are set for
pub const HIDPI_WIDTH: i32 = 2560; // from this width, the display is HiDPI

/// Whether the screen a tablet reports can be one: what follows trusts it.
pub fn valid(screen: Size) -> bool {
    (1..=16384).contains(&screen.0) && (1..=16384).contains(&screen.1)
}

/// Every size at the tablet's proportions, its own first.
pub fn all(screen: Size) -> Vec<Size> {
    let (width, height) = (screen.0.max(screen.1), screen.0.min(screen.1));
    let ratio = width as f64 / height as f64;
    let mut found = vec![(width, height)];
    for w in WIDTHS.into_iter().filter(|&w| w < width) {
        let h = ((w as f64 * height as f64 / width as f64 / 2.0).round() as i32) * 2;
        if (w as f64 / h as f64 - ratio).abs() < 0.005 * ratio {
            found.push((w, h));
        }
    }
    found
}

/// Samsung tablets by their panel (landscape): their names, and the width
/// Automatic uses over Wi-Fi, where the densest panels need more bandwidth
/// than most networks give.
const TABLETS: [(Size, &str, i32); 7] = [
    ((1340, 800), "Galaxy Tab A7 Lite, Tab A9", 1340),
    ((1920, 1200), "Galaxy Tab A8, Tab A9+", 1920),
    ((2000, 1200), "Galaxy Tab S6 Lite, Tab A7", 2000),
    ((2304, 1440), "Galaxy Tab S9 FE, Tab S10 FE", 1920),
    ((2560, 1600), "Galaxy Tab S6, S7, S7 FE, S8, S9, S9 FE+, S11", 1920),
    ((2800, 1752), "Galaxy Tab S7+, S8+, S9+, S10+", 1920),
    ((2960, 1848), "Galaxy Tab S8 Ultra, S9 Ultra, S10 Ultra, S11 Ultra", 1920),
];

pub fn models(screen: Size) -> Option<&'static str> {
    let key = (screen.0.max(screen.1), screen.0.min(screen.1));
    TABLETS.iter().find(|t| t.0 == key).map(|t| t.1)
}

/// Native over the cable; over Wi-Fi the table's width for this tablet, or
/// the size closest to 1920x1200.
pub fn auto(screen: Size, wifi: bool) -> Size {
    let sizes = all(screen);
    if !wifi {
        return sizes[0];
    }
    if let Some(known) = TABLETS.iter().find(|t| t.0 == sizes[0]) {
        if let Some(&size) = sizes.iter().find(|s| s.0 <= known.2) {
            return size;
        }
    }
    *sizes.iter().min_by_key(|s| (s.0 as i64 * s.1 as i64 - REFERENCE).abs()).unwrap()
}

/// "auto", "tablet" (native) or "WxH"; one not at the tablet's proportions: automatic.
pub fn choose(resolution: &str, screen: Size, wifi: bool) -> Size {
    let sizes = all(screen);
    if resolution == "tablet" {
        return sizes[0];
    }
    let asked: Vec<i32> = resolution.split('x').filter_map(|n| n.parse().ok()).collect();
    sizes.into_iter().find(|s| asked == [s.0, s.1]).unwrap_or_else(|| auto(screen, wifi))
}

/// size scaled down to fit the tablet in either orientation, even, never up:
/// a mirrored monitor, which the app letterboxes.
pub fn fit(size: Size, screen: Size) -> Size {
    let tablet = if size.0 >= size.1 { (screen.0.max(screen.1), screen.0.min(screen.1)) }
                 else { (screen.0.min(screen.1), screen.0.max(screen.1)) };
    let scale = 1f64.min(tablet.0 as f64 / size.0 as f64).min(tablet.1 as f64 / size.1 as f64);
    (((size.0 as f64 * scale / 2.0) as i32) * 2, ((size.1 as f64 * scale / 2.0) as i32) * 2)
}

pub fn scale(size: Size) -> f64 {
    if size.0 >= HIDPI_WIDTH { 2.0 } else { 1.0 }
}

/// The preset's bitrate at 1920x1200 and the largest frame in average frames.
pub fn priority(name: &str) -> (u32, f64) {
    match name {
        "latency" => (12000, 2.0),
        "clarity" => (40000, 8.0),
        _ => (20000, 4.0),
    }
}

/// kbps scaled up with the pixels, never down.
pub fn bitrate(kbps: u32, size: Size) -> u32 {
    (kbps as f64 * 1f64.max(size.0 as f64 * size.1 as f64 / REFERENCE as f64)) as u32
}

pub const MAX_FPS: u32 = 120;
/// Tab S6 Lite at 2000x1200, window to decoded: 31 ms at 60 fps, 23 at 100, 22 at 110,
/// 29 at 120, where its decoder cannot keep up. Its decoder does not say so.
const CABLE_FPS: u32 = 100;

/// The capture rate for a panel of refresh Hz (60 when it does not say),
/// and the decoder's rate at the size: above 60 the rate gives way, not the resolution.
/// On the cable, where the bandwidth is there, at least CABLE_FPS: a newer frame at each
/// refresh, and two clocks at one rate would stay locked at any phase.
pub fn fps(refresh: Option<f64>, decoder: Option<u32>, cable: bool) -> u32 {
    let mut fps = match refresh {
        Some(r) if r >= 30.0 => MAX_FPS.min(r.round() as u32),
        _ => 60,
    };
    if cable {
        fps = fps.max(CABLE_FPS);
    }
    if let Some(d) = decoder.filter(|&d| d < fps) {
        fps = d.max(60);
    }
    fps
}

/// Frames between keyframes: 1024 is the most VA's encoders take.
pub fn keyframe_interval(fps: u32, seconds: u32) -> u32 {
    fps.saturating_mul(seconds).min(1024)
}

/// The bitrate as a TCP window: down a third when frames arrive late or are
/// lost, up a tenth while they arrive on time, capped at the target.
pub struct Rate {
    pub target: u32,
    pub kbps: u32,
    medians: Vec<f64>,
    calm: u32,
    holding: u32,
}

pub const MIN_KBPS: u32 = 2000;
const LATE: f64 = 90.0; // a median above this many ms is late, whatever the link's best

impl Rate {
    /// Wi-Fi starts at half, so the start does not fill the queues.
    pub fn new(target: u32, wifi: bool) -> Rate {
        Rate { target, kbps: if wifi { MIN_KBPS.max(target / 2) } else { target }, medians: Vec::new(), calm: 0, holding: 0 }
    }

    /// A report: the new bitrate if it changed. On time means close to the best
    /// of recent reports, which absorbs the link's base latency.
    pub fn report(&mut self, median: f64, lost: u32) -> Option<u32> {
        self.medians.push(median);
        if self.medians.len() > 20 {
            self.medians.remove(0);
        }
        let best = self.medians.iter().cloned().fold(f64::MAX, f64::min);
        if self.holding > 0 {
            self.holding -= 1;
        } else if median > best + 60.0 || median > LATE || (lost > 0 && median > best + 25.0) {
            self.calm = 0;
            self.holding = 2;
            let next = MIN_KBPS.max(self.kbps * 2 / 3);
            if next < self.kbps {
                self.kbps = next;
                return Some(next);
            }
        } else if median < best + 30.0 {
            self.calm += 1;
            if self.calm >= 2 && self.kbps < self.target {
                self.calm = 0;
                self.kbps = self.target.min(self.kbps * 11 / 10);
                return Some(self.kbps);
            }
        } else {
            self.calm = 0;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_at_the_tablets_proportions() {
        assert_eq!(all((2000, 1200)), vec![(2000, 1200), (1920, 1152), (1680, 1008), (1440, 864), (1280, 768)]);
        assert_eq!(auto((2000, 1200), false), (2000, 1200));
        assert_eq!(auto((2800, 1752), true), (1920, 1202));
        assert_eq!(auto((2560, 1600), true), (1920, 1200));
        assert_eq!(auto((2400, 1500), true), (1920, 1200));
    }

    #[test]
    fn chosen_and_mirrored_sizes() {
        assert_eq!(choose("tablet", (2000, 1200), true), (2000, 1200));
        assert_eq!(choose("1440x864", (2000, 1200), true), (1440, 864));
        assert_eq!(choose("1440x900", (2000, 1200), true), (2000, 1200));
        assert_eq!(fit((2880, 1800), (2000, 1200)), (1920, 1200));
        assert_eq!(fit((1440, 900), (1200, 2000)), (1440, 900));
    }

    #[test]
    fn the_rate_follows_the_panel_and_the_decoder() {
        assert_eq!(fps(None, None, false), 60);
        assert_eq!(fps(Some(120.0), None, false), 120);
        assert_eq!(fps(Some(144.0), Some(90), false), 90);
        assert_eq!(fps(Some(120.0), Some(30), false), 60);
    }

    #[test]
    fn the_cable_captures_faster_than_the_panel() {
        assert_eq!(fps(Some(60.0), Some(487), true), CABLE_FPS);
        assert_eq!(fps(Some(120.0), Some(487), true), MAX_FPS);
        assert_eq!(fps(Some(60.0), Some(90), true), 90); // the decoder's limit stands
        assert_eq!(fps(Some(60.0), Some(487), false), 60);
    }

    #[test]
    fn bitrate_down_when_late_up_when_calm() {
        let mut rate = Rate::new(20000, true);
        assert_eq!(rate.kbps, 10000);
        assert_eq!(rate.report(20.0, 0), None);
        assert_eq!(rate.report(20.0, 0), Some(11000));
        assert_eq!(rate.report(100.0, 0), Some(7333));
        assert_eq!(rate.report(20.0, 0), None); // holding while the queues drain
    }
}
