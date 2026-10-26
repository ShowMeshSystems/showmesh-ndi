use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub device: DeviceConfig,
    pub api: ApiConfig,
    pub display: DisplayConfig,
    pub ndi: NdiConfig,
    #[serde(rename = "output")]
    pub outputs: Vec<OutputConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DeviceConfig {
    pub name: String,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        let host = fs::read_to_string("/etc/hostname").unwrap_or_default();
        let host = host.trim();
        Self { name: if host.is_empty() { "showmesh-ndi".into() } else { host.into() } }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    pub listen: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self { listen: "0.0.0.0:8790".into() }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SinkKind {
    /// Direct DRM/KMS output with no desktop.
    #[default]
    Kms,
    /// Fullscreen window on a running Wayland desktop. One output only.
    Wayland,
    /// Discards frames. For tests and benches without a display.
    Fake,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayConfig {
    pub sink: SinkKind,
    /// DRM card to use. Empty picks the first card with display connectors.
    pub card: Option<PathBuf>,
    /// Number of simulated outputs when `sink = "fake"`.
    pub fake_outputs: u32,
    /// Make kmssink wait for vblank itself. Atomic drivers already wait in the plane update, so this halves the frame rate there.
    pub kms_wait_vsync: bool,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self { sink: SinkKind::Kms, card: None, fake_outputs: 1, kms_wait_vsync: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct NdiConfig {
    /// Milliseconds without a frame before the output goes black and reconnects.
    pub timeout_ms: u32,
}

impl Default for NdiConfig {
    fn default() -> Self {
        Self { timeout_ms: 5000 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    /// DRM connector name, such as `HDMI-A-1`.
    pub connector: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Display mode as `WIDTHxHEIGHT@HZ`. Empty uses the display's preferred mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceConfig>,
}

fn default_true() -> bool {
    true
}

impl OutputConfig {
    pub fn new(connector: &str) -> Self {
        Self { connector: connector.into(), enabled: true, mode: None, source: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceConfig {
    /// An NDI sender, by name, by address, or both.
    Ndi {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// `host:port` of the sender, for networks where discovery does not work.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<String>,
    },
    /// A generated pattern at the output's mode, for checking a display.
    TestPattern {
        #[serde(default = "default_pattern")]
        pattern: String,
    },
}

fn default_pattern() -> String {
    "smpte".into()
}

impl SourceConfig {
    pub fn validate(&self) -> Result<()> {
        match self {
            SourceConfig::Ndi { name, address } => {
                let blank = |v: &Option<String>| v.as_deref().is_none_or(|s| s.trim().is_empty());
                if blank(name) && blank(address) {
                    bail!("An NDI source needs a name or an address. Set one and try again.");
                }
            }
            SourceConfig::TestPattern { pattern } => {
                if !TEST_PATTERNS.contains(&pattern.as_str()) {
                    bail!("Test pattern {pattern:?} does not exist. Use one of: {}.", TEST_PATTERNS.join(", "));
                }
            }
        }
        Ok(())
    }
}

pub const TEST_PATTERNS: &[&str] =
    &["smpte", "smpte100", "bar", "checkers-1", "checkers-8", "black", "white", "red", "green", "blue", "ball"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ModeSpec {
    pub width: u16,
    pub height: u16,
    pub refresh: Option<u32>,
}

impl ModeSpec {
    /// Parses `1920x1080` or `1920x1080@60`.
    pub fn parse(s: &str) -> Result<Self> {
        let bad = || anyhow::anyhow!("Mode {s:?} is not valid. Write it as 1920x1080@60.");
        let (size, refresh) = match s.split_once('@') {
            Some((size, hz)) => (size, Some(hz.trim().parse::<f32>().map_err(|_| bad())?.round() as u32)),
            None => (s, None),
        };
        let (w, h) = size.trim().split_once(['x', 'X']).ok_or_else(bad)?;
        Ok(Self { width: w.trim().parse().map_err(|_| bad())?, height: h.trim().parse().map_err(|_| bad())?, refresh })
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Writes through a temporary file so a power cut never leaves a half-written config.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, toml::to_string_pretty(self)?).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        let playing = self.outputs.iter().filter(|o| o.enabled && o.source.is_some()).count();
        if self.display.sink == SinkKind::Wayland && playing > 1 {
            bail!("The wayland display mode plays one output only. Use the kms display mode for several outputs.");
        }
        let mut seen = std::collections::HashSet::new();
        for o in &self.outputs {
            if !seen.insert(o.connector.as_str()) {
                bail!("Output {} is listed twice in the configuration. Remove one entry.", o.connector);
            }
            if let Some(mode) = &o.mode {
                ModeSpec::parse(mode)?;
            }
            if let Some(source) = &o.source {
                source.validate()?;
            }
        }
        Ok(())
    }

    pub fn output(&self, connector: &str) -> Option<&OutputConfig> {
        self.outputs.iter().find(|o| o.connector == connector)
    }

    pub fn output_mut(&mut self, connector: &str) -> &mut OutputConfig {
        if let Some(i) = self.outputs.iter().position(|o| o.connector == connector) {
            return &mut self.outputs[i];
        }
        self.outputs.push(OutputConfig::new(connector));
        self.outputs.last_mut().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_config() {
        let text = include_str!("../packaging/config.example.toml");
        let config: Config = toml::from_str(text).unwrap();
        config.validate().unwrap();
        assert_eq!(config.outputs.len(), 2);
        assert_eq!(
            config.outputs[0].source,
            Some(SourceConfig::Ndi { name: Some("ARENA-MAC (Arena - Projector 1)".into()), address: None })
        );
    }

    #[test]
    fn round_trips_through_toml() {
        let mut config = Config::default();
        config.output_mut("HDMI-A-1").source = Some(SourceConfig::TestPattern { pattern: "smpte".into() });
        config.output_mut("HDMI-A-2").mode = Some("1280x720@60".into());
        let back: Config = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
        assert_eq!(back, config);
    }

    #[test]
    fn save_then_load() {
        let dir = std::env::temp_dir().join(format!("showmesh-ndi-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let mut config = Config::default();
        config.output_mut("DP-1").source =
            Some(SourceConfig::Ndi { name: None, address: Some("10.0.0.5:5961".into()) });
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(ModeSpec::parse("1920x1080@60").unwrap(), ModeSpec { width: 1920, height: 1080, refresh: Some(60) });
        assert_eq!(ModeSpec::parse("1280x720").unwrap(), ModeSpec { width: 1280, height: 720, refresh: None });
        assert_eq!(ModeSpec::parse("1920x1080@59.94").unwrap().refresh, Some(60));
        assert!(ModeSpec::parse("1080p").is_err());
        assert!(ModeSpec::parse("1920x@60").is_err());
    }

    #[test]
    fn rejects_empty_ndi_source_and_duplicates() {
        assert!(SourceConfig::Ndi { name: Some(" ".into()), address: None }.validate().is_err());
        assert!(SourceConfig::TestPattern { pattern: "nope".into() }.validate().is_err());
        let mut config = Config::default();
        config.outputs.push(OutputConfig::new("HDMI-A-1"));
        config.outputs.push(OutputConfig::new("HDMI-A-1"));
        assert!(config.validate().is_err());
    }
}
