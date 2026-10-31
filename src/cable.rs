//! The tablet over the USB cable (PROTOCOL.md §4, Android Open Accessory): this PC is the
//! USB host, the tablet its accessory, one bulk stream each way. The link's TLS connections
//! ride in it as numbered streams and the display's sealed datagrams as frames of their
//! own. The cable is trusted with nothing: TLS and AES-GCM run end to end.
//!
//! Windows lets a program reach a USB device only through WinUSB, Microsoft's own driver.
//! The tablet as an accessory gets it from an INF this PC signs for itself (install). Before
//! that, the requests that make it one go through whichever of its functions has WinUSB:
//! its debugging interface, with USB debugging on, or else its files' function, which the
//! service hands to WinUSB for those few seconds. An accessory has no files to show anyway.
//! When the app hangs up on the service, however it ends, the service cycles the tablet's
//! USB port, as if the cable had been pulled out: it comes back as it was, files and all.
use crate::display::{self, hidden, published};
use crate::link::Result;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{GUID, HSTRING, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Devices::Usb::*;
use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::IO::DeviceIoControl;

pub const HELLO: u8 = 0;
pub const OPEN: u8 = 1;
pub const DATA: u8 = 2;
pub const CLOSE: u8 = 3;
pub const DATAGRAM: u8 = 4;
const HEADER: usize = 6;
const READ: usize = 16 * 1024;     // the app's transfer size
const BATCH: usize = 48 * 1024;    // the most the app takes in one transfer
const MAX_FRAME: usize = 1 << 20;
const IDENTITY: [&str; 6] = ["Screenlet", "Screenlet", "Screenlet on the PC", "1",
                             "https://github.com/DavideLoconte/Screenlet-Companion", ""];

const GOOGLE: u16 = 0x18D1;
/// Android tablets' makers: Samsung, Google, Lenovo, Xiaomi, OnePlus, Motorola,
/// Huawei, OPPO/realme, Honor, Amazon, Asus, Sony, TCL, Nothing.
pub const ANDROID_VENDORS: [u16; 14] = [0x04E8, 0x18D1, 0x17EF, 0x2717, 0x2A70, 0x22B8, 0x12D1, 0x22D9,
                                        0x339B, 0x1949, 0x0B05, 0x0FCE, 0x1BBB, 0x2B0E];
/// The device interface Android's debugging drivers register with WinUSB.
const ADB: GUID = GUID::from_u128(0xF72FE0D4_CBCB_407D_8814_9ED673D0DD6B);
/// The one this program's own INFs register.
const ACCESSORY: GUID = GUID::from_u128(0x7C1D5F3A_6C4E_4F0B_9B7A_3E2A5D6C8F10);
/// What a tablet shows as, for its files.
const PORTABLE: GUID = GUID::from_u128(0x6AC27878_A6FA_4155_BA85_F98F491D4F33);

const ACCESSORY_INF: &str = "screenlet-usb";
const TABLET_INF: &str = "screenlet-tablet"; // and the tablet's ids: one INF for each kind
const PIPE: &str = r"\\.\pipe\ScreenletCable";
/// Every USB hub's interface.
const HUB: &str = "{f18a0e88-c30c-11d0-8815-00a0c906bed8}";
const IOCTL_USB_HUB_CYCLE_PORT: u32 = 0x0022_0444;
const SIGNER: &str = "CN=Screenlet USB on this PC";

fn is_accessory(vendor: u16, product: u16) -> bool {
    vendor == GOOGLE && (0x2D00..=0x2D05).contains(&product)
}

/// A frame: kind | connection | length u32 | data.
pub fn frame(kind: u8, connection: u8, data: &[u8], out: &mut Vec<u8>) {
    out.push(kind);
    out.push(connection);
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
}

/// The whole frames at the start of buffer, removed from it.
pub fn frames(buffer: &mut Vec<u8>) -> Result<Vec<(u8, u8, Vec<u8>)>> {
    let mut found = Vec::new();
    let mut at = 0;
    while buffer.len() - at >= HEADER {
        let length = u32::from_be_bytes(buffer[at + 2..at + 6].try_into()?) as usize;
        if length > MAX_FRAME {
            return Err(format!("the tablet sent a frame of {length} bytes").into());
        }
        if buffer.len() - at < HEADER + length {
            break;
        }
        found.push((buffer[at], buffer[at + 1], buffer[at + HEADER..at + HEADER + length].to_vec()));
        at += HEADER + length;
    }
    buffer.drain(..at);
    Ok(found)
}

/// The vendor and product in a device path (\\?\usb#vid_04e8&pid_6860&mi_03#...).
pub fn ids(path: &str) -> Option<(u16, u16)> {
    let path = path.to_lowercase();
    let number = |key: &str| u16::from_str_radix(path.get(path.find(key)? + key.len()..)?.get(..4)?, 16).ok();
    Some((number("vid_")?, number("pid_")?))
}

/// The paths of the devices with this interface, plugged in.
fn devices(interface: &GUID) -> Vec<String> {
    let mut size = 0;
    unsafe {
        if CM_Get_Device_Interface_List_SizeW(&mut size, interface, PCWSTR::null(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT) != CR_SUCCESS { return Vec::new(); }
        let mut list = vec![0u16; size as usize];
        if CM_Get_Device_Interface_ListW(interface, PCWSTR::null(), &mut list, CM_GET_DEVICE_INTERFACE_LIST_PRESENT) != CR_SUCCESS { return Vec::new(); }
        list.split(|u| *u == 0).filter(|p| !p.is_empty()).map(String::from_utf16_lossy).collect()
    }
}

/// The INF that gives these devices (hardware ids) to WinUSB, under the interface above.
fn inf(ids: &[String], catalog: &str) -> String {
    let devices: String = ids.iter().map(|id| format!("%Name%=Device, {id}\r\n")).collect();
    format!("[Version]\r\nSignature=\"$Windows NT$\"\r\nClass=USBDevice\r\nClassGuid={{88BAE032-5A81-49F0-BC3D-A4FF138216D6}}\r\n\
        Provider=%Provider%\r\nDriverVer=10/30/2026,1.0.0.0\r\nCatalogFile={catalog}\r\nPnpLockdown=1\r\n\r\n\
        [Manufacturer]\r\n%Provider%=Devices,NTamd64\r\n\r\n[Devices.NTamd64]\r\n{devices}\r\n\
        [Device.NT]\r\nInclude=winusb.inf\r\nNeeds=WINUSB.NT\r\n\r\n[Device.NT.Services]\r\nInclude=winusb.inf\r\nNeeds=WINUSB.NT.Services\r\n\r\n\
        [Device.NT.HW]\r\nAddReg=Device.AddReg\r\n\r\n[Device.AddReg]\r\nHKR,,DeviceInterfaceGUIDs,0x10000,\"{{7C1D5F3A-6C4E-4F0B-9B7A-3E2A5D6C8F10}}\"\r\n\r\n\
        [Strings]\r\nProvider=\"Screenlet\"\r\nName=\"Screenlet USB\"\r\n")
}

/// An INF in a folder of its own, signed by a certificate made here and now, trusted by this
/// PC alone, whose key is gone once it has signed: a catalog is all Windows asks for, and
/// the driver it names is Microsoft's. Installed, with pnputil.
fn sign_and_add(stem: &str, ids: &[String]) -> Result<()> {
    let folder = folder(stem);
    let catalog = format!("{stem}.cat");
    if !folder.join(&catalog).exists() { sign(&folder, stem, ids, &catalog)?; }
    let done = hidden("pnputil").arg("/add-driver").arg(folder.join(format!("{stem}.inf"))).arg("/install").status()?;
    if !done.success() && !matches!(done.code(), Some(3010 | 259)) { return Err(format!("pnputil ended with {done}").into()); }
    Ok(())
}

fn sign(folder: &std::path::Path, stem: &str, ids: &[String], catalog: &str) -> Result<()> {
    let _ = std::fs::remove_dir_all(folder);
    std::fs::create_dir_all(folder)?;
    std::fs::write(folder.join(format!("{stem}.inf")), inf(ids, catalog))?;
    let script = format!(r#"$ErrorActionPreference='Stop'
$c = New-SelfSignedCertificate -Type CodeSigningCert -Subject '{SIGNER}' -CertStoreLocation Cert:\LocalMachine\My -KeyExportPolicy NonExportable -NotAfter (Get-Date).AddYears(30)
$public = [Security.Cryptography.X509Certificates.X509Certificate2]::new($c.RawData)
foreach ($name in 'Root','TrustedPublisher') {{ $s = [Security.Cryptography.X509Certificates.X509Store]::new($name,'LocalMachine'); $s.Open('ReadWrite'); $s.Add($public); $s.Close() }}
$cat = '{}'
New-FileCatalog -Path '{}' -CatalogFilePath $cat -CatalogVersion 2.0 | Out-Null
$signed = Set-AuthenticodeSignature -FilePath $cat -Certificate $c -HashAlgorithm SHA256
Remove-Item "Cert:\LocalMachine\My\$($c.Thumbprint)" -DeleteKey
if ($signed.Status -ne 'Valid') {{ throw $signed.StatusMessage }}"#, folder.join(catalog).display(), folder.display());
    let done = hidden("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).output()?;
    if !done.status.success() { return Err(format!("signing: {}", String::from_utf8_lossy(&done.stderr)).into()); }
    Ok(())
}

/// As administrator (--install-cable): the tablet as an accessory, for WinUSB.
pub fn install() -> Result<()> {
    sign_and_add(ACCESSORY_INF, &[r"USB\VID_18D1&PID_2D00".to_string(), r"USB\VID_18D1&PID_2D01&MI_00".to_string()])
}

/// The hardware id in a device path: \\?\usb#vid_04e8&pid_6860&mi_00#... is USB\VID_04E8&PID_6860&MI_00.
fn hardware_id(path: &str) -> Option<String> {
    let mut parts = path.trim_start_matches(r"\\?\").split('#');
    Some(format!(r"{}\{}", parts.next()?, parts.next()?).to_uppercase())
}

/// The device instance in a device path: USB\VID_04E8&PID_6860&MI_00\6&315&0&0000.
fn instance(path: &str) -> Option<String> {
    let mut parts = path.trim_start_matches(r"\\?\").split('#');
    Some(format!(r"{}\{}\{}", parts.next()?, parts.next()?, parts.next()?).to_uppercase())
}

/// The INF of a tablet's files function, for these ids.
fn tablet_stem(hardware: &str) -> String {
    let word: String = hardware.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    format!("{TABLET_INF}-{word}")
}

fn is_tablet(path: &String) -> bool {
    ids(path).is_some_and(|(v, d)| ANDROID_VENDORS.contains(&v) && !is_accessory(v, d))
}

fn accessories() -> Vec<String> {
    devices(&ACCESSORY).into_iter().filter(|p| ids(p).is_some_and(|(v, d)| is_accessory(v, d))).collect()
}

/// As the service, for an app about to use the cable: a tablet with USB debugging off has its
/// files' function given to WinUSB, until it is an accessory or the time is up.
fn prepare() -> Result<()> {
    if !accessories().is_empty() || devices(&ADB).iter().any(is_tablet) { return Ok(()); }
    let tablet = devices(&PORTABLE).into_iter().find(is_tablet).ok_or("no tablet on the cable")?;
    let hardware = hardware_id(&tablet).ok_or("an odd device path")?;
    let stem = tablet_stem(&hardware);
    sign_and_add(&stem, &[hardware])?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !devices(&ACCESSORY).iter().any(is_tablet) {
        if Instant::now() > deadline { let _ = remove(&stem); return Err("WinUSB did not take the tablet".into()); }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        while accessories().is_empty() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(200)); }
        let _ = remove(&stem);
    });
    Ok(())
}

/// Pulls the cable out and back in, as far as the tablet can tell: Android leaves accessory mode.
fn cycle() -> Result<()> {
    let path = accessories().into_iter().next().ok_or("no accessory")?;
    let mut id = instance(&path).ok_or("an odd device path")?;
    unsafe {
        let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain([0]).collect() };
        let located = wide(&id);
        let mut node = 0;
        if CM_Locate_DevNodeW(&mut node, PCWSTR(located.as_ptr()), CM_LOCATE_DEVNODE_NORMAL) != CR_SUCCESS { return Err("no such device".into()); }
        if id.contains("&MI_") { // a function of the tablet: the tablet is its parent
            let function = node;
            if CM_Get_Parent(&mut node, function, 0) != CR_SUCCESS { return Err("no parent".into()); }
        }
        let (mut port, mut size) = (0u32, 4u32);
        if CM_Get_DevNode_Registry_PropertyW(node, CM_DRP_ADDRESS, None, Some(&mut port as *mut u32 as _), &mut size, 0) != CR_SUCCESS { return Err("no port".into()); }
        let device = node;
        if CM_Get_Parent(&mut node, device, 0) != CR_SUCCESS { return Err("no hub".into()); }
        let mut name = [0u16; 256];
        if CM_Get_Device_IDW(node, &mut name, 0) != CR_SUCCESS { return Err("no hub's name".into()); }
        id = String::from_utf16_lossy(&name).trim_end_matches('\0').to_string();
        let hub = CreateFileW(&HSTRING::from(format!(r"\\?\{}#{HUB}", id.replace('\\', "#"))), (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, None)?;
        let (mut params, mut returned) = ([port, 0], 0);
        let done = DeviceIoControl(hub, IOCTL_USB_HUB_CYCLE_PORT, Some(params.as_ptr() as _), 8, Some(params.as_mut_ptr() as _), 8, Some(&mut returned), None);
        let _ = CloseHandle(hub);
        done?;
    }
    Ok(())
}

/// The service's pipe for the cable, on a thread of its own.
pub fn serve() {
    // Nobody reads a service's stderr: what went wrong goes where an install's would.
    let note = |result: Result<()>| { let _ = display::logged(result); };
    display::serve(PIPE, || { let ready = prepare(); let ok = ready.is_ok(); note(ready); ok.then_some(()) }, |()| {
        // The tablet stays an accessory if its app still holds the link when the port cycles.
        for _ in 0..4 {
            std::thread::sleep(Duration::from_secs(1));
            if accessories().is_empty() { break; } // pulled out, or gone back
            note(cycle());
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

fn folder(stem: &str) -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramData").unwrap_or_default()).join("Screenlet").join(stem)
}

/// Every copy of an INF away, each device it held going back to its best other driver.
/// Its signed files stay: the next time needs no new certificate.
fn remove(stem: &str) -> Result<()> {
    for _ in 0..16 {
        let listed = hidden("pnputil").arg("/enum-drivers").output()?;
        let Some(name) = published(&String::from_utf8_lossy(&listed.stdout).to_lowercase(), &format!("{stem}.inf")) else { break };
        hidden("pnputil").args(["/delete-driver", &name, "/uninstall", "/force"]).status()?;
    }
    Ok(())
}

/// As administrator (--uninstall-driver): those INFs, and the certificates that signed them.
pub fn uninstall() -> Result<()> {
    remove(ACCESSORY_INF)?;
    let root = folder(ACCESSORY_INF);
    let parent = root.parent().ok_or("no folder")?;
    for entry in std::fs::read_dir(parent)?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(TABLET_INF) { remove(&name)?; let _ = std::fs::remove_dir_all(entry.path()); }
    }
    let _ = std::fs::remove_dir_all(root);
    let script = format!(r"foreach ($s in 'Root','TrustedPublisher') {{ Get-ChildItem Cert:\LocalMachine\$s | Where-Object Subject -eq '{SIGNER}' | Remove-Item }}");
    hidden("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).status()?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Plugged {
    None,
    /// Nothing here can reach it: USB debugging is off, and no service to help.
    Closed,
    Ready,
}

/// What is on the cable: looked at every second. A tablet with USB debugging off shows only
/// as a portable device (MTP), which only the service can open.
pub fn plugged() -> Plugged {
    let tablet = |p: &String| ids(p).is_some_and(|(v, _)| ANDROID_VENDORS.contains(&v));
    if !devices(&ACCESSORY).is_empty() || devices(&ADB).iter().any(tablet) { return Plugged::Ready; }
    if !devices(&PORTABLE).iter().any(tablet) { return Plugged::None; }
    if display::available() { Plugged::Ready } else { Plugged::Closed }
}

/// A device through WinUSB.
struct Usb {
    file: HANDLE,
    usb: WINUSB_INTERFACE_HANDLE,
}

// Handles of Windows, for use from any thread: reads and writes go on at once on WinUSB.
unsafe impl Send for Usb {}
unsafe impl Sync for Usb {}

impl Usb {
    fn open(path: &str) -> Result<Usb> {
        unsafe {
            let file = CreateFileW(&HSTRING::from(path), (GENERIC_READ | GENERIC_WRITE).0, FILE_SHARE_READ | FILE_SHARE_WRITE, None,
                OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED, None)?;
            let mut usb = WINUSB_INTERFACE_HANDLE::default();
            if let Err(e) = WinUsb_Initialize(file, &mut usb) {
                let _ = CloseHandle(file);
                return Err(e.into());
            }
            Ok(Usb { file, usb })
        }
    }

    fn control(&self, kind: u8, request: u8, index: u16, data: &mut [u8]) -> Result<usize> {
        let setup = WINUSB_SETUP_PACKET { RequestType: kind, Request: request, Value: 0, Index: index, Length: data.len() as u16 };
        let mut done = 0;
        unsafe { WinUsb_ControlTransfer(self.usb, setup, Some(data), Some(&mut done), None)?; }
        Ok(done as usize)
    }

    /// The bulk endpoints of interface 0: in, out, and out's packet size.
    fn endpoints(&self) -> Result<(u8, u8)> {
        let (mut input, mut out) = (0, 0);
        unsafe {
            let mut interface = USB_INTERFACE_DESCRIPTOR::default();
            WinUsb_QueryInterfaceSettings(self.usb, 0, &mut interface)?;
            for i in 0..interface.bNumEndpoints {
                let mut pipe = WINUSB_PIPE_INFORMATION::default();
                WinUsb_QueryPipe(self.usb, 0, i, &mut pipe)?;
                if pipe.PipeType == UsbdPipeTypeBulk {
                    if pipe.PipeId & 0x80 != 0 { input = pipe.PipeId } else { out = pipe.PipeId }
                }
            }
        }
        if input == 0 || out == 0 { return Err("the accessory has no bulk endpoints".into()); }
        Ok((input, out))
    }

    fn policy(&self, pipe: u8, which: WINUSB_PIPE_POLICY, value: &[u8]) -> Result<()> {
        unsafe { WinUsb_SetPipePolicy(self.usb, pipe, which, value.len() as u32, value.as_ptr() as _)?; }
        Ok(())
    }

    fn timeout(&self, pipe: u8, timeout: Duration) -> Result<()> {
        self.policy(pipe, PIPE_TRANSFER_TIMEOUT, &(timeout.as_millis() as u32).to_le_bytes())
    }

    /// Bytes read; 0 when none came in the pipe's timeout.
    fn read(&self, pipe: u8, into: &mut [u8]) -> windows::core::Result<usize> {
        let mut done = 0;
        match unsafe { WinUsb_ReadPipe(self.usb, pipe, Some(into), Some(&mut done), None) } {
            Err(e) if e.code() == ERROR_SEM_TIMEOUT.to_hresult() => Ok(0),
            Err(e) => Err(e),
            Ok(()) => Ok(done as usize),
        }
    }

    fn write(&self, pipe: u8, data: &[u8]) -> windows::core::Result<usize> {
        let mut done = 0;
        unsafe { WinUsb_WritePipe(self.usb, pipe, data, Some(&mut done), None)?; }
        Ok(done as usize)
    }
}

impl Drop for Usb {
    fn drop(&mut self) {
        unsafe {
            let _ = WinUsb_Free(self.usb);
            let _ = CloseHandle(self.file);
        }
    }
}

/// Asks the tablet behind path to be Screenlet's accessory: it comes back as another device.
fn switch(path: &str) -> Result<()> {
    let usb = Usb::open(path).map_err(|e| format!("the tablet's USB: {e} (WinUSB takes one program at a time: is adb running? adb kill-server)"))?;
    let mut version = [0u8; 2];
    usb.control(0xC0, 51, 0, &mut version)?;
    if u16::from_le_bytes(version) == 0 {
        return Err("the tablet cannot be an accessory (Android Open Accessory)".into());
    }
    for (i, text) in IDENTITY.iter().enumerate() {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        usb.control(0x40, 52, i as u16, &mut bytes)?;
    }
    usb.control(0x40, 53, 0, &mut [])?;
    eprintln!("usb: asked the tablet to be Screenlet's accessory");
    Ok(())
}

pub struct Cable {
    held: Mutex<Option<std::fs::File>>, // to the service, which cycles the port when this closes
    usb: Usb,
    out: u8,
    writing: Mutex<()>,
    streams: Mutex<HashMap<u8, Sender<Vec<u8>>>>,
    next: Mutex<u8>,
    running: AtomicBool,
    /// The display's datagrams from the tablet, as they arrive (the reading thread).
    pub on_datagram: Mutex<Option<Box<dyn FnMut(&[u8]) + Send>>>,
}

impl Cable {
    /// Switches the tablet to accessory mode, opens it and waits for the app's
    /// HELLO (the first time, Android asks its user). Blocks.
    pub fn open() -> Result<Arc<Cable>> {
        let accessory = || accessories().into_iter().next();
        // Blocks while the service gets the tablet ready, the first time by signing for it.
        let mut held = display::connect(PIPE).ok();
        if let Some(pipe) = held.as_mut() {
            let mut ready = [0u8; 1];
            if pipe.read_exact(&mut ready).is_err() || ready[0] != 1 {
                return Err("the tablet on the cable is not ready: unlock it, and try again".into());
            }
        }
        if accessory().is_none() {
            let reachable = devices(&ADB).into_iter().chain(devices(&ACCESSORY));
            let tablet = reachable.into_iter().find(is_tablet);
            switch(&tablet.ok_or("no tablet on the cable, or nothing here can reach it")?)?;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let path = loop {
            if let Some(path) = accessory() {
                break path;
            }
            if Instant::now() > deadline {
                return Err("the tablet did not come back as an accessory (is what the cable needs installed?)".into());
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        let usb = Usb::open(&path).map_err(|e| format!("the accessory: {e}"))?;
        let (input, out) = usb.endpoints()?;
        // A transfer ends on a short packet: one that ends on a full packet needs an empty one.
        usb.policy(out, SHORT_PACKET_TERMINATE, &[1])?;
        unsafe {
            // Still an accessory from an earlier session, maybe mid-frame.
            let _ = WinUsb_ResetPipe(usb.usb, input);
            let _ = WinUsb_ResetPipe(usb.usb, out);
        }
        usb.timeout(input, Duration::from_secs(1))?;
        usb.timeout(out, Duration::from_secs(1))?;
        let cable = Arc::new(Cable {
            held: Mutex::new(held), usb, out, writing: Mutex::new(()), streams: Mutex::new(HashMap::new()), next: Mutex::new(0),
            running: AtomicBool::new(true), on_datagram: Mutex::new(None),
        });
        // HELLO until the app reads it (Android asks its user the first time), then its answer.
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut hello = Vec::new();
        frame(HELLO, 0, &[], &mut hello);
        while cable.write_all(&hello).is_err() {
            if Instant::now() > deadline {
                return Err("Screenlet did not open on the tablet (allow it there)".into());
            }
        }
        let mut buffer = Vec::new();
        let mut chunk = vec![0u8; READ];
        'answer: loop {
            if Instant::now() > deadline {
                return Err("Screenlet's app did not answer on the cable".into());
            }
            let n = cable.usb.read(input, &mut chunk)?;
            buffer.extend_from_slice(&chunk[..n]);
            for (kind, _, _) in frames(&mut buffer)? {
                if kind == HELLO {
                    break 'answer;
                }
            }
        }
        eprintln!("usb: Screenlet's app answers on the cable");
        cable.usb.timeout(input, Duration::from_millis(500))?;
        cable.usb.timeout(cable.out, Duration::from_secs(5))?;
        let reader = cable.clone();
        std::thread::Builder::new().name("screenlet-usb-in".into()).spawn(move || reader.read_loop(input, buffer))?;
        Ok(cable)
    }

    fn write_all(&self, data: &[u8]) -> windows::core::Result<()> {
        let _held = self.writing.lock().unwrap();
        let mut at = 0;
        while at < data.len() {
            at += self.usb.write(self.out, &data[at..])?;
        }
        Ok(())
    }

    pub fn write(&self, data: &[u8]) -> Result<()> {
        self.write_all(data).inspect_err(|_| self.close())?;
        Ok(())
    }

    fn read_loop(self: Arc<Self>, input: u8, mut buffer: Vec<u8>) {
        let mut chunk = vec![0u8; READ];
        while self.running.load(Ordering::Relaxed) {
            match self.usb.read(input, &mut chunk) {
                Ok(0) => continue,
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                Err(e) => {
                    eprintln!("usb: the cable's link is lost: {e}");
                    break;
                }
            }
            let Ok(found) = frames(&mut buffer) else { break };
            for (kind, connection, data) in found {
                match kind {
                    DATAGRAM => {
                        if let Some(callback) = self.on_datagram.lock().unwrap().as_mut() {
                            callback(&data);
                        }
                    }
                    DATA => {
                        if let Some(stream) = self.streams.lock().unwrap().get(&connection) {
                            let _ = stream.send(data);
                        }
                    }
                    CLOSE => {
                        self.streams.lock().unwrap().remove(&connection);
                    }
                    _ => {}
                }
            }
        }
        self.close();
    }

    /// A new stream to the app, for a TLS link.
    pub fn connect(self: &Arc<Self>) -> Result<CableStream> {
        let id = {
            let mut next = self.next.lock().unwrap();
            *next = *next % 255 + 1;
            *next
        };
        let (tx, rx) = channel();
        self.streams.lock().unwrap().insert(id, tx);
        let mut open = Vec::new();
        frame(OPEN, id, &[], &mut open);
        self.write(&open)?;
        Ok(CableStream { cable: self.clone(), id, rx, pending: Vec::new(), at: 0 })
    }

    /// One video frame's datagrams in as few transfers as the app takes.
    pub fn send_datagrams<'a>(&self, datagrams: impl Iterator<Item = &'a [u8]>, batch: &mut Vec<u8>) -> Result<()> {
        batch.clear();
        for datagram in datagrams {
            if !batch.is_empty() && batch.len() + HEADER + datagram.len() > BATCH {
                self.write(batch)?;
                batch.clear();
            }
            frame(DATAGRAM, 0, datagram, batch);
        }
        if !batch.is_empty() {
            self.write(batch)?;
        }
        Ok(())
    }

    pub fn alive(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn close(&self) {
        if self.running.swap(false, Ordering::Relaxed) {
            self.streams.lock().unwrap().clear();
            *self.held.lock().unwrap() = None; // the tablet goes back to what it was
        }
    }
}

/// A TLS connection's bytes over the cable. Reads time out (WouldBlock) after
/// a second, so the link can ping meanwhile.
pub struct CableStream {
    cable: Arc<Cable>,
    id: u8,
    rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
    at: usize,
}

impl Read for CableStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.at == self.pending.len() {
            match self.rx.recv_timeout(Duration::from_secs(1)) {
                Ok(data) => {
                    self.pending = data;
                    self.at = 0;
                }
                Err(RecvTimeoutError::Timeout) => return Err(io::ErrorKind::WouldBlock.into()),
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len() - self.at);
        buf[..n].copy_from_slice(&self.pending[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

impl Write for CableStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut data = Vec::with_capacity(HEADER + buf.len());
        let n = buf.len().min(MAX_FRAME);
        frame(DATA, self.id, &buf[..n], &mut data);
        self.cable.write(&data).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for CableStream {
    fn drop(&mut self) {
        if self.cable.streams.lock().unwrap().remove(&self.id).is_some() {
            let mut close = Vec::new();
            frame(CLOSE, self.id, &[], &mut close);
            let _ = self.cable.write(&close);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_matches_the_vector() {
        let v: serde_json::Value = serde_json::from_str(include_str!("../vectors.json")).unwrap();
        let mut out = Vec::new();
        frame(DATAGRAM, 0, &crate::protocol::unhex(v["cable_frame"]["data"].as_str().unwrap()).unwrap(), &mut out);
        assert_eq!(crate::protocol::hex(&out), v["cable_frame"]["frame"].as_str().unwrap());
    }

    #[test]
    fn frames_across_reads() {
        let mut stream = Vec::new();
        frame(DATA, 3, &[1, 2, 3], &mut stream);
        frame(DATAGRAM, 0, &[9; 40], &mut stream);
        let mut buffer = stream[..5].to_vec();
        assert!(frames(&mut buffer).unwrap().is_empty());
        buffer.extend_from_slice(&stream[5..15]);
        assert_eq!(frames(&mut buffer).unwrap(), vec![(DATA, 3, vec![1, 2, 3])]);
        buffer.extend_from_slice(&stream[15..]);
        let second = frames(&mut buffer).unwrap();
        assert_eq!((second[0].0, second[0].2.len()), (DATAGRAM, 40));
        assert!(buffer.is_empty());
        let mut huge = vec![DATA, 1, 0x7f, 0xff, 0xff, 0xff];
        assert!(frames(&mut huge).is_err());
    }

    #[test]
    fn the_inf_gives_its_devices_to_winusb() {
        let text = inf(&[r"USB\VID_18D1&PID_2D00".to_string()], "x.cat");
        assert!(text.contains(r"%Name%=Device, USB\VID_18D1&PID_2D00"));
        assert!(text.contains("CatalogFile=x.cat\r\n") && text.contains("Needs=WINUSB.NT\r\n") && text.contains("Signature=\"$Windows NT$\""));
        assert_eq!(hardware_id(r"\\?\usb#vid_04e8&pid_6860&mi_00#6&315#{6ac27878}").as_deref(), Some(r"USB\VID_04E8&PID_6860&MI_00"));
        assert_eq!(instance(r"\\?\usb#vid_18d1&pid_2d01&mi_00#6&195d4289&0&0000#{7c1d}").as_deref(), Some(r"USB\VID_18D1&PID_2D01&MI_00\6&195D4289&0&0000"));
        assert_eq!(tablet_stem(r"USB\VID_04E8&PID_6860&MI_00"), "screenlet-tablet-usb-vid-04e8-pid-6860-mi-00");
    }

    #[test]
    fn device_paths_say_their_ids() {
        assert_eq!(ids(r"\\?\usb#vid_04e8&pid_6860&mi_03#6&315cdb0b&0&0003#{f72fe0d4-cbcb-407d-8814-9ed673d0dd6b}"), Some((0x04E8, 0x6860)));
        assert_eq!(ids(r"\\?\USB#VID_18D1&PID_2D01&MI_00#7&1#{x}"), Some((GOOGLE, 0x2D01)));
        assert_eq!(ids(r"\\?\swd#wpdbusenum#x"), None);
    }
}
