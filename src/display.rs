//! Extend: a monitor of the tablet's size, for as long as the session lasts. Windows
//! makes monitors only through a display driver: Virtual Display Driver (MIT, signed,
//! driver/), installed by the installer. It reads its modes from a file; the software
//! device made here is what brings it up, and goes with this process, however it ends.
use crate::link::Result;
use std::os::windows::process::CommandExt;
use std::time::{Duration, Instant};
use windows::core::{w, HRESULT, PCWSTR};
use windows::Win32::Devices::Display::*;
use windows::Win32::Devices::Enumeration::Pnp::*;
use windows::Win32::Graphics::Dxgi::*;

const SETTINGS: &str = r"C:\VirtualDisplayDriver\vdd_settings.xml";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const ARRIVAL: Duration = Duration::from_secs(10);

/// As administrator, on the desktop (--install-driver): the driver next to this program,
/// and its modes writable by the app, which sets the tablet's.
pub fn install() -> Result<()> {
    let driver = std::env::current_exe()?.with_file_name("driver");
    let folder = std::path::Path::new(SETTINGS).parent().ok_or("no folder")?;
    std::fs::create_dir_all(folder)?;
    std::fs::copy(driver.join("vdd_settings.xml"), SETTINGS)?;
    let done = std::process::Command::new("icacls").arg(folder).args(["/grant", "*S-1-5-32-545:(OI)(CI)M"])
        .creation_flags(CREATE_NO_WINDOW).status()?;
    if !done.success() { return Err(format!("icacls ended with {done}").into()); }
    // On the desktop Windows asks the user. Its publisher is not trusted beforehand, on
    // purpose: the certificate is shared by many open source projects.
    let done = std::process::Command::new("pnputil").arg("/add-driver").arg(driver.join("MttVDD.inf")).arg("/install")
        .creation_flags(CREATE_NO_WINDOW).status()?;
    // 3010 asks for a restart, which a software device does not need.
    if !done.success() && done.code() != Some(3010) { return Err(format!("pnputil ended with {done}").into()); }
    Ok(())
}

pub fn available() -> bool {
    std::path::Path::new(SETTINGS).exists()
}

/// The driver's settings with this one mode, and the pointer left in the frames.
pub fn settings(xml: &str, size: (i32, i32), fps: u32) -> Option<String> {
    let start = xml.find("<resolutions>")? + "<resolutions>".len();
    let end = xml.find("</resolutions>")?;
    let mode = format!("<resolution><width>{}</width><height>{}</height><refresh_rate>{fps}</refresh_rate></resolution>", size.0, size.1);
    Some(format!("{}{mode}{}", &xml[..start], &xml[end..]).replace("<HardwareCursor>true<", "<HardwareCursor>false<"))
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

pub struct Monitor {
    device: HSWDEVICE,
    pub name: String,
}

// A handle of Windows: closed once, wherever.
unsafe impl Send for Monitor {}

unsafe extern "system" fn created(_: HSWDEVICE, _: HRESULT, _: *const core::ffi::c_void, _: PCWSTR) {}

impl Monitor {
    pub fn create(size: (i32, i32), fps: u32) -> Result<Monitor> {
        let xml = std::fs::read_to_string(SETTINGS)?;
        std::fs::write(SETTINGS, settings(&xml, size, fps).ok_or("the driver's settings are not as expected")?)?;
        let before = outputs();
        let ids: Vec<u16> = "MttVDD\0\0".encode_utf16().collect();
        let info = SW_DEVICE_CREATE_INFO { cbSize: size_of::<SW_DEVICE_CREATE_INFO>() as u32, pszInstanceId: w!("Screenlet"),
            pszzHardwareIds: PCWSTR(ids.as_ptr()), CapabilityFlags: (SWDeviceCapabilitiesSilentInstall.0 | SWDeviceCapabilitiesDriverRequired.0) as u32,
            pszDeviceDescription: w!("Screenlet display"), ..Default::default() };
        let device = unsafe { SwDeviceCreate(w!("ScreenletDisplay"), w!("HTREE\\ROOT\\0"), &info, None, Some(created), None)? };
        let monitor = Monitor { device, name: String::new() };
        let deadline = Instant::now() + ARRIVAL;
        while Instant::now() < deadline {
            // A new monitor may come up as a copy of the others.
            unsafe { SetDisplayConfig(None, None, SDC_TOPOLOGY_EXTEND | SDC_APPLY); }
            if let Some(name) = outputs().into_iter().find(|n| !before.contains(n)) {
                return Ok(Monitor { name, ..monitor });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err("the tablet's monitor did not appear".into())
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        unsafe { SwDeviceClose(self.device) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_have_the_one_mode_and_keep_the_rest() {
        let xml = "<a/><resolutions><resolution>old</resolution></resolutions><HardwareCursor>true</HardwareCursor>";
        let out = settings(xml, (2000, 1200), 60).unwrap();
        assert_eq!(out, "<a/><resolutions><resolution><width>2000</width><height>1200</height><refresh_rate>60</refresh_rate></resolution></resolutions><HardwareCursor>false</HardwareCursor>");
        assert!(settings("<a/>", (1, 1), 1).is_none());
    }
}
