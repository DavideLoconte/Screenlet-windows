//! Tablets on the network: the app announces _screenlet._tcp with its
//! certificate's id and its name (PROTOCOL.md §1).

use crate::protocol::SERVICE;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::collections::HashMap;
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
