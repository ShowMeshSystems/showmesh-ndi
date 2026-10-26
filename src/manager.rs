use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;

use crate::config::{Config, OutputConfig, SinkKind};
use crate::discovery::{Discovery, DiscoveryStatus};
use crate::display::{Display, ModeInfo, OutputInfo};
use crate::receiver::{Receiver, ReceiverStatus};

#[derive(Debug, Clone, Serialize)]
pub struct OutputView {
    pub name: String,
    pub connected: bool,
    pub modes: Vec<ModeInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physical_size_mm: Option<(u32, u32)>,
    pub config: OutputConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiver: Option<ReceiverStatus>,
}

#[derive(Debug)]
pub enum ApiError {
    NotFound(String),
    Invalid(String),
    Internal(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::NotFound(m) | ApiError::Invalid(m) | ApiError::Internal(m) => f.write_str(m),
        }
    }
}

/// Owns the configuration, the display, and one receiver per output.
pub struct Manager {
    config_path: Option<PathBuf>,
    config: Mutex<Config>,
    display: Arc<Display>,
    receivers: Mutex<HashMap<String, Receiver>>,
    shutting_down: AtomicBool,
    discovery: Arc<Discovery>,
    started: Instant,
}

impl Manager {
    pub fn new(config: Config, config_path: Option<PathBuf>) -> Result<Arc<Self>> {
        let display = Arc::new(Display::open(&config.display)?);
        let sink = display.sink();
        if let Some(path) = display.card() {
            tracing::info!(card = %path.display(), ?sink, "display opened");
        }
        Ok(Arc::new(Self {
            config_path,
            config: Mutex::new(config),
            display,
            receivers: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            discovery: Discovery::start(),
            started: Instant::now(),
        }))
    }

    /// Starts receivers now and keeps them in step with hotplugged displays.
    pub fn start(self: &Arc<Self>) {
        self.sync_receivers();
        let me = Arc::downgrade(self);
        thread::Builder::new()
            .name("hotplug".into())
            .spawn(move || {
                while let Some(m) = me.upgrade() {
                    m.sync_receivers();
                    drop(m);
                    thread::sleep(Duration::from_secs(2));
                }
            })
            .expect("spawning hotplug thread");
    }

    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let receivers: Vec<Receiver> = self.receivers.lock().unwrap().drain().map(|(_, r)| r).collect();
        for r in receivers {
            r.shutdown();
        }
    }

    /// Every configured output gets a receiver. Under KMS, every connected output does too, so it shows black.
    fn sync_receivers(&self) {
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        let config = self.config.lock().unwrap().clone();
        let mut wanted: Vec<OutputConfig> = config.outputs.clone();
        if config.display.sink != SinkKind::Wayland
            && let Ok(outputs) = self.display.outputs()
        {
            for o in outputs.into_iter().filter(|o| o.connected) {
                if config.output(&o.name).is_none() {
                    wanted.push(OutputConfig::new(&o.name));
                }
            }
        }
        let mut receivers = self.receivers.lock().unwrap();
        for o in wanted {
            receivers
                .entry(o.connector.clone())
                .or_insert_with(|| Receiver::spawn(o, self.display.clone(), config.ndi.clone()));
        }
    }

    fn respawn(&self, connector: &str) {
        let old = self.receivers.lock().unwrap().remove(connector);
        if let Some(r) = old {
            r.shutdown();
        }
        self.sync_receivers();
    }

    pub fn config(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    pub fn sink(&self) -> SinkKind {
        self.display.sink()
    }

    pub fn card(&self) -> Option<String> {
        self.display.card().map(|p| p.display().to_string())
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn discovery(&self) -> DiscoveryStatus {
        self.discovery.status()
    }

    fn hardware_outputs(&self) -> Vec<OutputInfo> {
        self.display.outputs().unwrap_or_else(|e| {
            tracing::warn!("listing outputs failed: {e:#}");
            vec![]
        })
    }

    pub fn outputs(&self) -> Vec<OutputView> {
        let config = self.config();
        let receivers = self.receivers.lock().unwrap();
        let mut views: BTreeMap<String, OutputView> = BTreeMap::new();
        for hw in self.hardware_outputs() {
            let cfg = config.output(&hw.name).cloned().unwrap_or_else(|| OutputConfig::new(&hw.name));
            views.insert(
                hw.name.clone(),
                OutputView {
                    receiver: receivers.get(&hw.name).map(|r| r.status()),
                    name: hw.name,
                    connected: hw.connected,
                    modes: hw.modes,
                    physical_size_mm: hw.physical_size_mm,
                    config: cfg,
                },
            );
        }
        for cfg in config.outputs {
            views.entry(cfg.connector.clone()).or_insert_with(|| OutputView {
                name: cfg.connector.clone(),
                connected: false,
                modes: vec![],
                physical_size_mm: None,
                receiver: receivers.get(&cfg.connector).map(|r| r.status()),
                config: cfg,
            });
        }
        views.into_values().collect()
    }

    pub fn output(&self, name: &str) -> Result<OutputView, ApiError> {
        self.outputs().into_iter().find(|o| o.name == name).ok_or_else(|| not_found(name))
    }

    /// Applies a change to one output's configuration, saves it, and restarts that output's receiver.
    pub fn update_output(&self, name: &str, change: impl FnOnce(&mut OutputConfig)) -> Result<OutputView, ApiError> {
        let exists = self.hardware_outputs().iter().any(|o| o.name == name) || self.config().output(name).is_some();
        if !exists {
            return Err(not_found(name));
        }
        self.update_config(|c| change(c.output_mut(name)))?;
        self.respawn(name);
        self.output(name)
    }

    pub fn update_config(&self, change: impl FnOnce(&mut Config)) -> Result<(), ApiError> {
        let mut guard = self.config.lock().unwrap();
        let mut next = guard.clone();
        change(&mut next);
        next.validate().map_err(|e| ApiError::Invalid(e.to_string()))?;
        if let Some(path) = &self.config_path {
            next.save(path).map_err(|e| {
                ApiError::Internal(format!("The configuration could not be saved: {e:#}. Check the service's disk."))
            })?;
        }
        *guard = next;
        Ok(())
    }

    pub fn restart(&self, name: &str) -> Result<OutputView, ApiError> {
        if !self.receivers.lock().unwrap().contains_key(name) {
            return Err(not_found(name));
        }
        self.respawn(name);
        self.output(name)
    }
}

fn not_found(name: &str) -> ApiError {
    ApiError::NotFound(format!("Output {name} does not exist on this receiver. List outputs to see valid names."))
}
