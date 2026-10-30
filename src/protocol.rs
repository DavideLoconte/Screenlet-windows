//! Screenlet protocol 1 (PROTOCOL.md): constants, the pairing code, sealed
//! packets, video packetization, the app's STATS and INPUT bodies. Pure:
//! everything here is checked by the tests against vectors.json.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::digest;

pub const VERSION: i64 = 1;
pub const SERVICE: &str = "_screenlet._tcp.local.";
pub const PORT: u16 = 47236;
pub const UNPAIRED_MAX: usize = 4096;
pub const PAIRED_MAX: usize = 64 * 1024;

pub const VIDEO: u8 = 1;
pub const KEYFRAME: u8 = 2;
pub const PING: u8 = 3;
pub const PONG: u8 = 4;
pub const STATS: u8 = 5;
pub const INPUT: u8 = 6;

pub const FROM_PC: u32 = 1;
pub const FROM_APP: u32 = 2;
pub const NONCE: usize = 12;
pub const TAG: usize = 16;
pub const VIDEO_HEADER: usize = 18;
/// Video bytes per packet: with header, nonce and tag, one 1500-byte Ethernet frame.
pub const PAYLOAD: usize = 1472 - VIDEO_HEADER - NONCE - TAG;
/// In large packets (a 16 KiB datagram): fewer for the app to decrypt. Over
/// the cable always; over Wi-Fi until no frame arrives (IP fragments dropped).
pub const LARGE_PAYLOAD: usize = 16384 - VIDEO_HEADER - NONCE - TAG;
pub const STAGES: usize = 5;
pub const STATS_SIZE: usize = 1 + 8 + STAGES * 6;
/// An earlier app's STATS: capture->decoded and first packet->decoded.
pub const EARLIER_STATS_SIZE: usize = 1 + 8 + 12;
pub const MAX_PACKET: usize = NONCE + VIDEO_HEADER + LARGE_PAYLOAD + TAG;

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(text: &str) -> Option<Vec<u8>> {
    let digits = text.chars().filter(|c| !c.is_whitespace()).map(|c| c.to_digit(16)).collect::<Option<Vec<u32>>>()?;
    digits.len().is_multiple_of(2).then(|| digits.chunks(2).map(|d| (d[0] * 16 + d[1]) as u8).collect())
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut hash = digest::Context::new(&digest::SHA256);
    for part in parts {
        hash.update(part);
    }
    hash.finish().as_ref().try_into().expect("32 bytes")
}

pub fn fingerprint(der: &[u8]) -> String {
    hex(&sha256(&[der]))
}

/// The six digits both screens show: the two certificates (in either order) and both nonces.
pub fn pairing_code(a: &[u8], b: &[u8], pc_nonce: &[u8], app_nonce: &[u8]) -> String {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let d = sha256(&[lo, hi, pc_nonce, app_nonce]);
    format!("{:06}", u32::from_be_bytes([d[0], d[1], d[2], d[3]]) % 1_000_000)
}

/// The app's commitment to its nonce, sent before it sees ours.
pub fn commitment(app_nonce: &[u8], app_cert: &[u8], pc_cert: &[u8]) -> String {
    hex(&sha256(&[app_nonce, app_cert, pc_cert]))
}

fn cipher(key: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).expect("AES-256 key"))
}

/// AES-256-GCM for one direction's packets.
pub struct Sealer {
    key: LessSafeKey,
    direction: u32,
    count: u64,
}

impl Sealer {
    pub fn new(key: &[u8; 32], direction: u32) -> Sealer {
        Sealer { key: cipher(key), direction, count: 0 }
    }

    /// Seals body into out (at least body + NONCE + TAG long): the packet's length.
    pub fn seal(&mut self, body: &[u8], out: &mut [u8]) -> usize {
        let n = self.seal_with(self.count, body, out);
        self.count += 1;
        n
    }

    pub fn seal_with(&mut self, count: u64, body: &[u8], out: &mut [u8]) -> usize {
        out[..4].copy_from_slice(&self.direction.to_be_bytes());
        out[4..NONCE].copy_from_slice(&count.to_be_bytes());
        let (nonce, rest) = out.split_at_mut(NONCE);
        let nonce = Nonce::try_assume_unique_for_key(nonce).expect("12 bytes");
        rest[..body.len()].copy_from_slice(body);
        let tag = self.key.seal_in_place_separate_tag(nonce, Aad::empty(), &mut rest[..body.len()]).expect("seal");
        rest[body.len()..body.len() + TAG].copy_from_slice(tag.as_ref());
        NONCE + body.len() + TAG
    }
}

/// Opens the app's packets, each count once (a window of 64 for Wi-Fi's reordering).
pub struct Opener {
    key: LessSafeKey,
    direction: u32,
    replay: Replay,
}

impl Opener {
    pub fn new(key: &[u8; 32], direction: u32) -> Opener {
        Opener { key: cipher(key), direction, replay: Replay::default() }
    }

    /// The body of packet, written to out, or None if it is not one of the
    /// other side's sealed with this key, or a replay.
    pub fn open<'a>(&mut self, packet: &[u8], out: &'a mut [u8]) -> Option<&'a [u8]> {
        if packet.len() < NONCE + TAG + 1 || u32::from_be_bytes(packet[..4].try_into().ok()?) != self.direction {
            return None;
        }
        let count = u64::from_be_bytes(packet[4..NONCE].try_into().ok()?);
        let body = packet.len() - NONCE - TAG;
        if body + TAG > out.len() || !self.replay.fresh(count) {
            return None;
        }
        let nonce = Nonce::try_assume_unique_for_key(&packet[..NONCE]).ok()?;
        out[..body + TAG].copy_from_slice(&packet[NONCE..]);
        self.key.open_in_place(nonce, Aad::empty(), &mut out[..body + TAG]).ok()?;
        self.replay.accept(count);
        Some(&out[..body])
    }
}

/// The counts accepted: the highest and a window of 64 below it.
#[derive(Default)]
pub struct Replay {
    highest: Option<u64>,
    window: u64, // bit i: highest - i was accepted
}

impl Replay {
    pub fn fresh(&self, count: u64) -> bool {
        match self.highest {
            None => true,
            Some(h) if count > h => true,
            Some(h) => h - count < 64 && self.window & (1 << (h - count)) == 0,
        }
    }

    pub fn accept(&mut self, count: u64) {
        match self.highest {
            Some(h) if count <= h => self.window |= 1 << (h - count),
            _ => {
                let shift = self.highest.map_or(64, |h| count - h);
                self.window = if shift >= 64 { 1 } else { self.window << shift | 1 };
                self.highest = Some(count);
            }
        }
    }
}

/// The header of a frame's packet index of count.
pub fn video_header(keyframe: bool, frame: u32, index: u16, count: u16, captured: i64) -> [u8; VIDEO_HEADER] {
    let mut h = [0u8; VIDEO_HEADER];
    h[0] = VIDEO;
    h[1] = keyframe as u8;
    h[2..6].copy_from_slice(&frame.to_be_bytes());
    h[6..8].copy_from_slice(&index.to_be_bytes());
    h[8..10].copy_from_slice(&count.to_be_bytes());
    h[10..18].copy_from_slice(&captured.to_be_bytes());
    h
}

/// The app's report of a second: frames shown, lost, then STAGES × (median,
/// p90, max) in ms (PROTOCOL.md §3.5). From an earlier app only stage 0 is
/// known, and first packet -> decoded (decoding).
pub struct Stats {
    pub shown: u32,
    pub lost: u32,
    pub stages: [Option<[f64; 3]>; STAGES],
    pub decoding: f64,
}

impl Stats {
    pub fn parse(body: &[u8]) -> Option<Stats> {
        if (body.len() != STATS_SIZE && body.len() != EARLIER_STATS_SIZE) || body[0] != STATS {
            return None;
        }
        let triple = |s: usize| {
            let v = |q: usize| u16::from_be_bytes([body[9 + s * 6 + q * 2], body[10 + s * 6 + q * 2]]) as f64 / 10.0;
            [v(0), v(1), v(2)]
        };
        let mut stages = [None; STAGES];
        let decoding = if body.len() == STATS_SIZE {
            for (s, stage) in stages.iter_mut().enumerate() {
                *stage = Some(triple(s));
            }
            (2..STAGES).map(|s| triple(s)[0]).sum()
        } else {
            stages[0] = Some(triple(0));
            triple(1)[0]
        };
        Some(Stats {
            shown: u32::from_be_bytes(body[1..5].try_into().ok()?),
            lost: u32::from_be_bytes(body[5..9].try_into().ok()?),
            stages,
            decoding,
        })
    }
}

pub const PEN_SAMPLE: usize = 9;
pub const MAX_FINGERS: usize = 10;
pub const TOUCH_MAX: f64 = 65535.0;

/// An INPUT body: the fingers down (id, x and y in 0..1) and the pen's samples.
pub struct Input<'a> {
    pub fingers: Vec<(u8, f64, f64)>,
    pub pen: &'a [u8],
}

impl<'a> Input<'a> {
    pub fn parse(body: &'a [u8]) -> Option<Input<'a>> {
        if body.len() < 3 || body[0] != INPUT {
            return None;
        }
        let count = body[1] as usize;
        let at = 2 + count * 5;
        if count > MAX_FINGERS || body.len() < at + 1 {
            return None;
        }
        let samples = body[at] as usize;
        if body.len() != at + 1 + samples * PEN_SAMPLE {
            return None;
        }
        let fingers = (0..count)
            .map(|i| {
                let f = &body[2 + i * 5..];
                let x = u16::from_be_bytes([f[1], f[2]]) as f64 / TOUCH_MAX;
                let y = u16::from_be_bytes([f[3], f[4]]) as f64 / TOUCH_MAX;
                (f[0], x, y)
            })
            .collect();
        Some(Input { fingers, pen: &body[at + 1..] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn vectors() -> Value {
        serde_json::from_str(include_str!("../vectors.json")).unwrap()
    }

    fn bytes(v: &Value, section: &str, key: &str) -> Vec<u8> {
        unhex(v[section][key].as_str().unwrap()).unwrap()
    }

    #[test]
    fn pairing_code_and_commitment() {
        let v = vectors();
        let code = pairing_code(&bytes(&v, "pairing", "cert_b"), &bytes(&v, "pairing", "cert_a"),
                                &bytes(&v, "pairing", "pc_nonce"), &bytes(&v, "pairing", "app_nonce"));
        assert_eq!(code, v["pairing"]["code"].as_str().unwrap());
        let commit = commitment(&bytes(&v, "commitment", "app_nonce"), &bytes(&v, "commitment", "app_cert"),
                                &bytes(&v, "commitment", "pc_cert"));
        assert_eq!(commit, v["commitment"]["commit"].as_str().unwrap());
    }

    #[test]
    fn seals_the_vector_and_opens_it_once() {
        let v = vectors();
        let key: [u8; 32] = bytes(&v, "sealed", "key").try_into().unwrap();
        let body = bytes(&v, "sealed", "body");
        let mut out = [0u8; 2048];
        let n = Sealer::new(&key, FROM_PC).seal_with(v["sealed"]["count"].as_u64().unwrap(), &body, &mut out);
        assert_eq!(&out[..n], &bytes(&v, "sealed", "packet")[..]);
        let packet = out[..n].to_vec();
        let mut opener = Opener::new(&key, FROM_PC);
        let mut plain = [0u8; 2048];
        assert_eq!(opener.open(&packet, &mut plain), Some(&body[..]));
        assert_eq!(opener.open(&packet, &mut plain), None); // replayed
        let mut tampered = packet.clone();
        tampered[20] ^= 1;
        assert_eq!(Opener::new(&key, FROM_PC).open(&tampered, &mut plain), None);
        assert_eq!(Opener::new(&key, FROM_APP).open(&packet, &mut plain), None); // our own direction
    }

    #[test]
    fn unhex_takes_only_hex() {
        assert_eq!(unhex("0a F\nf"), Some(vec![0x0a, 0xff]));
        for text in ["0", "+a", "0ض", "zz"] {
            assert_eq!(unhex(text), None, "{text}");
        }
    }

    #[test]
    fn replay_window() {
        let mut r = Replay::default();
        for c in [0, 2, 1, 70] {
            assert!(r.fresh(c));
            r.accept(c);
        }
        assert!(!r.fresh(1) && !r.fresh(70) && !r.fresh(6));
        assert!(r.fresh(69) && r.fresh(71));
    }

    #[test]
    fn input_vector() {
        let body = bytes(&vectors(), "input", "body");
        let input = Input::parse(&body).unwrap();
        assert_eq!(input.fingers.len(), 1);
        let (id, x, y) = input.fingers[0];
        assert_eq!(id, 3);
        assert!((x - 0.5).abs() < 1e-3 && (y - 0.25).abs() < 1e-3);
        assert_eq!(input.pen.len(), PEN_SAMPLE);
        assert_eq!(input.pen[0], 0x11);
        assert!(Input::parse(&[INPUT, 11, 0]).is_none()); // more fingers than a hand
        assert!(Input::parse(&body[..body.len() - 1]).is_none()); // truncated
    }

    #[test]
    fn stats_stage_by_stage() {
        let mut body = vec![STATS];
        body.extend(60u32.to_be_bytes());
        body.extend(1u32.to_be_bytes());
        for s in 0..STAGES as u16 {
            for q in 0..3u16 {
                body.extend((s * 10 + q).to_be_bytes());
            }
        }
        let stats = Stats::parse(&body).unwrap();
        assert_eq!((stats.shown, stats.lost), (60, 1));
        assert_eq!(stats.stages[4], Some([4.0, 4.1, 4.2]));
        let earlier = Stats::parse(&body[..EARLIER_STATS_SIZE]).unwrap();
        assert_eq!((earlier.stages[0], earlier.stages[1], earlier.decoding), (Some([0.0, 0.1, 0.2]), None, 1.0));
    }

    #[test]
    fn packets_fill_one_ethernet_frame_or_16_kib() {
        assert_eq!(NONCE + VIDEO_HEADER + PAYLOAD + TAG, 1472);
        assert_eq!(MAX_PACKET, 16384);
        assert_eq!(video_header(true, 7, 1, 2, 9)[..10], [VIDEO, 1, 0, 0, 0, 7, 0, 1, 0, 2]);
    }

    /// The cost of sealing one full packet, printed and bounded.
    #[test]
    fn sealing_cost_per_packet() {
        let mut sealer = Sealer::new(&[7; 32], FROM_PC);
        let body = [1u8; VIDEO_HEADER + PAYLOAD];
        let mut out = [0u8; 2048];
        let n = 20_000;
        let start = std::time::Instant::now();
        for _ in 0..n {
            sealer.seal(&body, &mut out);
        }
        let ns = start.elapsed().as_nanos() / n;
        println!("sealing: {ns} ns per 1.4 KB packet");
        assert!(ns < 20_000, "{ns} ns per packet");
    }
}
