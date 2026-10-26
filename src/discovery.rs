use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use gst::prelude::*;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct NdiSource {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DiscoveryStatus {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub sources: Vec<NdiSource>,
}

/// Keeps an NDI device monitor running in the background and caches what it sees.
pub struct Discovery {
    status: Mutex<DiscoveryStatus>,
}

impl Discovery {
    pub fn start() -> Arc<Self> {
        let d = Arc::new(Self { status: Mutex::new(DiscoveryStatus::default()) });
        let worker = d.clone();
        thread::Builder::new().name("ndi-discovery".into()).spawn(move || worker.run()).expect("spawning discovery");
        d
    }

    pub fn status(&self) -> DiscoveryStatus {
        self.status.lock().unwrap().clone()
    }

    fn run(&self) {
        loop {
            let monitor = gst::DeviceMonitor::new();
            monitor.add_filter(Some("Source/Network"), None);
            if monitor.start().is_err() {
                let mut s = self.status.lock().unwrap();
                s.running = false;
                s.sources.clear();
                s.message = Some(
                    "NDI discovery could not start, usually because the NDI runtime is not installed. \
                     Install it from ndi.video; sources set by address still play."
                        .into(),
                );
                drop(s);
                tracing::warn!("NDI discovery could not start; retrying in 30 s");
                thread::sleep(Duration::from_secs(30));
                continue;
            }
            tracing::info!("NDI discovery started");
            let bus = monitor.bus();
            loop {
                while bus.pop().is_some() {}
                let mut sources: Vec<NdiSource> = monitor.devices().iter().filter_map(device_source).collect();
                sources.sort();
                sources.dedup();
                let mut s = self.status.lock().unwrap();
                s.running = true;
                s.message = None;
                s.sources = sources;
                drop(s);
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn device_source(device: &gst::Device) -> Option<NdiSource> {
    let props = device.properties()?;
    let name = props.get::<String>("ndi-name").ok()?;
    let address = props.get::<String>("url-address").ok().filter(|a| !a.is_empty());
    Some(NdiSource { name, address })
}
