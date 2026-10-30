//! Extend: a monitor of the tablet's size, for as long as the session lasts. Windows
//! makes monitors only through a display driver: Virtual Display Driver (MIT, signed,
//! driver/, inside this program too). It makes a monitor while a device of its exists, and
//! reads the mode from a file. Only an administrator may make that device (SwDeviceCreate is
//! denied otherwise), so a small service holds it for as long as the app holds a pipe to
//! the service open: the monitor goes with the session, however it ends.
use crate::link::Result;
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use windows::core::{w, HRESULT, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Devices::Display::*;
use windows::Win32::Devices::Enumeration::Pnp::*;
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, INVALID_HANDLE_VALUE};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_TYPE_BYTE, PIPE_WAIT};
use windows::Win32::System::Services::*;
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

const SETTINGS: &str = r"C:\VirtualDisplayDriver\vdd_settings.xml";
const SERVICE: &str = "ScreenletDisplay";
const PIPE: &str = r"\\.\pipe\ScreenletDisplay";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const ARRIVAL: Duration = Duration::from_secs(10);
const PACKAGE: [(&str, &[u8]); 4] = [("MttVDD.inf", include_bytes!("../driver/MttVDD.inf")), ("MttVDD.dll", include_bytes!("../driver/MttVDD.dll")),
    ("mttvdd.cat", include_bytes!("../driver/mttvdd.cat")), ("vdd_settings.xml", include_bytes!("../driver/vdd_settings.xml"))];

pub fn hidden(program: &str) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// The driver's files, from inside this program: it installs from anywhere.
fn unpack() -> Result<PathBuf> {
    let folder = std::env::temp_dir().join(format!("screenlet-driver-{}", std::process::id()));
    std::fs::create_dir_all(&folder)?;
    for (name, bytes) in PACKAGE {
        std::fs::write(folder.join(name), bytes)?;
    }
    Ok(folder)
}

/// The software device the driver runs on, with its callback waited for.
unsafe fn device() -> Result<HSWDEVICE> {
    unsafe extern "system" fn created(_: HSWDEVICE, result: HRESULT, context: *const core::ffi::c_void, _: PCWSTR) {
        let _ = unsafe { &*(context as *const mpsc::Sender<HRESULT>) }.send(result);
    }
    let (done, wait) = mpsc::channel();
    let done = Box::into_raw(Box::new(done)); // the callback may come after any wait: not freed, in a short process
    let ids: Vec<u16> = "MttVDD\0\0".encode_utf16().collect();
    let info = SW_DEVICE_CREATE_INFO { cbSize: size_of::<SW_DEVICE_CREATE_INFO>() as u32, pszInstanceId: w!("Screenlet"),
        pszzHardwareIds: PCWSTR(ids.as_ptr()), pszzCompatibleIds: PCWSTR(ids.as_ptr()),
        CapabilityFlags: (SWDeviceCapabilitiesSilentInstall.0 | SWDeviceCapabilitiesDriverRequired.0) as u32,
        pszDeviceDescription: w!("Screenlet display"), ..Default::default() };
    let device = SwDeviceCreate(w!("ScreenletDisplay"), w!("HTREE\\ROOT\\0"), &info, None, Some(created), Some(done as _))?;
    let result: HRESULT = wait.recv_timeout(Duration::from_secs(30)).map_err(|_| "the device did not come up")?;
    result.ok()?;
    Ok(device)
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramFiles").unwrap_or_default()).join("Screenlet")
}

fn log() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramData").unwrap_or_default()).join("Screenlet").join("install.log")
}

/// What an elevated step ended with, for the app that asked and cannot see it.
pub fn logged(result: Result<()>) -> Result<()> {
    let _ = std::fs::create_dir_all(log().parent().unwrap_or(Path::new(".")));
    let _ = std::fs::write(log(), result.as_ref().err().map(|e| e.to_string()).unwrap_or_default());
    result
}

/// As administrator, on the desktop (--install-driver): the driver, and its modes writable
/// by the app, which sets the tablet's.
pub fn install() -> Result<()> {
    let folder = Path::new(SETTINGS).parent().ok_or("no folder")?;
    std::fs::create_dir_all(folder)?;
    let done = hidden("icacls").arg(folder).args(["/grant", "*S-1-5-32-545:(OI)(CI)M"]).status()?;
    if !done.success() { return Err(format!("icacls ended with {done}").into()); }
    let driver = unpack()?;
    let xml = std::fs::read_to_string(driver.join("vdd_settings.xml"))?;
    std::fs::write(SETTINGS, xml)?;
    // On the desktop Windows asks the user. Its publisher is not trusted beforehand, on
    // purpose: the certificate is shared by many open source projects.
    let done = hidden("pnputil").arg("/add-driver").arg(driver.join("MttVDD.inf")).arg("/install").status();
    let _ = std::fs::remove_dir_all(&driver);
    // 3010 asks for a restart, which a software device does not need; 259: nothing to update.
    let done = done?;
    if !done.success() && !matches!(done.code(), Some(3010 | 259)) { return Err(format!("pnputil ended with {done}").into()); }
    // The service runs as the system: from a folder only administrators can change.
    std::fs::create_dir_all(home())?;
    let binary = home().join("screenlet-display.exe");
    remove_service();
    std::fs::copy(std::env::current_exe()?, &binary)?;
    let done = hidden("sc").args(["create", SERVICE, "start=", "auto", "binPath="]).arg(format!("\"{}\" --display-service", binary.display())).status()?;
    if !done.success() { return Err(format!("sc create ended with {done}").into()); }
    let done = hidden("sc").args(["start", SERVICE]).status()?;
    if !done.success() { return Err(format!("sc start ended with {done}").into()); }
    Ok(())
}

fn remove_service() {
    let _ = hidden("sc").args(["stop", SERVICE]).status();
    let _ = hidden("sc").args(["delete", SERVICE]).status();
    std::thread::sleep(Duration::from_secs(1)); // or its program is still in use
}

static mut STATUS: SERVICE_STATUS_HANDLE = SERVICE_STATUS_HANDLE(std::ptr::null_mut());

unsafe fn report(state: SERVICE_STATUS_CURRENT_STATE) {
    let status = SERVICE_STATUS { dwServiceType: SERVICE_WIN32_OWN_PROCESS, dwCurrentState: state, dwControlsAccepted: SERVICE_ACCEPT_STOP, ..Default::default() };
    let _ = SetServiceStatus(STATUS, &status);
}

unsafe extern "system" fn control(what: u32) {
    if what == SERVICE_CONTROL_STOP {
        unsafe { report(SERVICE_STOPPED); }
        std::process::exit(0); // and the device with it, if any
    }
}

/// For each app that connects: the device, and a byte that says how that went, until it hangs up.
unsafe extern "system" fn service_main(_: u32, _: *mut PWSTR) {
    unsafe {
        let Ok(status) = RegisterServiceCtrlHandlerW(&HSTRING::from(SERVICE), Some(control)) else { return };
        STATUS = status;
        report(SERVICE_RUNNING);
        std::thread::spawn(crate::cable::serve);
        serve(PIPE, || device().ok(), |device| SwDeviceClose(device));
        report(SERVICE_STOPPED);
    }
}

/// A pipe of the service. For each app that connects: start, a byte that says how that went,
/// and end when it hangs up, however it does. Returns only if there can be no pipe.
pub fn serve<T>(name: &str, start: impl Fn() -> Option<T>, end: impl Fn(T)) {
    unsafe {
        // The default access: anyone may connect to read, which is all an app has to do.
        let pipe = CreateNamedPipeW(&HSTRING::from(name), PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE | PIPE_WAIT, 1, 16, 16, 0, None);
        if pipe == INVALID_HANDLE_VALUE { return; }
        loop {
            if ConnectNamedPipe(pipe, None).is_ok() {
                let done = start();
                let _ = WriteFile(pipe, Some(&[done.is_some() as u8]), None, None);
                let mut byte = [0u8; 1];
                while ReadFile(pipe, Some(&mut byte), None, None).is_ok() {}
                if let Some(done) = done { end(done); }
            }
            let _ = DisconnectNamedPipe(pipe);
        }
    }
}

/// As the service (--display-service): what the system runs.
pub fn service() -> Result<()> {
    let name: Vec<u16> = SERVICE.encode_utf16().chain([0]).collect();
    let table = [SERVICE_TABLE_ENTRYW { lpServiceName: PWSTR(name.as_ptr() as _), lpServiceProc: Some(service_main) }, SERVICE_TABLE_ENTRYW::default()];
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr())?; }
    Ok(())
}

/// As administrator (--uninstall-driver): the service, the device, the driver and its settings.
pub fn uninstall() -> Result<()> {
    remove_service();
    unsafe {
        if let Ok(device) = device() {
            let _ = SwDeviceSetLifetime(device, SWDeviceLifetimeHandle);
            SwDeviceClose(device); // with the handle's lifetime, this removes it
        }
    }
    let listed = hidden("pnputil").arg("/enum-drivers").output()?;
    let text = String::from_utf8_lossy(&listed.stdout).to_lowercase();
    if let Some(name) = published(&text, "mttvdd.inf") {
        hidden("pnputil").args(["/delete-driver", &name, "/uninstall", "/force"]).status()?;
    }
    let _ = std::fs::remove_dir_all(Path::new(SETTINGS).parent().ok_or("no folder")?);
    let _ = std::fs::remove_file(home().join("screenlet-display.exe"));
    Ok(())
}

/// The oemNN.inf of this inf in pnputil's list, whatever the language of its labels.
pub fn published(text: &str, inf: &str) -> Option<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let at = words.iter().position(|w| *w == inf)?;
    words[..at].iter().rev().find(|w| w.starts_with("oem") && w.ends_with(".inf")).map(|w| w.to_string())
}

/// This program again as administrator: Windows asks the user, then this returns.
/// Refused, or failed: why.
pub fn elevated(parameters: &str) -> Result<()> {
    let parameters = HSTRING::from(parameters);
    let exe = HSTRING::from(std::env::current_exe()?.as_os_str());
    let mut run = SHELLEXECUTEINFOW { cbSize: size_of::<SHELLEXECUTEINFOW>() as u32, fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: w!("runas"), lpFile: PCWSTR(exe.as_ptr()), lpParameters: PCWSTR(parameters.as_ptr()), nShow: SW_HIDE.0, ..Default::default() };
    unsafe {
        ShellExecuteExW(&mut run)?;
        WaitForSingleObject(run.hProcess, INFINITE);
        let mut code = 1;
        let looked = GetExitCodeProcess(run.hProcess, &mut code);
        let _ = CloseHandle(run.hProcess);
        looked?;
        if code != 0 {
            return Err(std::fs::read_to_string(log()).ok().filter(|e| !e.is_empty()).unwrap_or_else(|| format!("it ended with {code}")).into());
        }
    }
    Ok(())
}

/// Whether what makes a monitor is installed. Not whether its pipe answers: a session holds it.
pub fn available() -> bool {
    Path::new(SETTINGS).exists() && home().join("screenlet-display.exe").exists()
}

/// The service's pipe: one client at a time, busy for a moment after the last one left.
pub fn connect(name: &str) -> Result<std::fs::File> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match std::fs::File::open(name) {
            Ok(pipe) => return Ok(pipe),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("the display service: {e}").into()),
        }
    }
}

/// What is between open and close in xml, replaced.
fn between(xml: &str, open: &str, close: &str, with: &str) -> Option<String> {
    let start = xml.find(open)? + open.len();
    let end = xml[start..].find(close)? + start;
    Some(format!("{}{with}{}", &xml[..start], &xml[end..]))
}

/// The driver's settings with this one monitor at this one mode, and the pointer left
/// in the frames.
pub fn settings(xml: &str, size: (i32, i32), fps: u32) -> Option<String> {
    let mode = format!("<resolution><width>{}</width><height>{}</height><refresh_rate>{fps}</refresh_rate></resolution>", size.0, size.1);
    let xml = between(xml, "<resolutions>", "</resolutions>", &mode)?;
    let xml = between(&xml, "<global>", "</global>", &format!("<g_refresh_rate>{fps}</g_refresh_rate>"))?;
    Some(between(&xml, "<count>", "</count>", "1")?.replace("<HardwareCursor>true<", "<HardwareCursor>false<"))
}

/// Every monitor's device name (\\.\DISPLAY3), through all the adapters.
pub fn outputs() -> Vec<String> {
    let mut names = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return names };
    let mut a = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(a) } {
        let mut o = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(o) } {
            if let Ok(description) = unsafe { output.GetDesc() } {
                names.push(String::from_utf16_lossy(&description.DeviceName).trim_end_matches('\0').to_string());
            }
            o += 1;
        }
        a += 1;
    }
    names
}

/// Each monitor's device name and what its maker calls it, by asking Windows' display paths.
fn makers() -> HashMap<String, String> {
    let mut found = HashMap::new();
    unsafe {
        let (mut paths, mut modes) = (0, 0);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths, &mut modes) != ERROR_SUCCESS { return found; }
        let mut path_list = vec![DISPLAYCONFIG_PATH_INFO::default(); paths as usize];
        let mut mode_list = vec![DISPLAYCONFIG_MODE_INFO::default(); modes as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut paths, path_list.as_mut_ptr(), &mut modes, mode_list.as_mut_ptr(), None) != ERROR_SUCCESS { return found; }
        for path in &path_list[..paths as usize] {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            source.header = DISPLAYCONFIG_DEVICE_INFO_HEADER { r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, size: size_of_val(&source) as u32,
                adapterId: path.sourceInfo.adapterId, id: path.sourceInfo.id };
            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            target.header = DISPLAYCONFIG_DEVICE_INFO_HEADER { r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, size: size_of_val(&target) as u32,
                adapterId: path.targetInfo.adapterId, id: path.targetInfo.id };
            if DisplayConfigGetDeviceInfo(&mut source.header) == 0 && DisplayConfigGetDeviceInfo(&mut target.header) == 0 {
                let text = |units: &[u16]| String::from_utf16_lossy(units).trim_end_matches('\0').to_string();
                found.insert(text(&source.viewGdiDeviceName), text(&target.monitorFriendlyDeviceName));
            }
        }
    }
    found
}

/// The monitors that can be mirrored: device name, and a name for the window.
pub fn monitors() -> Vec<(String, String)> {
    let makers = makers();
    outputs().into_iter().filter(|name| makers.get(name).is_none_or(|m| !m.contains("VDD"))).enumerate().map(|(i, name)| {
        let number = i + 1;
        let shown = match makers.get(&name).filter(|m| !m.is_empty()) {
            Some(maker) => format!("{number}: {maker}"),
            None => format!("Monitor {number}"),
        };
        (name, shown)
    }).collect()
}

pub struct Monitor {
    pub name: String,
    _held: std::fs::File, // the service keeps the device while this is open
}

impl Monitor {
    pub fn create(size: (i32, i32), fps: u32) -> Result<Monitor> {
        let xml = std::fs::read_to_string(SETTINGS)?;
        let wanted = settings(&xml, size, fps).ok_or("the driver's settings are not as expected")?;
        std::fs::write(SETTINGS, wanted)?;
        let before = outputs();
        let mut held = connect(PIPE)?;
        let mut made = [0u8; 1];
        held.read_exact(&mut made)?;
        if made[0] != 1 { return Err("the display service could not make the device".into()); }
        let deadline = Instant::now() + ARRIVAL;
        while Instant::now() < deadline {
            // A new monitor may come up as a copy of the others.
            unsafe { SetDisplayConfig(None, None, SDC_TOPOLOGY_EXTEND | SDC_APPLY); }
            if let Some(name) = outputs().into_iter().find(|n| !before.contains(n)) {
                return Ok(Monitor { name, _held: held });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err("the tablet's monitor did not appear".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_have_the_one_mode_and_keep_the_rest() {
        let xml = "<monitors><count>0</count></monitors><global><g_refresh_rate>60</g_refresh_rate><g_refresh_rate>90</g_refresh_rate></global><a/><resolutions><resolution>old</resolution></resolutions><HardwareCursor>true</HardwareCursor>";
        let out = settings(xml, (2000, 1200), 100).unwrap();
        assert_eq!(out, "<monitors><count>1</count></monitors><global><g_refresh_rate>100</g_refresh_rate></global><a/><resolutions><resolution><width>2000</width><height>1200</height><refresh_rate>100</refresh_rate></resolution></resolutions><HardwareCursor>false</HardwareCursor>");
        assert!(settings("<a/>", (1, 1), 1).is_none());
    }

    #[test]
    fn the_shipped_settings_are_as_expected() {
        assert!(std::str::from_utf8(PACKAGE[3].1).is_ok_and(|xml| settings(xml, (1, 1), 1).is_some()));
    }

    #[test]
    fn the_published_name_comes_from_any_language() {
        let list = "Nome pubblicato:     oem7.inf\nNome originale:      other.inf\n\nNome pubblicato:     oem12.inf\nNome originale:      mttvdd.inf\n";
        assert_eq!(published(&list.to_lowercase(), "mttvdd.inf").as_deref(), Some("oem12.inf"));
        assert_eq!(published("oem1.inf other.inf", "mttvdd.inf"), None);
    }
}
