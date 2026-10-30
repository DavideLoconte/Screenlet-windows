//! The tablet's fingers and pen as Windows' own touch and pen, with pressure and
//! tilt. Press and hold is the right click, as on any Windows touch screen.
use crate::protocol::{Input as Packet, PEN_SAMPLE};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::Input::Pointer::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const SILENCE: Duration = Duration::from_millis(400); // the app repeats its state every 0.1 s
const SLOTS: usize = 10;
const PEN_MAX: f64 = 32767.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Phase { Down, Move, Up }

type At = (f64, f64);

/// Windows' pointer ids for the app's: each finger a slot for as long as it is down.
#[derive(Default)]
pub struct Slots([Option<(u8, At)>; SLOTS]);

impl Slots {
    /// The frames for this state, every contact in each: first what lifted (Windows
    /// wants no lift and press in one frame), then what is down.
    pub fn frames(&mut self, fingers: &[(u8, At)]) -> Vec<Vec<(u32, Phase, At)>> {
        let mut out = Vec::new();
        if self.0.iter().flatten().any(|(id, _)| fingers.iter().all(|f| f.0 != *id)) {
            let mut lifting = Vec::new();
            for (slot, contact) in self.0.iter_mut().enumerate() {
                if let Some((id, at)) = *contact {
                    let gone = fingers.iter().all(|f| f.0 != id);
                    lifting.push((slot as u32, if gone { Phase::Up } else { Phase::Move }, at));
                    if gone { *contact = None; }
                }
            }
            out.push(lifting);
        }
        let mut pressing = Vec::new();
        for &(id, at) in fingers {
            if let Some(slot) = self.0.iter().position(|c| matches!(c, Some((known, _)) if *known == id)) {
                self.0[slot] = Some((id, at));
                pressing.push((slot as u32, Phase::Move, at));
            } else if let Some(slot) = self.0.iter().position(Option::is_none) {
                self.0[slot] = Some((id, at));
                pressing.push((slot as u32, Phase::Down, at));
            }
        }
        if !pressing.is_empty() { out.push(pressing); }
        out
    }

    fn empty(&self) -> bool { self.0.iter().all(Option::is_none) }
}

/// The pen's pointer flags from where it was to where it is; None while out of range.
pub fn pen_flags(was: (bool, bool), near: bool, tip: bool) -> Option<POINTER_FLAGS> {
    let tip = near && tip;
    let range = if near { POINTER_FLAG_INRANGE } else { POINTER_FLAGS(0) };
    match (was.1, tip) {
        (false, true) => Some(POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT),
        (true, true) => Some(POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT),
        (true, false) => Some(POINTER_FLAG_UP | range),
        (false, false) => (was.0 || near).then_some(POINTER_FLAG_UPDATE | range),
    }
}

struct Device(HSYNTHETICPOINTERDEVICE);
// Handles of Windows, used from the one feedback thread.
unsafe impl Send for Device {}

impl Device {
    fn open(kind: POINTER_INPUT_TYPE, contacts: u32) -> Option<Device> {
        match unsafe { CreateSyntheticPointerDevice(kind, contacts, POINTER_FEEDBACK_DEFAULT) } {
            Ok(device) => Some(Device(device)),
            Err(e) => { eprintln!("input: no synthetic pointer device: {e}"); None }
        }
    }

    fn inject(&self, frame: &[POINTER_TYPE_INFO]) {
        if let Err(e) = unsafe { InjectSyntheticPointerInput(self.0, frame) } {
            eprintln!("input: {e}");
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) { unsafe { DestroySyntheticPointerDevice(self.0) } }
}

fn pointer(kind: POINTER_INPUT_TYPE, id: u32, flags: POINTER_FLAGS, x: f64, y: f64) -> POINTER_INFO {
    POINTER_INFO { pointerType: kind, pointerId: id, pointerFlags: flags,
        ptPixelLocation: POINT { x: x as i32, y: y as i32 }, ..Default::default() }
}

pub struct Input {
    touch: Option<Device>,
    pen: Option<Device>,
    origin: (i32, i32),
    size: (f64, f64),
    slots: Slots,
    pen_was: (bool, bool), // in range, on the screen
    pen_at: (f64, f64),
    heard: Instant,
}

impl Input {
    /// size and origin: the mirrored monitor's, in desktop pixels.
    pub fn new(size: (i32, i32), origin: (i32, i32)) -> Input {
        Input { touch: Device::open(PT_TOUCH, SLOTS as u32), pen: Device::open(PT_PEN, 1), origin,
            size: (size.0 as f64, size.1 as f64), slots: Slots::default(), pen_was: (false, false),
            pen_at: (0.0, 0.0), heard: Instant::now() }
    }

    pub fn busy(&self) -> bool { !self.slots.empty() || self.pen_was.1 }

    /// An INPUT body; false if malformed.
    pub fn packet(&mut self, body: &[u8]) -> bool {
        let Some(input) = Packet::parse(body) else { return false };
        self.heard = Instant::now();
        let fingers: Vec<(u8, At)> = input.fingers.iter().map(|&(id, x, y)| (id, (x * self.size.0, y * self.size.1))).collect();
        self.touches(&fingers);
        for sample in input.pen.as_chunks::<PEN_SAMPLE>().0 {
            self.pen(sample);
        }
        true
    }

    fn touches(&mut self, fingers: &[(u8, At)]) {
        for frame in self.slots.frames(fingers) {
            let frame: Vec<POINTER_TYPE_INFO> = frame.iter().map(|&(slot, phase, (x, y))| {
                let flags = match phase {
                    Phase::Down => POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
                    Phase::Move => POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
                    Phase::Up => POINTER_FLAG_UP,
                };
                let info = pointer(PT_TOUCH, slot, flags, x + self.origin.0 as f64, y + self.origin.1 as f64);
                POINTER_TYPE_INFO { r#type: PT_TOUCH, Anonymous: POINTER_TYPE_INFO_0 {
                    touchInfo: POINTER_TOUCH_INFO { pointerInfo: info, ..Default::default() } } }
            }).collect();
            if let Some(device) = &self.touch { device.inject(&frame); }
        }
    }

    fn pen(&mut self, sample: &[u8]) {
        let bits = sample[0];
        let (near, tip) = (bits & 0x10 != 0, bits & 1 != 0);
        if near {
            let axis = |low, high| (u16::from_le_bytes([low, high]) as f64 / PEN_MAX).min(1.0);
            self.pen_at = (axis(sample[1], sample[2]) * self.size.0, axis(sample[3], sample[4]) * self.size.1);
        }
        let Some(flags) = pen_flags(self.pen_was, near, tip) else { return };
        self.pen_was = (near, near && tip);
        let mut pen_flags = PEN_FLAG_NONE;
        if bits & 2 != 0 { pen_flags |= PEN_FLAG_BARREL; }
        if bits & 8 != 0 { pen_flags |= PEN_FLAG_ERASER; }
        let info = pointer(PT_PEN, 0, flags, self.pen_at.0 + self.origin.0 as f64, self.pen_at.1 + self.origin.1 as f64);
        let pressure = u16::from_le_bytes([sample[5], sample[6]]) as u32 * 1024 / 4096;
        let frame = [POINTER_TYPE_INFO { r#type: PT_PEN, Anonymous: POINTER_TYPE_INFO_0 {
            penInfo: POINTER_PEN_INFO { pointerInfo: info, penFlags: pen_flags,
                penMask: PEN_MASK_PRESSURE | PEN_MASK_TILT_X | PEN_MASK_TILT_Y, pressure: pressure.min(1024),
                tiltX: (sample[7] as i8 as i32).clamp(-90, 90), tiltY: (sample[8] as i8 as i32).clamp(-90, 90), ..Default::default() } } }];
        if let Some(device) = &self.pen { device.inject(&frame); }
    }

    /// Every few ms: the tablet gone quiet, everything lifts.
    pub fn check(&mut self) {
        if self.heard.elapsed() >= SILENCE && self.busy() { self.close(); }
    }

    pub fn close(&mut self) {
        self.touches(&[]);
        self.pen(&[0; PEN_SAMPLE]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingers_keep_a_slot_and_lift_in_a_frame_of_their_own() {
        let mut s = Slots::default();
        assert_eq!(s.frames(&[(7, (1.0, 1.0))]), [vec![(0, Phase::Down, (1.0, 1.0))]]);
        assert_eq!(s.frames(&[(7, (2.0, 1.0)), (9, (5.0, 5.0))]),
                   [vec![(0, Phase::Move, (2.0, 1.0)), (1, Phase::Down, (5.0, 5.0))]]);
        assert_eq!(s.frames(&[(9, (5.0, 6.0)), (3, (8.0, 8.0))]),
                   [vec![(0, Phase::Up, (2.0, 1.0)), (1, Phase::Move, (5.0, 5.0))],
                    vec![(1, Phase::Move, (5.0, 6.0)), (0, Phase::Down, (8.0, 8.0))]]);
        assert_eq!(s.frames(&[]), [vec![(0, Phase::Up, (8.0, 8.0)), (1, Phase::Up, (5.0, 6.0))]]);
        assert!(s.empty() && s.frames(&[]).is_empty());
    }

    #[test]
    fn more_fingers_than_slots_are_left_out() {
        let mut s = Slots::default();
        let many: Vec<(u8, At)> = (0..12).map(|i| (i, (0.0, 0.0))).collect();
        assert_eq!(s.frames(&many)[0].len(), SLOTS);
    }

    #[test]
    fn the_pen_hovers_touches_and_leaves() {
        let (down, update, up) = (POINTER_FLAG_DOWN, POINTER_FLAG_UPDATE, POINTER_FLAG_UP);
        let (range, contact) = (POINTER_FLAG_INRANGE, POINTER_FLAG_INCONTACT);
        assert_eq!(pen_flags((false, false), false, false), None);
        assert_eq!(pen_flags((false, false), true, false), Some(update | range));
        assert_eq!(pen_flags((true, false), true, true), Some(down | range | contact));
        assert_eq!(pen_flags((true, true), true, true), Some(update | range | contact));
        assert_eq!(pen_flags((true, true), true, false), Some(up | range));
        assert_eq!(pen_flags((true, true), false, false), Some(up));
        assert_eq!(pen_flags((true, false), false, false), Some(update));
    }
}
