//! Tablets on the network: the app announces _screenlet._tcp with its
//! certificate's id and its name (PROTOCOL.md §1).

use crate::protocol::SERVICE;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct Tablet {
    pub id: String,
    pub name: String,
    pub address: SocketAddr,
}

/// The tablets found so far, by their service name; kept up to date on a thread of mdns-sd's.
pub fn browse() -> Arc<Mutex<HashMap<String, Tablet>>> {
    let found: Arc<Mutex<HashMap<String, Tablet>>> = Arc::default();
    let Ok(daemon) = ServiceDaemon::new() else { return found };
    let Ok(events) = daemon.browse(SERVICE) else { return found };
    let into = found.clone();
    std::thread::spawn(move || {
        let _daemon = daemon; // browsing for as long as this thread lives
        while let Ok(event) = events.recv() {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    // IPv4: the display's packets go to the address the link reached.
                    let Some(ip) = info.get_addresses_v4().into_iter().next() else { continue };
                    let name = info.get_property_val_str("name").filter(|n| !n.is_empty())
                        .unwrap_or_else(|| info.get_fullname().split('.').next().unwrap_or("tablet")).chars().take(64).collect();
                    let id = info.get_property_val_str("id").unwrap_or_default().to_lowercase();
                    into.lock().unwrap().insert(info.get_fullname().to_string(),
                        Tablet { id, name, address: SocketAddr::new(ip.into(), info.get_port()) });
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    into.lock().unwrap().remove(&fullname);
                }
                _ => {}
            }
        }
    });
    found
}

/// A tablet's own Wi-Fi Direct group, joined as an ordinary Wi-Fi network (PROTOCOL.md §0):
/// a profile for it, read by netsh from a file that lasts only that long.
pub fn join(ssid: &str, passphrase: &str) -> crate::link::Result<()> {
    let safe = |text: &str| text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let (name, key) = (safe(ssid), safe(passphrase));
    let profile = format!(r#"<?xml version="1.0"?><WLANProfile xmlns="http://www.microsoft.com/networking/WLAN/profile/v1"><name>{name}</name><SSIDConfig><SSID><name>{name}</name></SSID></SSIDConfig><connectionType>ESS</connectionType><connectionMode>manual</connectionMode><MSM><security><authEncryption><authentication>WPA2PSK</authentication><encryption>AES</encryption><useOneX>false</useOneX></authEncryption><sharedKey><keyType>passPhrase</keyType><protected>false</protected><keyMaterial>{key}</keyMaterial></sharedKey></security></MSM></WLANProfile>"#);
    let file = crate::link::config().join("direct.xml");
    std::fs::create_dir_all(crate::link::config())?;
    std::fs::write(&file, profile)?;
    let run = |args: &[&str]| -> crate::link::Result<bool> {
        Ok(std::process::Command::new("netsh").args(args).creation_flags(0x0800_0000).output()?.status.success())
    };
    let added = run(&["wlan", "add", "profile", &format!("filename={}", file.display()), "user=current"]);
    let _ = std::fs::remove_file(&file);
    if !added? || !run(&["wlan", "connect", &format!("name={ssid}"), &format!("ssid={ssid}")])? {
        return Err(format!("could not join {ssid}: is Screenlet open on the tablet, with Wi-Fi on?").into());
    }
    Ok(())
}

/// The address of a tablet's Wi-Fi Direct group, joined and answering.
pub fn reach(ssid: &str, passphrase: &str, at: &str) -> crate::link::Result<SocketAddr> {
    join(ssid, passphrase)?;
    let address: SocketAddr = format!("{at}:{}", crate::protocol::PORT).parse()?;
    // The network comes up some seconds after joining it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(1)).is_err() {
        if std::time::Instant::now() > deadline { return Err("the tablet's Wi-Fi Direct network did not answer".into()); }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Ok(address)
}
