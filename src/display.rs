use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use drm::buffer::DrmFourcc;
use drm::control::{
    Device as ControlDevice, Mode, ModeTypeFlags, connector, crtc, dumbbuffer::DumbBuffer, framebuffer,
};
use serde::Serialize;

use crate::config::{DisplayConfig, ModeSpec, SinkKind};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OutputInfo {
    pub name: String,
    pub connected: bool,
    pub modes: Vec<ModeInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physical_size_mm: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModeInfo {
    pub name: String,
    pub width: u16,
    pub height: u16,
    pub refresh: u32,
    pub preferred: bool,
}

impl ModeInfo {
    fn new(width: u16, height: u16, refresh: u32, preferred: bool) -> Self {
        Self { name: format!("{width}x{height}@{refresh}"), width, height, refresh, preferred }
    }
}

/// What a receiver pipeline needs to render onto a prepared output.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub mode: ModeInfo,
    pub kms: Option<KmsTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KmsTarget {
    pub fd: RawFd,
    pub connector_id: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct KmsOptions {
    pub wait_vsync: bool,
}

#[derive(Debug)]
pub enum PrepareError {
    Disconnected,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for PrepareError {
    fn from(e: anyhow::Error) -> Self {
        PrepareError::Other(e)
    }
}

pub enum Display {
    Kms(Kms, KmsOptions),
    /// Wayland draws through the desktop; DRM is only read to list outputs.
    Wayland(Option<Kms>),
    Fake(u32),
}

impl Display {
    pub fn open(cfg: &DisplayConfig) -> Result<Self> {
        Ok(match cfg.sink {
            SinkKind::Kms => {
                Display::Kms(Kms::open(cfg.card.as_deref(), true)?, KmsOptions { wait_vsync: cfg.kms_wait_vsync })
            }
            SinkKind::Wayland => Display::Wayland(Kms::open(cfg.card.as_deref(), false).ok()),
            SinkKind::Fake => Display::Fake(cfg.fake_outputs.max(1)),
        })
    }

    pub fn sink(&self) -> SinkKind {
        match self {
            Display::Kms(..) => SinkKind::Kms,
            Display::Wayland(_) => SinkKind::Wayland,
            Display::Fake(_) => SinkKind::Fake,
        }
    }

    pub fn card(&self) -> Option<&Path> {
        match self {
            Display::Kms(k, _) | Display::Wayland(Some(k)) => Some(&k.path),
            _ => None,
        }
    }

    pub fn outputs(&self) -> Result<Vec<OutputInfo>> {
        match self {
            Display::Kms(k, _) | Display::Wayland(Some(k)) => k.outputs(),
            Display::Wayland(None) => Ok(vec![]),
            Display::Fake(n) => Ok((1..=*n).map(fake_output).collect()),
        }
    }

    pub fn is_connected(&self, name: &str) -> bool {
        match self {
            Display::Kms(k, _) => k.find(name).map(|c| c.state() == connector::State::Connected).unwrap_or(false),
            Display::Wayland(_) | Display::Fake(_) => true,
        }
    }

    /// Lights the output black at the chosen mode and returns where to render.
    pub fn prepare(&self, name: &str, mode: Option<ModeSpec>) -> Result<Target, PrepareError> {
        match self {
            Display::Kms(k, _) => k.prepare(name, mode),
            Display::Wayland(_) | Display::Fake(_) => {
                let m = mode.unwrap_or(ModeSpec { width: 1920, height: 1080, refresh: Some(60) });
                Ok(Target { mode: ModeInfo::new(m.width, m.height, m.refresh.unwrap_or(60), true), kms: None })
            }
        }
    }
}

fn fake_output(i: u32) -> OutputInfo {
    OutputInfo {
        name: format!("FAKE-{i}"),
        connected: true,
        modes: vec![
            ModeInfo::new(1920, 1080, 60, true),
            ModeInfo::new(1920, 1080, 30, false),
            ModeInfo::new(1280, 720, 60, false),
        ],
        physical_size_mm: None,
    }
}

struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for Card {}
impl ControlDevice for Card {}

/// One open DRM card shared by every output, so a single DRM master drives all of them.
pub struct Kms {
    card: Card,
    path: PathBuf,
    lit: Mutex<HashMap<String, Lit>>,
}

struct Lit {
    crtc: crtc::Handle,
    fb: framebuffer::Handle,
    buffer: Option<DumbBuffer>,
    mode: Mode,
}

impl Kms {
    fn open(card: Option<&Path>, master: bool) -> Result<Self> {
        let path = match card {
            Some(p) => p.to_path_buf(),
            None => find_card()?,
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let card = Card(file);
        if master && drm::Device::acquire_master_lock(&card).is_err() {
            bail!(
                "Another program, usually a desktop session, controls the display on {}. \
                 Stop the desktop or set display.sink to \"wayland\".",
                path.display()
            );
        }
        let kms = Self { card, path, lit: Mutex::new(HashMap::new()) };
        // Probe once so cached connector state is fresh; later reads rely on the kernel's hotplug updates.
        kms.read_connectors(true)?;
        Ok(kms)
    }

    fn connectors(&self) -> Result<Vec<connector::Info>> {
        self.read_connectors(false)
    }

    /// A forced probe re-reads EDID, which can blank some displays, so only `open` forces one.
    fn read_connectors(&self, force_probe: bool) -> Result<Vec<connector::Info>> {
        let res = self.card.resource_handles().context("reading display resources")?;
        res.connectors().iter().map(|h| Ok(self.card.get_connector(*h, force_probe)?)).collect()
    }

    fn find(&self, name: &str) -> Result<connector::Info> {
        self.connectors()?
            .into_iter()
            .find(|c| c.to_string() == name)
            .ok_or_else(|| anyhow!("Output {name} does not exist on this receiver."))
    }

    fn outputs(&self) -> Result<Vec<OutputInfo>> {
        Ok(self
            .connectors()?
            .iter()
            .map(|c| OutputInfo {
                name: c.to_string(),
                connected: c.state() == connector::State::Connected,
                modes: dedup_modes(c.modes()),
                physical_size_mm: c.size().filter(|s| s.0 > 0),
            })
            .collect())
    }

    fn prepare(&self, name: &str, spec: Option<ModeSpec>) -> Result<Target, PrepareError> {
        let conn = self.find(name)?;
        if conn.state() != connector::State::Connected || conn.modes().is_empty() {
            return Err(PrepareError::Disconnected);
        }
        let mode = pick_mode(conn.modes(), spec)?;
        let mut lit = self.lit.lock().unwrap();

        // Re-use the existing black frame when this output already shows it at this mode.
        if let Some(l) = lit.get(name)
            && l.mode == mode
            && self.card.get_crtc(l.crtc).ok().is_some_and(|i| i.framebuffer() == Some(l.fb) && i.mode() == Some(mode))
        {
            return Ok(self.target(&conn, mode));
        }

        let taken: Vec<crtc::Handle> = lit.iter().filter(|(n, _)| n.as_str() != name).map(|(_, l)| l.crtc).collect();
        let crtc =
            lit.get(name).map(|l| l.crtc).or_else(|| self.pick_crtc(&conn, &taken)).ok_or_else(|| {
                anyhow!("No display controller is free for output {name}. Use fewer outputs at once.")
            })?;

        let (w, h) = mode.size();
        let mut buffer = self
            .card
            .create_dumb_buffer((w.into(), h.into()), DrmFourcc::Xrgb8888, 32)
            .context("allocating a black frame")?;
        if let Ok(mut map) = self.card.map_dumb_buffer(&mut buffer) {
            map.as_mut().fill(0);
        }
        let fb = self.card.add_framebuffer(&buffer, 24, 32).context("registering a black frame")?;
        self.card
            .set_crtc(crtc, Some(fb), (0, 0), &[conn.handle()], Some(mode))
            .with_context(|| format!("setting mode {}x{} on {name}", w, h))?;
        if let Some(old) = lit.insert(name.to_string(), Lit { crtc, fb, buffer: Some(buffer), mode }) {
            self.free(old);
        }
        Ok(self.target(&conn, mode))
    }

    fn target(&self, conn: &connector::Info, mode: Mode) -> Target {
        let (w, h) = mode.size();
        Target {
            mode: ModeInfo::new(w, h, mode.vrefresh(), mode.mode_type().contains(ModeTypeFlags::PREFERRED)),
            kms: Some(KmsTarget { fd: self.card.0.as_raw_fd(), connector_id: conn.handle().into() }),
        }
    }

    fn pick_crtc(&self, conn: &connector::Info, taken: &[crtc::Handle]) -> Option<crtc::Handle> {
        let res = self.card.resource_handles().ok()?;
        let current = conn
            .current_encoder()
            .and_then(|e| self.card.get_encoder(e).ok())
            .and_then(|e| e.crtc())
            .filter(|c| !taken.contains(c));
        current.or_else(|| {
            conn.encoders()
                .iter()
                .filter_map(|e| self.card.get_encoder(*e).ok())
                .flat_map(|e| res.filter_crtcs(e.possible_crtcs()))
                .find(|c| !taken.contains(c))
        })
    }

    fn free(&self, mut l: Lit) {
        let _ = self.card.destroy_framebuffer(l.fb);
        if let Some(b) = l.buffer.take() {
            let _ = self.card.destroy_dumb_buffer(b);
        }
    }
}

impl Drop for Kms {
    fn drop(&mut self) {
        for (_, l) in self.lit.get_mut().unwrap().drain() {
            let _ = self.card.destroy_framebuffer(l.fb);
            if let Some(b) = l.buffer {
                let _ = self.card.destroy_dumb_buffer(b);
            }
        }
    }
}

fn find_card() -> Result<PathBuf> {
    let mut cards: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .context("reading /dev/dri. Is a GPU driver loaded?")?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("card")))
        .collect();
    cards.sort();
    for path in cards {
        let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else { continue };
        let card = Card(file);
        if card.resource_handles().is_ok_and(|r| !r.connectors().is_empty()) {
            return Ok(path);
        }
    }
    bail!("No display card with outputs was found in /dev/dri. Check the GPU driver.")
}

fn dedup_modes(modes: &[Mode]) -> Vec<ModeInfo> {
    let mut out: Vec<ModeInfo> = Vec::new();
    for m in modes {
        let (w, h) = m.size();
        let info = ModeInfo::new(w, h, m.vrefresh(), m.mode_type().contains(ModeTypeFlags::PREFERRED));
        match out.iter_mut().find(|o| o.name == info.name) {
            Some(existing) => existing.preferred |= info.preferred,
            None => out.push(info),
        }
    }
    out
}

fn pick_mode(modes: &[Mode], spec: Option<ModeSpec>) -> Result<Mode> {
    let preferred = || modes.iter().find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED)).or(modes.first());
    let Some(spec) = spec else {
        return preferred().copied().ok_or_else(|| anyhow!("The display reports no modes."));
    };
    modes
        .iter()
        .filter(|m| m.size() == (spec.width, spec.height))
        .filter(|m| spec.refresh.is_none_or(|r| m.vrefresh() == r))
        .max_by_key(|m| (m.mode_type().contains(ModeTypeFlags::PREFERRED), m.vrefresh()))
        .copied()
        .ok_or_else(|| {
            let hz = spec.refresh.map(|r| format!("@{r}")).unwrap_or_default();
            anyhow!(
                "The display does not support {}x{}{hz}. Pick a mode from the output's mode list.",
                spec.width,
                spec.height
            )
        })
}
