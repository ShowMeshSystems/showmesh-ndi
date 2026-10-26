pub mod api;
pub mod config;
pub mod discovery;
pub mod display;
pub mod manager;
pub mod receiver;

/// Initializes GStreamer and registers the NDI elements linked into this binary.
pub fn init_gstreamer() -> anyhow::Result<()> {
    gst::init()?;
    gstndi::plugin_register_static()?;
    Ok(())
}
