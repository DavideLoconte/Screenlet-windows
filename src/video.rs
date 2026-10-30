//! The desktop to H.264 or HEVC: Desktop Duplication into Media Foundation.
use crate::link::Result;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::{E_FAIL, HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Variant::{VARIANT, VT_UI4};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

pub const KEYFRAME_SECONDS: u32 = 10;
const WAIT_MS: u32 = 100; // for a new frame, between looks at stop and keyframe
const ENCODER_WAIT_MS: u64 = 40; // for its frame, then on to the next: it comes with that one
const SETTLE_MS: u32 = 30; // the wait for a new frame while the screen may have just gone still
const SETTLE_FRAMES: u32 = 3; // the last picture again, to bring it out of an encoder that holds frames
const TARGETS: usize = 4;
const POINTER_MAX: u32 = 256;

fn ticks_to_ns(ticks: i64) -> i64 {
    let mut frequency = 0;
    let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
    (ticks as i128 * 1_000_000_000 / frequency.max(1) as i128) as i64
}

/// The clock of Windows' present times, in ns.
pub fn now() -> i64 {
    let mut ticks = 0;
    let _ = unsafe { QueryPerformanceCounter(&mut ticks) };
    ticks_to_ns(ticks)
}

/// The codecs this PC encodes in hardware, the preferred first: H.264, the quickest; HEVC
/// for clarity. H.264 alone, in software, when it has none (PROTOCOL.md §3).
pub fn codecs(clarity: bool) -> Vec<String> {
    let in_hardware = |subtype: GUID| unsafe {
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype };
        let mut list = std::ptr::null_mut();
        let mut count = 0;
        let looked = MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, None, Some(&output), &mut list, &mut count);
        if looked.is_ok() && !list.is_null() {
            for i in 0..count as usize { std::ptr::drop_in_place(list.add(i)); }
            CoTaskMemFree(Some(list as _));
        }
        looked.is_ok() && count > 0
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    }
    let mut found: Vec<String> = Vec::new();
    if in_hardware(MFVideoFormat_H264) { found.push("avc".into()); }
    if in_hardware(MFVideoFormat_HEVC) { found.push("hevc".into()); }
    if clarity { found.reverse(); }
    if found.is_empty() { found.push("avc".into()); }
    found
}

/// A screen that goes still must end on its last picture. That one may have been left out
/// (the network behind), and an encoder may hold its frame until the next comes, which on
/// a still screen never does: a window dragged away, or the pointer, would stay on the tablet.
#[derive(Default)]
struct Settling {
    left_out: bool,
    repeats: u32,
}

#[derive(Debug, PartialEq)]
enum Still {
    Nothing,
    Repeat,
    LeftOut, // the picture that was left out, at last
}

impl Settling {
    fn taken(&mut self) {
        self.left_out = false;
        self.repeats = SETTLE_FRAMES;
    }

    fn busy(&self) -> bool { self.left_out || self.repeats > 0 }

    /// No new frame came, and the network has room.
    fn still(&mut self) -> Still {
        if self.left_out {
            self.taken();
            return Still::LeftOut;
        }
        if self.repeats == 0 { return Still::Nothing; }
        self.repeats -= 1;
        Still::Repeat
    }
}

/// What goes to the tablet.
pub enum Source {
    /// A monitor by its device name, or the first.
    Screen(Option<String>),
    /// A moving test pattern of this size: the same load every run.
    Pattern((i32, i32)),
}

#[derive(Default)]
struct Control {
    stop: AtomicBool,
    paused: AtomicBool,
    keyframe: AtomicBool,
    kbps: AtomicU32,
    skipped: Arc<AtomicU32>,
}

pub struct Video {
    pub name: String,
    pub size: (i32, i32),   // encoded
    pub screen: (i32, i32), // the monitor's
    pub origin: (i32, i32), // of the monitor on the desktop
    /// Frames not encoded since this was last read: the network behind.
    pub skipped: Arc<AtomicU32>,
    control: Arc<Control>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

/// bytes, keyframe, captured, into the encoder (ns, now()'s clock).
pub type OnFrame = Box<dyn FnMut(&[u8], bool, i64, i64) + Send>;

impl Video {
    /// on_frame on the capture thread; skip() leaves a frame out.
    #[allow(clippy::too_many_arguments)]
    pub fn start(source: Source, codec: &str, fps: u32, kbps: u32, vbv_frames: f64, fit: (i32, i32), keyframes: u32, skip: Box<dyn Fn() -> bool + Send>, on_frame: OnFrame) -> Result<Video> {
        let control = Arc::new(Control { kbps: AtomicU32::new(kbps), ..Default::default() });
        let skipped = control.skipped.clone();
        let error = Arc::new(Mutex::new(None));
        let (ready_tx, ready) = mpsc::channel();
        let (c, e) = (control.clone(), error.clone());
        let subtype = if codec == "hevc" { MFVideoFormat_HEVC } else { MFVideoFormat_H264 };
        let thread = std::thread::Builder::new().name("screenlet-video".into()).spawn(move || {
            let result = unsafe { Capture::open(&source, subtype, fps, c.kbps.load(Ordering::Relaxed), vbv_frames, fit, if keyframes > 0 { keyframes } else { KEYFRAME_SECONDS }) };
            let mut capture = match result {
                Ok(capture) => {
                    let _ = ready_tx.send(Ok((capture.encoder.name.clone(), capture.size, (capture.screen.0 as i32, capture.screen.1 as i32), capture.origin)));
                    capture
                }
                Err(e) => { let _ = ready_tx.send(Err(e.to_string())); return; }
            };
            if let Err(failure) = unsafe { capture.run(&c, skip, on_frame, fps) } {
                *e.lock().unwrap() = Some(failure.to_string());
            }
        })?;
        let (name, size, screen, origin) = ready.recv().map_err(|_| "the capture thread ended")??;
        Ok(Video { name, size, screen, origin, skipped, control, error, thread: Some(thread) })
    }

    pub fn pause(&self, paused: bool) { self.control.paused.store(paused, Ordering::Relaxed); }
    pub fn keyframe(&self) { self.control.keyframe.store(true, Ordering::Relaxed); }
    pub fn rate(&self, kbps: u32) { self.control.kbps.store(kbps, Ordering::Relaxed); }
    pub fn error(&self) -> Option<String> { self.error.lock().unwrap().clone() }
}

impl Drop for Video {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

/// BGRA to NV12 at the same size (limited range, BT.601).
pub fn nv12(bgra: &[u8], pitch: usize, width: usize, height: usize, out: &mut [u8]) {
    let (luma, chroma) = out.split_at_mut(width * height);
    for y in 0..height {
        let row = &bgra[y * pitch..][..width * 4];
        for (x, p) in row.chunks_exact(4).enumerate() {
            let (b, g, r) = (p[0] as i32, p[1] as i32, p[2] as i32);
            luma[y * width + x] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
            if y % 2 == 0 && x % 2 == 0 {
                let at = y / 2 * width + x;
                chroma[at] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                chroma[at + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
            }
        }
    }
}

/// A BGRA test pattern for frame number frame: a diagonal gradient that moves, and a bar
/// that crosses it, so that every frame differs from the last everywhere.
pub fn pattern(picture: &mut [u8], width: usize, height: usize, frame: usize) {
    for (y, row) in picture.chunks_exact_mut(width * 4).take(height).enumerate() {
        for (x, p) in row.chunks_exact_mut(4).enumerate() {
            let bar = (x + frame * 7) % width < width / 16;
            let shade = ((x + y + frame * 3) & 0xff) as u8;
            p.copy_from_slice(&if bar { [255, 255, 255, 255] } else { [shade, shade / 2, 255 - shade, 255] });
        }
    }
}

/// The pointer's picture, as Desktop Duplication gives it.
#[derive(Default)]
pub struct Shape {
    pub kind: u32, // DXGI_OUTDUPL_POINTER_SHAPE_TYPE
    pub width: usize,
    pub height: usize,
    pub pitch: usize,
    pub bytes: Vec<u8>,
}

impl Shape {
    /// Desktop Duplication leaves the pointer out of its frames: over a BGRA picture,
    /// whose pixel (0, 0) is (left, top) of the pointer. The rest is the picture's bounds.
    pub fn draw(&self, picture: &mut [u8], pitch: usize, columns: usize, rows: usize, left: usize, top: usize) {
        const MONOCHROME: u32 = 1;
        const MASKED: u32 = 4;
        let height = if self.kind == MONOCHROME { self.height / 2 } else { self.height };
        for y in top..height.min(top + rows) {
            for x in left..self.width.min(left + columns) {
                let at = (y - top) * pitch + (x - left) * 4;
                let Some(pixel) = picture.get_mut(at..at + 4) else { return };
                if self.kind == MONOCHROME {
                    let bit = |row: usize| self.bytes[row * self.pitch + x / 8] >> (7 - x % 8) & 1;
                    let (and, xor) = (bit(y), bit(y + height));
                    for c in &mut pixel[..3] {
                        *c = (*c & if and == 1 { 0xff } else { 0 }) ^ if xor == 1 { 0xff } else { 0 };
                    }
                    continue;
                }
                let from = &self.bytes[y * self.pitch + x * 4..][..4];
                if self.kind == MASKED {
                    for i in 0..3 {
                        pixel[i] = if from[3] == 0 { from[i] } else { pixel[i] ^ from[i] };
                    }
                } else {
                    let alpha = from[3] as u32;
                    for i in 0..3 {
                        pixel[i] = ((from[i] as u32 * alpha + pixel[i] as u32 * (255 - alpha)) / 255) as u8;
                    }
                }
            }
        }
    }
}

/// What the encoder takes.
enum Picture<'a> {
    Memory(&'a [u8]),
    Texture(&'a ID3D11Texture2D),
}

struct Encoder {
    mft: IMFTransform,
    codec: Option<ICodecAPI>,
    name: String,
    time: i64,
    frame: i64, // 100 ns
    kbps: u32,
    vbv_bits: f64,                          // of the largest frame, for each kbit/s
    events: Option<IMFMediaEventGenerator>, // hardware encoders are asynchronous
    needed: u32,                            // inputs it asked for
}

/// What an encoder is opened for.
#[derive(Clone, Copy)]
struct Wanted {
    subtype: GUID,
    width: u32,
    height: u32,
    fps: u32,
    kbps: u32,
    vbv_frames: f64,
    keyframes: u32,
}

impl Encoder {
    /// Software, in system memory: every PC has it.
    unsafe fn software(wanted: Wanted) -> Result<Encoder> {
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: wanted.subtype };
        let mut list = std::ptr::null_mut();
        let mut count = 0;
        MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER, None, Some(&output), &mut list, &mut count)?;
        if count == 0 { return Err("no software encoder for this codec".into()); }
        Self::activate((*list).as_ref().ok_or(windows::core::Error::from(E_FAIL))?, None, wanted)
    }

    /// The GPU's own, on the adapter of the frames, taking its textures.
    unsafe fn hardware(device: &ID3D11Device, luid: LUID, wanted: Wanted) -> Result<Encoder> {
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: wanted.subtype };
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1)?;
        let attributes = attributes.ok_or("no attributes")?;
        let adapter = std::slice::from_raw_parts(&luid as *const LUID as *const u8, size_of::<LUID>());
        attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, adapter)?;
        let mut list = std::ptr::null_mut();
        let mut count = 0;
        MFTEnum2(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, None, Some(&output), &attributes, &mut list, &mut count)?;
        if count == 0 { return Err("no hardware encoder on this adapter".into()); }
        let (mut token, mut manager) = (0, None);
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.ok_or("no device manager")?;
        manager.ResetDevice(device, token)?;
        Self::activate((*list).as_ref().ok_or(windows::core::Error::from(E_FAIL))?, Some(&manager), wanted)
    }

    unsafe fn activate(activate: &IMFActivate, manager: Option<&IMFDXGIDeviceManager>, wanted: Wanted) -> Result<Encoder> {
        let Wanted { subtype, width, height, fps, kbps, vbv_frames, keyframes } = wanted;
        let mut text = windows::core::PWSTR::null();
        let mut length = 0;
        activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut text, &mut length)?;
        let name = text.to_string().unwrap_or_default();
        let mft: IMFTransform = activate.ActivateObject()?;
        let attributes = mft.GetAttributes().ok();
        let asynchronous = attributes.as_ref().is_some_and(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0);
        if let Some(a) = &attributes {
            let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            if asynchronous { a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?; }
        }
        if let Some(manager) = manager {
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
        }
        let codec = mft.cast::<ICodecAPI>().ok();
        let encoder = Encoder { mft: mft.clone(), codec, name, time: 0, frame: 10_000_000 / fps as i64, kbps, needed: 0,
            vbv_bits: 1000.0 / fps as f64 * vbv_frames, events: if asynchronous { Some(mft.cast()?) } else { None } };
        encoder.set(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32);
        encoder.set(&CODECAPI_AVEncMPVDefaultBPictureCount, 0);
        encoder.set(&CODECAPI_AVEncCommonBufferSize, (kbps as f64 * encoder.vbv_bits) as u32);
        let size = (width as u64) << 32 | height as u64;
        let rate = (fps as u64) << 32 | 1;
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, kbps * 1000)?;
        out.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
        out.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        let profile = if subtype == MFVideoFormat_HEVC { eAVEncH265VProfile_Main_420_8.0 } else { eAVEncH264VProfile_Base.0 };
        out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile as u32)?;
        out.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, crate::sizes::keyframe_interval(fps, keyframes))?;
        mft.SetOutputType(0, &out, 0)?;
        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        input.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
        input.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
        input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        mft.SetInputType(0, &input, 0)?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        Ok(encoder)
    }

    unsafe fn set(&self, what: &windows::core::GUID, value: u32) {
        if let Some(codec) = &self.codec {
            let mut variant = VARIANT::default();
            (*variant.Anonymous.Anonymous).vt = VT_UI4;
            (*variant.Anonymous.Anonymous).Anonymous.ulVal = value;
            let _ = codec.SetValue(what, &variant);
        }
    }

    unsafe fn output(&self, from: &IMFSample, out: &mut impl FnMut(&[u8], bool)) -> Result<()> {
        let key = from.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
        let data = from.ConvertToContiguousBuffer()?;
        let (mut at, mut length) = (std::ptr::null_mut(), 0);
        data.Lock(&mut at, None, Some(&mut length))?;
        out(std::slice::from_raw_parts(at, length as usize), key);
        data.Unlock()?;
        Ok(())
    }

    /// The asynchronous encoder's events, until it wants input (want_input) or has given
    /// a frame, or for wait at most: false when it did neither.
    unsafe fn pump(&mut self, want_input: bool, wait: Duration, out: &mut impl FnMut(&[u8], bool)) -> Result<bool> {
        let events = self.events.clone().ok_or("no events")?;
        let deadline = Instant::now() + wait;
        loop {
            if want_input && self.needed > 0 { return Ok(true); }
            let event = match events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                Ok(event) => event,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if Instant::now() >= deadline { return Ok(false); }
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let kind = event.GetType()?;
            if kind == METransformNeedInput.0 as u32 {
                self.needed += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                let mut outputs = [MFT_OUTPUT_DATA_BUFFER::default()]; // the encoder's sample
                let mut status = 0;
                let result = self.mft.ProcessOutput(0, &mut outputs, &mut status);
                let sample = std::mem::ManuallyDrop::take(&mut outputs[0].pSample);
                std::mem::ManuallyDrop::drop(&mut outputs[0].pEvents);
                if matches!(&result, Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE) {
                    // It settled its format with the first frame: taken, and it says again.
                    self.mft.SetOutputType(0, &self.mft.GetOutputAvailableType(0, 0)?, 0)?;
                    continue;
                }
                result?;
                if let Some(sample) = sample { self.output(&sample, out)?; }
                if !want_input { return Ok(true); }
            }
        }
    }

    unsafe fn encode(&mut self, picture: Picture, keyframe: bool, kbps: u32, mut out: impl FnMut(&[u8], bool)) -> Result<()> {
        if kbps != self.kbps {
            self.kbps = kbps;
            self.set(&CODECAPI_AVEncCommonMeanBitRate, kbps * 1000);
            self.set(&CODECAPI_AVEncCommonBufferSize, (kbps as f64 * self.vbv_bits) as u32);
        }
        if keyframe { self.set(&CODECAPI_AVEncVideoForceKeyFrame, 1); }
        let buffer = match picture {
            Picture::Memory(nv12) => {
                let buffer = MFCreateMemoryBuffer(nv12.len() as u32)?;
                let mut at = std::ptr::null_mut();
                buffer.Lock(&mut at, None, None)?;
                std::ptr::copy_nonoverlapping(nv12.as_ptr(), at, nv12.len());
                buffer.Unlock()?;
                buffer.SetCurrentLength(nv12.len() as u32)?;
                buffer
            }
            Picture::Texture(texture) => MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?,
        };
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(self.time)?;
        sample.SetSampleDuration(self.frame)?;
        self.time += self.frame;
        if self.events.is_some() {
            if !self.pump(true, Duration::from_millis(100), &mut out)? { return Ok(()); } // this frame goes
            self.needed -= 1;
            self.mft.ProcessInput(0, &sample, 0)?;
            self.pump(false, Duration::from_millis(ENCODER_WAIT_MS), &mut out)?;
            return Ok(());
        }
        self.mft.ProcessInput(0, &sample, 0)?;
        let room = self.mft.GetOutputStreamInfo(0)?.cbSize.max(1 << 20);
        loop {
            let sample = MFCreateSample()?;
            sample.AddBuffer(&MFCreateMemoryBuffer(room)?)?;
            let mut outputs = [MFT_OUTPUT_DATA_BUFFER { pSample: std::mem::ManuallyDrop::new(Some(sample.clone())), ..Default::default() }];
            let mut status = 0;
            let result = self.mft.ProcessOutput(0, &mut outputs, &mut status);
            std::mem::ManuallyDrop::drop(&mut outputs[0].pSample);
            std::mem::ManuallyDrop::drop(&mut outputs[0].pEvents);
            match result {
                Ok(()) => self.output(&sample, &mut out)?,
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.mft.SetOutputType(0, &self.mft.GetOutputAvailableType(0, 0)?, 0)?,
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// BGRA to NV12 on the GPU, scaled, into a few textures in turn: the encoder may still
/// read the last one when the next frame comes.
struct Gpu {
    video: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    targets: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    turn: usize,
}

impl Gpu {
    unsafe fn open(device: &ID3D11Device, context: &ID3D11DeviceContext, canvas: &ID3D11Texture2D, from: (u32, u32), to: (u32, u32), fps: u32) -> Result<Gpu> {
        let devices: ID3D11VideoDevice = device.cast()?;
        let video: ID3D11VideoContext = context.cast()?;
        let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC { InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate, InputWidth: from.0, InputHeight: from.1, OutputFrameRate: rate, OutputWidth: to.0, OutputHeight: to.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED };
        let enumerator = devices.CreateVideoProcessorEnumerator(&content)?;
        let processor = devices.CreateVideoProcessor(&enumerator, 0)?;
        let description = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC { ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D, ..Default::default() };
        let mut input = None;
        devices.CreateVideoProcessorInputView(canvas, &enumerator, &description, Some(&mut input))?;
        let texture = D3D11_TEXTURE2D_DESC { Width: to.0, Height: to.1, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32, ..Default::default() };
        let view = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC { ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D, ..Default::default() };
        let mut targets = Vec::new();
        for _ in 0..TARGETS {
            let (mut t, mut v) = (None, None);
            device.CreateTexture2D(&texture, None, Some(&mut t))?;
            let t = t.ok_or("no texture")?;
            devices.CreateVideoProcessorOutputView(&t, &enumerator, &view, Some(&mut v))?;
            targets.push((t, v.ok_or("no view")?));
        }
        Ok(Gpu { video, processor, input: input.ok_or("no view")?, targets, turn: 0 })
    }

    unsafe fn convert(&mut self) -> Result<ID3D11Texture2D> {
        let (texture, view) = &self.targets[self.turn];
        self.turn = (self.turn + 1) % self.targets.len();
        let stream = D3D11_VIDEO_PROCESSOR_STREAM { Enable: true.into(), pInputSurface: std::mem::ManuallyDrop::new(Some(self.input.clone())), ..Default::default() };
        let mut streams = [stream];
        let done = self.video.VideoProcessorBlt(&self.processor, view, 0, &streams);
        std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
        done?;
        Ok(texture.clone())
    }
}

struct Capture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplicating: Option<(IDXGIOutput1, IDXGIOutputDuplication)>, // none: the test pattern
    staging: ID3D11Texture2D,
    canvas: ID3D11Texture2D,
    under: ID3D11Texture2D,
    gpu: Option<Gpu>,
    encoder: Encoder,
    size: (i32, i32),  // encoded
    screen: (u32, u32),
    origin: (i32, i32),
    shape: Shape,
    pointer: Option<(i32, i32)>,
}

impl Capture {
    unsafe fn open(source: &Source, subtype: GUID, fps: u32, kbps: u32, vbv_frames: f64, fit: (i32, i32), keyframes: u32) -> Result<Capture> {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
        // The chosen monitor, or the first one, and its adapter.
        let shown = match source { Source::Screen(name) => name.as_deref(), Source::Pattern(_) => None };
        let factory = CreateDXGIFactory1::<IDXGIFactory1>()?;
        let mut found = None;
        let mut a = 0;
        'adapters: while let Ok(adapter) = factory.EnumAdapters1(a) {
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                let name = String::from_utf16_lossy(&output.GetDesc()?.DeviceName);
                if shown.is_none_or(|s| name.trim_end_matches('\0') == s) {
                    found = Some((adapter.cast::<IDXGIAdapter>()?, output));
                    break 'adapters;
                }
                o += 1;
            }
            a += 1;
        }
        let (adapter, output) = found.ok_or("no such monitor")?;
        let (mut device, mut context) = (None, None);
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        if D3D11CreateDevice(&adapter, D3D_DRIVER_TYPE_UNKNOWN, HMODULE::default(), flags, None, D3D11_SDK_VERSION,
            Some(&mut device), None, Some(&mut context)).is_err() {
            D3D11CreateDevice(&adapter, D3D_DRIVER_TYPE_UNKNOWN, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None,
                D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut context))?;
        }
        let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device.unwrap(), context.unwrap());
        let _ = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true); // the encoder uses it too
        let luid = adapter.GetDesc()?.AdapterLuid;
        let output = output.cast::<IDXGIOutput1>()?;
        let desktop = output.GetDesc()?.DesktopCoordinates;
        let origin = (desktop.left, desktop.top);
        let (duplicating, screen) = match source {
            Source::Pattern(size) => (None, (size.0 as u32 & !1, size.1 as u32 & !1)),
            Source::Screen(_) => {
                let duplication = output.DuplicateOutput(&device)?;
                let mode = duplication.GetDesc().ModeDesc;
                let screen = (mode.Width & !1, mode.Height & !1);
                (Some((output, duplication)), screen)
            }
        };
        let texture = |usage, access: u32, bind: u32, w, h| -> Result<ID3D11Texture2D> {
            let description = D3D11_TEXTURE2D_DESC { Width: w, Height: h, MipLevels: 1, ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: usage, CPUAccessFlags: access, BindFlags: bind, ..Default::default() };
            let mut t = None;
            device.CreateTexture2D(&description, None, Some(&mut t))?;
            t.ok_or_else(|| "no texture".into())
        };
        let read_write = (D3D11_CPU_ACCESS_READ.0 | D3D11_CPU_ACCESS_WRITE.0) as u32;
        let staging = texture(D3D11_USAGE_STAGING, read_write, 0, screen.0, screen.1)?;
        let under = texture(D3D11_USAGE_STAGING, read_write, 0, POINTER_MAX, POINTER_MAX)?;
        let canvas = texture(D3D11_USAGE_DEFAULT, 0, D3D11_BIND_RENDER_TARGET.0 as u32, screen.0, screen.1)?;
        let scaled = crate::sizes::fit((screen.0 as i32, screen.1 as i32), fit);
        let to = (scaled.0 as u32 & !1, scaled.1 as u32 & !1);
        // The GPU's way, if all of it is there; any failure and the frames take the CPU's.
        let wanted = Wanted { subtype, width: to.0, height: to.1, fps, kbps, vbv_frames, keyframes };
        let fast = Encoder::hardware(&device, luid, wanted)
            .and_then(|encoder| Ok((encoder, Gpu::open(&device, &context, &canvas, screen, to, fps)?)));
        let (encoder, gpu, size) = match fast {
            Ok((encoder, gpu)) => (encoder, Some(gpu), to),
            Err(e) => {
                eprintln!("video: no hardware path ({e}): software, at the screen's size");
                (Encoder::software(Wanted { width: screen.0, height: screen.1, ..wanted })?, None, screen)
            }
        };
        Ok(Capture { device, context, duplicating, staging, canvas, under, gpu, encoder,
            size: (size.0 as i32, size.1 as i32), screen, origin, shape: Shape::default(), pointer: None })
    }

    unsafe fn follow_pointer(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO) -> Result<()> {
        if info.LastMouseUpdateTime != 0 {
            let p = info.PointerPosition;
            self.pointer = p.Visible.as_bool().then_some((p.Position.x, p.Position.y));
        }
        if info.PointerShapeBufferSize > 0 {
            self.shape.bytes.resize(info.PointerShapeBufferSize as usize, 0);
            let (mut used, mut shape) = (0, DXGI_OUTDUPL_POINTER_SHAPE_INFO::default());
            let Some((_, duplication)) = &self.duplicating else { return Ok(()) };
            duplication.GetFramePointerShape(info.PointerShapeBufferSize, self.shape.bytes.as_mut_ptr() as _, &mut used, &mut shape)?;
            self.shape.kind = shape.Type;
            self.shape.width = shape.Width as usize;
            self.shape.height = shape.Height as usize;
            self.shape.pitch = shape.Pitch as usize;
        }
        Ok(())
    }

    /// The pointer into canvas: the little square under it to the CPU and back.
    unsafe fn draw_pointer(&self) -> Result<()> {
        let Some((x, y)) = self.pointer else { return Ok(()) };
        if self.shape.bytes.is_empty() { return Ok(()); }
        let (left, top) = (x.max(0) as u32, y.max(0) as u32);
        let right = (x + self.shape.width as i32).clamp(0, self.screen.0 as i32) as u32;
        let bottom = (y + self.shape.height as i32).clamp(0, self.screen.1 as i32) as u32;
        if right <= left || bottom <= top { return Ok(()); }
        let (columns, rows) = ((right - left).min(POINTER_MAX), (bottom - top).min(POINTER_MAX));
        let region = D3D11_BOX { left, top, front: 0, right: left + columns, bottom: top + rows, back: 1 };
        self.context.CopySubresourceRegion(&self.under, 0, 0, 0, 0, &self.canvas, 0, Some(&region));
        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        self.context.Map(&self.under, 0, D3D11_MAP_READ_WRITE, 0, Some(&mut map))?;
        let pitch = map.RowPitch as usize;
        let picture = std::slice::from_raw_parts_mut(map.pData as *mut u8, pitch * rows as usize);
        self.shape.draw(picture, pitch, columns as usize, rows as usize, (-x).max(0) as usize, (-y).max(0) as usize);
        self.context.Unmap(&self.under, 0);
        let back = D3D11_BOX { left: 0, top: 0, front: 0, right: columns, bottom: rows, back: 1 };
        self.context.CopySubresourceRegion(&self.canvas, 0, left, top, 0, &self.under, 0, Some(&back));
        Ok(())
    }

    /// The canvas into what the encoder takes.
    unsafe fn convert(&mut self, plane: &mut [u8], target: &mut Option<ID3D11Texture2D>) -> Result<()> {
        if let Some(gpu) = &mut self.gpu {
            *target = Some(gpu.convert()?);
            return Ok(());
        }
        let (width, height) = (self.size.0 as usize, self.size.1 as usize);
        self.context.CopyResource(&self.staging, &self.canvas);
        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        self.context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
        let pitch = map.RowPitch as usize;
        nv12(std::slice::from_raw_parts(map.pData as *const u8, pitch * height), pitch, width, height, plane);
        self.context.Unmap(&self.staging, 0);
        Ok(())
    }

    /// The test pattern at fps, instead of the desktop.
    unsafe fn run_pattern(&mut self, control: &Control, skip: Box<dyn Fn() -> bool + Send>, mut on_frame: OnFrame, fps: u32, plane: &mut [u8]) -> Result<()> {
        let (width, height) = (self.screen.0 as usize, self.screen.1 as usize);
        let mut picture = vec![0u8; width * height * 4];
        let mut target = None;
        let mut frame = 0;
        let started = Instant::now();
        while !control.stop.load(Ordering::Relaxed) {
            frame += 1;
            if let Some(wait) = (started + Duration::from_secs_f64(frame as f64 / fps as f64)).checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            if control.paused.load(Ordering::Relaxed) { continue; }
            if skip() {
                control.skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let captured = now();
            pattern(&mut picture, width, height, frame);
            self.context.UpdateSubresource(&self.canvas, 0, None, picture.as_ptr() as _, width as u32 * 4, 0);
            self.convert(plane, &mut target)?;
            let keyframe = control.keyframe.swap(false, Ordering::Relaxed);
            let into = now();
            let picture = match &target { Some(t) => Picture::Texture(t), None => Picture::Memory(plane) };
            self.encoder.encode(picture, keyframe, control.kbps.load(Ordering::Relaxed), |bytes, key| on_frame(bytes, key, captured, into))?;
        }
        Ok(())
    }

    unsafe fn run(&mut self, control: &Control, skip: Box<dyn Fn() -> bool + Send>, mut on_frame: OnFrame, fps: u32) -> Result<()> {
        let (width, height) = (self.size.0 as usize, self.size.1 as usize);
        let mut plane = vec![0u8; if self.gpu.is_some() { 0 } else { width * height * 3 / 2 }];
        let Some((output, mut duplication)) = self.duplicating.clone() else {
            return self.run_pattern(control, skip, on_frame, fps, &mut plane);
        };
        let mut target = None;
        let mut have = false;
        let mut captured = 0;
        let mut settling = Settling::default();
        while !control.stop.load(Ordering::Relaxed) {
            let (mut info, mut resource) = (DXGI_OUTDUPL_FRAME_INFO::default(), None);
            let wait = if settling.busy() { SETTLE_MS } else { WAIT_MS };
            let mut taken = false;
            let fresh = match duplication.AcquireNextFrame(wait, &mut info, &mut resource) {
                Ok(()) => info.LastPresentTime != 0 || info.LastMouseUpdateTime != 0,
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => false,
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    // The secure desktop, or the display mode: same size or the session ends.
                    duplication = output.DuplicateOutput(&self.device)?;
                    self.duplicating = Some((output.clone(), duplication.clone()));
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if let Some(resource) = resource {
                self.follow_pointer(&info)?;
                if fresh && !control.paused.load(Ordering::Relaxed) {
                    let time = if info.LastPresentTime != 0 { info.LastPresentTime } else { info.LastMouseUpdateTime };
                    captured = ticks_to_ns(time);
                    // On the canvas whatever comes next: a frame left out may be the last one.
                    let texture: ID3D11Texture2D = resource.cast()?;
                    let region = D3D11_BOX { left: 0, top: 0, front: 0, right: self.screen.0, bottom: self.screen.1, back: 1 };
                    self.context.CopySubresourceRegion(&self.canvas, 0, 0, 0, 0, &texture, 0, Some(&region));
                    self.draw_pointer()?;
                    taken = !skip();
                    if taken {
                        self.convert(&mut plane, &mut target)?;
                        have = true;
                        settling.taken();
                    } else {
                        settling.left_out = true;
                        control.skipped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                duplication.ReleaseFrame()?;
                if !fresh { continue; }
            }
            // On a still screen: the last frame again, for a keyframe or to settle.
            let paused = control.paused.load(Ordering::Relaxed);
            let still = if !fresh && !paused && settling.busy() && !skip() { settling.still() } else { Still::Nothing };
            if still == Still::LeftOut {
                self.convert(&mut plane, &mut target)?;
                have = true;
            }
            let keyframe = control.keyframe.swap(false, Ordering::Relaxed);
            if have && (taken || keyframe || still != Still::Nothing) && !paused {
                let at = if taken { captured } else { now() };
                let into = now();
                let picture = match &target { Some(t) => Picture::Texture(t), None => Picture::Memory(&plane) };
                self.encoder.encode(picture, keyframe, control.kbps.load(Ordering::Relaxed), |bytes, key| on_frame(bytes, key, at, into))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(kind: u32, pixel: [u8; 4]) -> Shape {
        Shape { kind, width: 2, height: 2, pitch: 8, bytes: pixel.repeat(4) }
    }

    #[test]
    fn a_screen_that_goes_still_ends_on_its_last_picture() {
        let mut settling = Settling::default();
        assert!(!settling.busy());
        assert_eq!(settling.still(), Still::Nothing);
        settling.taken();
        for _ in 0..SETTLE_FRAMES { assert_eq!(settling.still(), Still::Repeat); }
        assert!(!settling.busy());
        settling.left_out = true;
        assert_eq!(settling.still(), Still::LeftOut);
        assert!(settling.busy()); // and that one settles too
        assert_eq!(settling.still(), Still::Repeat);
    }

    #[test]
    fn a_colour_pointer_blends_by_its_alpha() {
        let mut picture = [100u8, 100, 100, 255].repeat(4);
        square(2, [200, 0, 100, 128]).draw(&mut picture, 8, 2, 2, 0, 0);
        assert_eq!(picture[..3], [150, 49, 100]);
    }

    #[test]
    fn a_masked_pointer_replaces_or_inverts() {
        let mut picture = [0x0fu8, 0x0f, 0x0f, 255].repeat(4);
        square(4, [1, 2, 3, 0]).draw(&mut picture, 8, 2, 2, 0, 0);
        assert_eq!(picture[..3], [1, 2, 3]);
        let mut picture = [0x0fu8, 0x0f, 0x0f, 255].repeat(4);
        square(4, [0xff, 0xff, 0xff, 0xff]).draw(&mut picture, 8, 2, 2, 0, 0);
        assert_eq!(picture[..3], [0xf0, 0xf0, 0xf0]);
    }

    #[test]
    fn a_monochrome_pointer_has_its_and_mask_above_its_xor_mask() {
        // Two pixels wide, one tall: and 10, xor 01, so the first keeps, the second is the xor over black.
        let shape = Shape { kind: 1, width: 2, height: 2, pitch: 1, bytes: vec![0b1000_0000, 0b0100_0000] };
        let mut picture = [9u8, 9, 9, 255, 9, 9, 9, 255];
        shape.draw(&mut picture, 8, 2, 1, 0, 0);
        assert_eq!(picture, [9, 9, 9, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn a_pointer_past_the_screens_edges_is_cut() {
        let mut picture = [0u8; 4];
        square(2, [255, 255, 255, 255]).draw(&mut picture, 4, 1, 1, 1, 1); // only its last pixel, in the corner
        assert_eq!(picture[..3], [255, 255, 255]);
    }
}
