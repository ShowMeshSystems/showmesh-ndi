//! Sends a moving test pattern as an NDI source, for exercising the receiver without Resolume.
//!
//! cargo run --example ndi_test_sender -- "Bench Sender" 1920x1080@60

use gst::prelude::*;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let name = args.next().unwrap_or_else(|| "showmesh-ndi test".into());
    let mode = showmesh_ndi::config::ModeSpec::parse(&args.next().unwrap_or_else(|| "1920x1080@60".into()))?;
    showmesh_ndi::init_gstreamer()?;
    let pipeline = gst::parse::launch(&format!(
        "videotestsrc is-live=true pattern=ball ! video/x-raw,format=UYVY,width={},height={},framerate={}/1 \
         ! ndisink ndi-name=\"{name}\"",
        mode.width,
        mode.height,
        mode.refresh.unwrap_or(60)
    ))?;
    pipeline.set_state(gst::State::Playing)?;
    println!("Sending NDI source \"{name}\". Press Ctrl+C to stop.");
    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        if let gst::MessageView::Error(e) = msg.view() {
            anyhow::bail!("{} ({:?})", e.error(), e.debug());
        }
    }
    Ok(())
}
